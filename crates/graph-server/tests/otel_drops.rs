//! #250 (story 52, ADR 0009 D8): batch-queue overflow is counted. With a
//! tiny queue and a stalled collector, the spans and log records queued
//! minus those exported equal the rise in `mg_otel_dropped_total{signal}`,
//! and the collector received exactly the exported ones.
//!
//! !!! ONE TEST ONLY IN THIS FILE. !!! `mg_otel_dropped_total` is
//! process-wide, and the exact equality below holds only if nothing else in
//! this test binary exports. Put any other OpenTelemetry test in another
//! file (its own binary). The `ONLY_TEST` guard fails loudly if a second
//! test runs here.
use graph_server::telemetry::{
    self, otel_counters, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions,
};
use graph_server::testing::FakeCollector;
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::trace::{Tracer as _, TracerProvider as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const THREADS: u64 = 8;
const PER_THREAD: u64 = 500;
const ITEMS: u64 = THREADS * PER_THREAD;

static ONLY_TEST: AtomicUsize = AtomicUsize::new(0);

fn dropped(signal: &str) -> u64 {
    otel_counters()
        .iter()
        .find(|c| c.signal == signal)
        .expect("signal")
        .dropped
}

#[test]
fn queue_overflow_is_counted_as_dropped() {
    assert_eq!(
        ONLY_TEST.fetch_add(1, Ordering::SeqCst),
        0,
        "otel_drops.rs must hold one test only (see the module doc)"
    );
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
        // Longer than the stall: a held export succeeds once released, so
        // every drop here is a queue overflow and the collector's counts
        // equal `exported`.
        export_timeout: Some(Duration::from_secs(30)),
    };
    let base = (dropped("traces"), dropped("logs"));
    let guard = telemetry::init(&cfg).expect("init").expect("enabled");
    collector.stall();
    // A concurrent burst: the bound is enforced by a CAS under contention.
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let guard = &guard;
            s.spawn(move || {
                let tracer = guard.tracer_provider().expect("traces").tracer("drops");
                let logger = guard.logger_provider().expect("logs").logger("drops");
                for i in 0..PER_THREAD {
                    tracer.in_span(format!("span-{t}-{i}"), |_| {});
                    let mut record = logger.create_log_record();
                    record.set_body("drop test".into());
                    logger.emit(record);
                }
            });
        }
    });
    collector.release();
    // Flush until every admitted item has been handed to the exporter and
    // its export has ended (no fixed sleep: a probe with a deadline).
    let deadline = Instant::now() + Duration::from_secs(60);
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
    assert_eq!(traces.failures, 0, "{traces:?}");
    assert_eq!(logs.failures, 0, "{logs:?}");
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
    let received = collector.received();
    let spans: u64 = received
        .traces
        .iter()
        .flat_map(|r| &r.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .map(|s| s.spans.len() as u64)
        .sum();
    let records: u64 = received
        .logs
        .iter()
        .flat_map(|r| &r.resource_logs)
        .flat_map(|r| &r.scope_logs)
        .map(|s| s.log_records.len() as u64)
        .sum();
    assert_eq!(
        spans, traces.exported,
        "the collector got the exported spans"
    );
    assert_eq!(
        records, logs.exported,
        "the collector got the exported records"
    );
    guard.shutdown_blocking();
}
