//! Logs over OTLP with trace ids (ADR 0009 D7, D9; epic story 53), in
//! process: `serve`'s own subscriber (`graph_cli::logging::init_to`, JSON,
//! `--log-level info`) with the traces and logs layers, exporting to a
//! `FakeCollector`. One process, one global subscriber, so one test.
//! Without traces: `otel_logs_no_traces.rs`; failures and shutdown:
//! `otel_logs_failures.rs`.
use graph_cli::logging::{self, LogFormat, OtelLayers};
use graph_server::telemetry::{
    self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions, ALLOWED_LOG_FIELDS,
};
use graph_server::testing::{CollectedLog, FakeCollector, TestServer};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(60);
const SENTINEL: &str = "Zq7SentinelLogQuery";

/// The JSON lines the fmt layer writes (stderr in `serve`).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect()
    }
}

fn hex_of(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && s.bytes().any(|b| b != b'0')
}

fn record<'a>(logs: &'a [CollectedLog], body: &str) -> &'a CollectedLog {
    let found: Vec<_> = logs.iter().filter(|r| r.body == body).collect();
    assert_eq!(found.len(), 1, "one record `{body}` in {logs:#?}");
    found[0]
}

fn line<'a>(lines: &'a [serde_json::Value], message: &str) -> &'a serde_json::Value {
    lines
        .iter()
        .find(|l| l["message"] == message)
        .unwrap_or_else(|| panic!("no line `{message}`"))
}

#[test]
fn records_carry_mapped_severity_allowed_fields_and_the_spans_ids() {
    let collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("traces,logs".into()),
        ..Default::default()
    };
    let mut cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(50)),
        max_queue_size: Some(1 << 16),
        export_timeout: Some(Duration::from_secs(5)),
    };
    let guard = telemetry::init(&cfg).unwrap().expect("on");
    let out = Captured::default();
    let writer = out.clone();
    logging::init_to(
        LogFormat::Json,
        "info",
        OtelLayers {
            traces: telemetry::tracing_layer(&guard),
            logs: telemetry::log_layer(&guard),
        },
        move || writer.clone(),
    )
    .unwrap();

    // Outside any span.
    tracing::info!(target: "graph_server::t53", count = 3u64, query = SENTINEL, "outside");
    // Inside an exported span (a workspace target at info).
    let span = tracing::info_span!(target: "graph_server::t53", "rpc", method = "Store/Search");
    let (trace, span_id) = {
        use opentelemetry::trace::TraceContextExt;
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let sc = span.context().span().span_context().clone();
        (sc.trace_id().to_string(), sc.span_id().to_string())
    };
    span.in_scope(|| {
        tracing::warn!(
            target: "graph_server::t53",
            method = "Store/Search",
            duration_ms = 7u64,
            query = SENTINEL,
            error = SENTINEL,
            "inside warn"
        );
        tracing::error!(target: "graph_server::t53", "inside error");
        // Below --log-level: neither stderr nor OTLP.
        tracing::debug!(target: "graph_server::t53", "inside debug");
        // The OpenTelemetry crates' own events never reach the collector.
        tracing::warn!(target: "opentelemetry_sdk", "otel own");
    });
    // Closed: exported.
    drop(span);
    // A served request with the sentinel as its query.
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start(&d.path().join("g.redb"), graph_cli::shipped_extractors());
    {
        use graph_store::StoreRead;
        let c = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(srv.endpoint()))
            .unwrap();
        let _ = c.search(&graph_store::Query::new(SENTINEL));
    }
    tracing::info!(target: "graph_server::t53", "last");

    let until = Instant::now() + WAIT;
    let logs = loop {
        guard.logger_provider().unwrap().force_flush().unwrap();
        let logs = collector.received().log_records();
        if logs.iter().any(|r| r.body == "last") {
            break logs;
        }
        assert!(Instant::now() < until, "records never arrived");
        std::thread::sleep(Duration::from_millis(20));
    };
    guard.tracer_provider().unwrap().force_flush().unwrap();
    let spans = collector.wait_spans(WAIT, |all| all.iter().any(|s| s.trace_id == trace));
    let lines = out.lines();

    // Severity, message, allow-listed fields only.
    let w = record(&logs, "inside warn");
    assert_eq!((w.severity_number, w.severity_text.as_str()), (13, "WARN"));
    assert_eq!(
        w.attributes.keys().map(String::as_str).collect::<Vec<_>>(),
        ["duration_ms", "method"]
    );
    let e = record(&logs, "inside error");
    assert_eq!((e.severity_number, e.severity_text.as_str()), (17, "ERROR"));
    let o = record(&logs, "outside");
    assert_eq!((o.severity_number, o.severity_text.as_str()), (9, "INFO"));
    assert_eq!(o.attributes.keys().collect::<Vec<_>>(), ["count"]);
    for r in &logs {
        assert!(
            r.attributes
                .keys()
                .all(|k| ALLOWED_LOG_FIELDS.contains(&k.as_str())),
            "{r:?}"
        );
        assert!(!r.all_text.iter().any(|t| t.contains(SENTINEL)), "{r:?}");
        assert!(!r.scope.starts_with("opentelemetry"), "{r:?}");
        assert!(r.body != "inside debug");
        // Never zero ids: present and valid, or absent.
        assert!(r.trace_id.is_empty() || hex_of(&r.trace_id, 32), "{r:?}");
        assert!(r.span_id.is_empty() || hex_of(&r.span_id, 16), "{r:?}");
    }

    // The ids: the span's, in the record and in the JSON line.
    let rpc = spans
        .iter()
        .find(|s| s.trace_id == trace)
        .expect("the span was exported");
    assert_eq!(rpc.span_id, span_id);
    assert!(hex_of(&trace, 32) && hex_of(&span_id, 16));
    for msg in ["inside warn", "inside error"] {
        let r = record(&logs, msg);
        assert_eq!(
            (r.trace_id.as_str(), r.span_id.as_str()),
            (trace.as_str(), span_id.as_str())
        );
        let l = line(&lines, msg);
        assert_eq!(l["trace_id"], trace.as_str(), "{l}");
        assert_eq!(l["span_id"], span_id.as_str(), "{l}");
        // Stripped fields stay in local logs.
        if msg == "inside warn" {
            assert_eq!(l["query"], SENTINEL);
        }
    }
    // Outside a span: no ids anywhere.
    assert!(o.trace_id.is_empty() && o.span_id.is_empty(), "{o:?}");
    for msg in ["outside", "last"] {
        let l = line(&lines, msg);
        assert!(
            l.get("trace_id").is_none() && l.get("span_id").is_none(),
            "{l}"
        );
    }
    // Same events on stderr as over OTLP (the --log-level filter).
    assert!(lines.iter().all(|l| l["message"] != "inside debug"));
    let ours = |m: &str| ["outside", "inside warn", "inside error", "last"].contains(&m);
    let stderr: Vec<&str> = lines
        .iter()
        .filter_map(|l| l["message"].as_str())
        .filter(|m| ours(m))
        .collect();
    let otlp: Vec<&str> = logs
        .iter()
        .map(|r| r.body.as_str())
        .filter(|m| ours(m))
        .collect();
    assert_eq!(stderr, otlp);
    // Every JSON line inside a span with ids has well-formed ones.
    for l in &lines {
        if let Some(t) = l.get("trace_id") {
            assert!(hex_of(t.as_str().unwrap(), 32), "{l}");
            assert!(hex_of(l["span_id"].as_str().unwrap(), 16), "{l}");
        }
    }
    drop(srv);
    guard.shutdown_blocking();
}
