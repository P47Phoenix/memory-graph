//! ADR 0009 D5: `OTEL_TRACES_SAMPLER=always_off` exports no span, and
//! serving (a write included) is unaffected. A test binary of its own: the
//! SDK reads the variable when the provider is built, process-wide.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::telemetry::{self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions};
use graph_server::testing::{FakeCollector, TestServer};
use graph_store::{Query, StoreRead};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[test]
fn always_off_exports_nothing_and_writes_succeed() {
    // The only test in this binary, before any provider exists.
    std::env::set_var("OTEL_TRACES_SAMPLER", "always_off");
    let collector = FakeCollector::start();
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("traces".into()),
        ..Default::default()
    };
    let mut cfg =
        TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None).unwrap();
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(20)),
        ..Default::default()
    };
    let guard = telemetry::init(&cfg).unwrap().expect("traces on");
    tracing_subscriber::registry()
        .with(telemetry::tracing_layer(&guard).unwrap())
        .init();
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start(&d.path().join("g.redb"), exts());
    let c = RemoteStore::connect(ClientConfig::new(srv.endpoint())).unwrap();
    index_files(&c, "o", "r", &[small_file(0)]);
    assert!(!c.search(&Query::new("fn")).unwrap().is_empty());
    guard.tracer_provider().unwrap().force_flush().unwrap();
    drop(c);
    drop(srv);
    guard.shutdown_blocking();
    assert!(
        collector.received().spans().is_empty(),
        "no span is sampled: {:?}",
        collector.received().spans()
    );
}
