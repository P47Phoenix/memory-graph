//! Logs over OTLP when the collector fails (ADR 0009 D7, D8; epic story
//! 53): a refusing collector's errors never come back as records once it
//! recovers; a stalled one never blocks logging, and the failed exports
//! and lost records are counted (`mg_otel_export_failures_total` and
//! `mg_otel_dropped_total` with `signal="logs"`); a shutdown flushes what
//! is queued to a responsive collector and is bounded with a stalled one.
//! One process, one global subscriber, and the steps share the collector,
//! so one test.
use graph_cli::logging::{self, LogFormat, OtelLayers};
use graph_server::telemetry::{
    self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryGuard, TelemetryOptions,
    SHUTDOWN_TIMEOUT,
};
use graph_server::testing::{FakeCollector, FakeSignal};
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(60);

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let until = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn guard(collector: &FakeCollector, delay: Duration, queue: usize) -> TelemetryGuard {
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("logs".into()),
        ..Default::default()
    };
    let mut cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    cfg.batch = BatchTuning {
        schedule_delay: Some(delay),
        max_queue_size: Some(queue),
        export_timeout: Some(Duration::from_millis(300)),
    };
    telemetry::init(&cfg).unwrap().expect("on")
}

fn bodies(collector: &FakeCollector) -> Vec<String> {
    collector
        .received()
        .log_records()
        .into_iter()
        .map(|r| r.body)
        .collect()
}

/// Log `n` events; none may take long (logging never blocks).
fn log_burst(tag: &str, n: usize) {
    for i in 0..n {
        let t = Instant::now();
        tracing::info!(target: "graph_server::t53", count = i as u64, "{tag}");
        assert!(t.elapsed() < Duration::from_secs(1), "logging blocked");
    }
}

#[test]
fn failures_are_counted_never_block_and_never_export_themselves() {
    let mut collector = FakeCollector::start();
    let g = guard(&collector, Duration::from_millis(20), 64);
    logging::init_to(
        LogFormat::Text,
        "debug",
        OtelLayers {
            traces: None,
            logs: telemetry::log_layer(&g),
        },
        std::io::sink,
    )
    .unwrap();

    // Refused: export errors happen and are counted.
    let failures = telemetry::export_failures("logs");
    collector.refuse();
    eventually("a refused export", || {
        log_burst("refused", 5);
        telemetry::export_failures("logs") > failures
    });
    // Recovered: later records arrive, none from the OpenTelemetry crates.
    collector.accept();
    eventually("records after recovery", || {
        log_burst("recovered", 1);
        bodies(&collector).iter().any(|b| b == "recovered")
    });
    let records = collector.received().log_records();
    assert!(
        records
            .iter()
            .all(|r| !r.scope.starts_with("opentelemetry")),
        "{records:#?}"
    );

    // Stalled: logging goes on at full speed; exports time out and are
    // counted with what they carried, and a full queue drops.
    let (failures, dropped) = (
        telemetry::export_failures("logs"),
        telemetry::dropped("logs"),
    );
    collector.stall();
    let t = Instant::now();
    log_burst("stalled", 2_000);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    eventually("counted failures and drops", || {
        telemetry::export_failures("logs") > failures && telemetry::dropped("logs") > dropped
    });
    // Stopped: the same.
    collector.release();
    let failures = telemetry::export_failures("logs");
    collector.stop();
    eventually("a failure with the collector stopped", || {
        log_burst("stopped", 5);
        telemetry::export_failures("logs") > failures
    });
    collector.restart();
    collector.accept();
    eventually("records after a restart", || {
        log_burst("restarted", 1);
        bodies(&collector).iter().any(|b| b == "restarted")
    });
    g.shutdown_blocking();

    // Shutdown flushes what is queued to a responsive collector (a long
    // schedule delay: nothing would leave before it).
    let g = guard(&collector, Duration::from_secs(3_600), 1024);
    let logger = g.logger_provider().unwrap().logger("graph_server::t53");
    for _ in 0..10 {
        let mut r = logger.create_log_record();
        r.set_body("queued".into());
        logger.emit(r);
    }
    g.shutdown_blocking();
    assert_eq!(
        bodies(&collector).iter().filter(|b| *b == "queued").count(),
        10
    );

    // A stalled collector: the shutdown is bounded.
    let g = guard(&collector, Duration::from_millis(20), 1024);
    collector.stall();
    let logger = g.logger_provider().unwrap().logger("graph_server::t53");
    let mut r = logger.create_log_record();
    r.set_body("held".into());
    logger.emit(r);
    collector.wait_for(FakeSignal::Stalled, 1, WAIT);
    let t = Instant::now();
    g.shutdown_blocking();
    assert!(
        t.elapsed() <= SHUTDOWN_TIMEOUT * 2 + Duration::from_secs(1),
        "{:?}",
        t.elapsed()
    );
    collector.release();
}
