//! ADR 0009 D8 for traces (epic story 51): a stalled or stopped collector
//! never fails or blocks an RPC, the failed exports are counted
//! (`telemetry::export_failures` / `dropped`, the values behind
//! `mg_otel_export_failures_total` / `mg_otel_dropped_total`), and export
//! resumes when the collector comes back, with no restart. One test: the
//! process has one global subscriber and one collector, and the steps
//! depend on each other's state.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_core::NodeKind;
use graph_server::telemetry::{self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions};
use graph_server::testing::{FakeCollector, FakeSignal, TestServer};
use graph_store::StoreRead;
use std::time::{Duration, Instant};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const WAIT: Duration = Duration::from_secs(60);
const RPCS: usize = 20;

/// Poll `cond` until it holds or [`WAIT`] passes.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let until = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn rpcs(c: &RemoteStore) {
    for _ in 0..RPCS {
        c.count_nodes(NodeKind::File)
            .expect("serving is unaffected");
    }
}

#[test]
fn a_stalled_or_stopped_collector_is_counted_and_export_resumes() {
    let mut collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("traces".into()),
        ..Default::default()
    };
    let mut cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(20)),
        max_queue_size: Some(4096),
        export_timeout: Some(Duration::from_millis(300)),
    };
    let guard = telemetry::init(&cfg).unwrap().expect("traces on");
    tracing_subscriber::registry()
        .with(telemetry::tracing_layer(&guard).unwrap())
        .init();
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start(&d.path().join("g.redb"), exts());
    let c = RemoteStore::connect(ClientConfig::new(srv.endpoint())).unwrap();
    rpcs(&c);
    assert!(
        collector.wait_for(FakeSignal::Traces, 1, WAIT),
        "export works"
    );

    // Stalled: every RPC still succeeds; exports time out and are counted.
    let (f0, d0) = (
        telemetry::export_failures("traces"),
        telemetry::dropped("traces"),
    );
    collector.stall();
    rpcs(&c);
    eventually("a failed trace export", || {
        telemetry::export_failures("traces") > f0 && telemetry::dropped("traces") > d0
    });
    collector.release();

    // Stopped: the same, through a refused connection.
    let (f1, d1) = (
        telemetry::export_failures("traces"),
        telemetry::dropped("traces"),
    );
    collector.stop();
    rpcs(&c);
    eventually("a failed export to a stopped collector", || {
        telemetry::export_failures("traces") > f1 && telemetry::dropped("traces") > d1
    });

    // Back: new spans arrive again, with no restart of anything.
    collector.restart();
    let before = collector.received().spans().len();
    eventually("spans exported after the collector came back", || {
        c.count_nodes(NodeKind::File).unwrap();
        collector.received().spans().len() > before
    });
    assert!(collector
        .received()
        .spans()
        .iter()
        .any(|s| s.name == "rpc" && s.attr("rpc.method") == Some("CountNodes")));
    assert_eq!(telemetry::export_failures("metrics"), 0);
    drop(c);
    drop(srv);
    guard.shutdown_blocking();
}
