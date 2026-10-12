//! Logs over OTLP without traces (ADR 0009 D7, epic story 53): with
//! `--otlp-signals logs` the JSON lines are what they were before
//! OpenTelemetry (no id fields, inside a span or not), and the records
//! carry no ids. One process, one global subscriber, so one test.
use graph_cli::logging::{self, LogFormat, OtelLayers};
use graph_server::telemetry::{self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions};
use graph_server::testing::FakeCollector;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

#[test]
fn json_lines_are_unchanged_and_records_have_no_ids() {
    let collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("logs".into()),
        ..Default::default()
    };
    let mut cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(50)),
        max_queue_size: Some(1024),
        export_timeout: Some(Duration::from_secs(5)),
    };
    let guard = telemetry::init(&cfg).unwrap().expect("on");
    assert!(telemetry::tracing_layer::<tracing_subscriber::Registry>(&guard).is_none());
    let out = Captured::default();
    let writer = out.clone();
    logging::init_to(
        LogFormat::Json,
        "info",
        OtelLayers {
            traces: None,
            logs: telemetry::log_layer(&guard),
        },
        move || writer.clone(),
    )
    .unwrap();
    let span = tracing::info_span!(target: "graph_server::t53", "rpc", method = "Store/Hello");
    span.in_scope(|| tracing::info!(target: "graph_server::t53", count = 1u64, "inside"));
    let until = Instant::now() + Duration::from_secs(60);
    let logs = loop {
        guard.logger_provider().unwrap().force_flush().unwrap();
        let logs = collector.received().log_records();
        if logs.iter().any(|r| r.body == "inside") {
            break logs;
        }
        assert!(Instant::now() < until, "the record never arrived");
        std::thread::sleep(Duration::from_millis(20));
    };
    let r = logs.iter().find(|r| r.body == "inside").unwrap();
    assert!(r.trace_id.is_empty() && r.span_id.is_empty(), "{r:?}");
    assert_eq!(
        collector
            .received()
            .count(graph_server::testing::FakeSignal::Traces),
        0
    );

    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let l = text.lines().find(|l| l.contains("\"inside\"")).unwrap();
    let v: serde_json::Value = serde_json::from_str(l).unwrap();
    // The pre-OpenTelemetry shape: these keys and no others.
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["count", "level", "message", "span", "target", "timestamp"],
        "{l}"
    );
    assert_eq!(v["span"]["name"], "rpc");
    guard.shutdown_blocking();
}
