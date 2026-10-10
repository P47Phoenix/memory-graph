//! #250 (story 52, ADR 0009 D8): batch-queue overflow is counted. With a
//! tiny queue and a stalled collector, the spans and log records queued
//! minus those exported equal the rise in `mg_otel_dropped_total{signal}`.
//!
//! One test in its own binary: `mg_otel_dropped_total` is process-wide, so
//! nothing else in this process may move it.
use graph_server::telemetry::{
    self, otel_counters, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions,
};
use graph_server::testing::FakeCollector;
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::trace::{Tracer as _, TracerProvider as _};
use std::time::{Duration, Instant};

const ITEMS: u64 = 200;

fn dropped(signal: &str) -> u64 {
    otel_counters()
        .iter()
        .find(|c| c.signal == signal)
        .expect("signal")
        .dropped
}

#[test]
fn queue_overflow_is_counted_as_dropped() {
    let collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("traces,logs".into()),
        ..Default::default()
    };
    let mut cfg = TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None)
        .expect("config");
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(20)),
        max_queue_size: Some(4),
        export_timeout: Some(Duration::from_millis(300)),
    };
    let base = (dropped("traces"), dropped("logs"));
    let guard = telemetry::init(&cfg).expect("init").expect("enabled");
    collector.stall();
    let tracer = guard.tracer_provider().expect("traces").tracer("drops");
    let logger = guard.logger_provider().expect("logs").logger("drops");
    for i in 0..ITEMS {
        tracer.in_span(format!("span-{i}"), |_| {});
        let mut record = logger.create_log_record();
        record.set_body("drop test".into());
        logger.emit(record);
    }
    collector.release();
    // Flush until every admitted item has been handed to the exporter and
    // its export has ended (no fixed sleep: a probe with a deadline).
    let deadline = Instant::now() + Duration::from_secs(30);
    for signal in ["traces", "logs"] {
        loop {
            match signal {
                "traces" => {
                    let _ = guard.tracer_provider().unwrap().force_flush();
                }
                _ => {
                    let _ = guard.logger_provider().unwrap().force_flush();
                }
            }
            let c = guard.pipeline_counts(signal).unwrap();
            if c.pending == 0 && c.queued == c.exported + c.dropped {
                break;
            }
            assert!(Instant::now() < deadline, "{signal} never settled: {c:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let traces = guard.pipeline_counts("traces").unwrap();
    let logs = guard.pipeline_counts("logs").unwrap();
    assert_eq!(traces.queued, ITEMS);
    assert_eq!(logs.queued, ITEMS);
    assert!(traces.dropped > 0, "a 4-item queue overflowed: {traces:?}");
    assert!(logs.dropped > 0, "a 4-item queue overflowed: {logs:?}");
    assert_eq!(
        traces.queued - traces.exported,
        dropped("traces") - base.0,
        "{traces:?}"
    );
    assert_eq!(
        logs.queued - logs.exported,
        dropped("logs") - base.1,
        "{logs:?}"
    );
    guard.shutdown_blocking();
}
