//! #309, #310 on the fmt side: with traces on, the fmt layer's per-layer
//! `--log-level` filter caches `always` for what it logs, but the trace
//! layer's `AskEachTime` makes every combined interest `sometimes` (or
//! `never`), so each event runs `enabled` and a leftover `enabled` call
//! (a `tracing::enabled!` hint) cannot hide the next line. One process,
//! one global subscriber, so one test.
use graph_cli::logging::{self, LogFormat, OtelLayers};
use graph_server::telemetry::{self, NodeIdentity, TelemetryConfig, TelemetryOptions};
use graph_server::testing::FakeCollector;
use std::io::Write;
use std::sync::{Arc, Mutex};

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
fn a_leftover_enabled_call_does_not_hide_the_next_log_line() {
    // A callsite both filters reject (below `info`, not exported).
    let hint = tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        tracing::debug_span!(target: "openraft::raft", "hint")
            .metadata()
            .expect("a plain registry enables everything")
    });
    let collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("traces".into()),
        ..Default::default()
    };
    let cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    let guard = telemetry::init(&cfg).unwrap().expect("on");
    let out = Captured::default();
    let writer = out.clone();
    logging::init_to(
        LogFormat::Text,
        "info",
        OtelLayers {
            traces: telemetry::tracing_layer(&guard),
            logs: None,
        },
        move || writer.clone(),
    )
    .unwrap();
    for i in 0..3 {
        tracing::dispatcher::get_default(|d| d.enabled(hint));
        tracing::info!(target: "graph_cli::test", "line {i}");
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains(&format!("line {i}")),
            "line {i} missing: {text}"
        );
    }
}
