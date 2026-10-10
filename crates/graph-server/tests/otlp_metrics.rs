//! Story 52 (ADR 0009 D6, D8): every `/metrics` family over OTLP, checked
//! against the in-process fake collector. No fixed sleeps: every wait is
//! on collector counts or a probe, with a deadline.
mod support;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::observe::{MetricsSnapshot, SampleValue, DURATION_BUCKETS};
use graph_server::telemetry::{
    self, BatchTuning, NodeIdentity, OtelInstrument, TelemetryConfig, TelemetryGuard,
    TelemetryOptions, METRIC_MAPPING,
};
use graph_server::testing::{FakeCollector, FakeSignal, TestServer};
use opentelemetry_proto::tonic::common::v1::{any_value, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric::Data, number_data_point, Metric};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use support::*;

const WAIT: Duration = Duration::from_secs(30);

fn guard(collector: &FakeCollector, interval: Duration) -> TelemetryGuard {
    let options = TelemetryOptions {
        endpoint: Some(collector.endpoint()),
        signals: Some("metrics".into()),
        metrics_interval: Some(interval),
        ..Default::default()
    };
    let mut cfg = TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None)
        .expect("config");
    cfg.batch = BatchTuning {
        schedule_delay: Some(Duration::from_millis(50)),
        max_queue_size: Some(64),
        export_timeout: Some(Duration::from_millis(500)),
    };
    telemetry::init(&cfg).expect("init").expect("enabled")
}

fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !probe() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The metrics of the last export the collector received, by name.
fn last_export(collector: &FakeCollector) -> BTreeMap<String, Metric> {
    let received = collector.received();
    let last = received.metrics.last().expect("a metrics export");
    last.resource_metrics
        .iter()
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .map(|m| (m.name.clone(), m.clone()))
        .collect()
}

fn attrs(kvs: &[KeyValue]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = kvs
        .iter()
        .map(|kv| {
            let value = match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(any_value::Value::StringValue(s)) => s.clone(),
                Some(any_value::Value::IntValue(i)) => i.to_string(),
                other => format!("{other:?}"),
            };
            (kv.key.clone(), value)
        })
        .collect();
    v.sort();
    v
}

/// The number points of a gauge or sum: (attributes, value as f64).
fn number_points(m: &Metric) -> Vec<(Vec<(String, String)>, f64)> {
    let points = match m.data.as_ref().expect("data") {
        Data::Gauge(g) => &g.data_points,
        Data::Sum(s) => &s.data_points,
        other => panic!("{} is not a number metric: {other:?}", m.name),
    };
    let mut v: Vec<_> = points
        .iter()
        .map(|p| {
            let value = match p.value.expect("value") {
                number_data_point::Value::AsInt(i) => i as f64,
                number_data_point::Value::AsDouble(d) => d,
            };
            (attrs(&p.attributes), value)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// A family's samples as (labels, value), comparable with `number_points`.
fn samples(snapshot: &MetricsSnapshot, name: &str) -> Vec<(Vec<(String, String)>, f64)> {
    let family = snapshot.family(name).expect(name);
    let mut v: Vec<_> = family
        .samples
        .iter()
        .map(|s| {
            let mut labels: Vec<(String, String)> = s
                .labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            labels.sort();
            let value = match &s.value {
                SampleValue::Int(i) => *i as f64,
                SampleValue::Float(f) => *f,
                SampleValue::Histogram(h) => h.count() as f64,
            };
            (labels, value)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The kind check: instrument and unit as the mapping says.
fn assert_kind(m: &Metric, instrument: OtelInstrument, unit: &str) {
    assert_eq!(m.unit, unit, "{}", m.name);
    let data = m.data.as_ref().expect("data");
    match (instrument, data) {
        (OtelInstrument::Gauge, Data::Gauge(_)) => {}
        (OtelInstrument::Counter, Data::Sum(s)) => assert!(s.is_monotonic, "{}", m.name),
        (OtelInstrument::UpDownCounter, Data::Sum(s)) => {
            assert!(!s.is_monotonic, "{}", m.name)
        }
        (OtelInstrument::Histogram, Data::Histogram(h)) => {
            for p in &h.data_points {
                assert_eq!(p.explicit_bounds, DURATION_BUCKETS.to_vec(), "{}", m.name);
            }
        }
        (want, got) => panic!("{}: want {want:?}, got {got:?}", m.name),
    }
    // Cumulative, as Prometheus counters are.
    if let Data::Sum(s) = data {
        assert_eq!(s.aggregation_temporality, 2, "{} is cumulative", m.name);
    }
}

fn histogram_count(m: &Metric) -> u64 {
    match m.data.as_ref() {
        Some(Data::Histogram(h)) => h.data_points.iter().map(|p| p.count).sum(),
        other => panic!("{} is not a histogram: {other:?}", m.name),
    }
}

fn no_overflow(collector: &FakeCollector) {
    for request in collector.received().metrics {
        for m in request
            .resource_metrics
            .iter()
            .flat_map(|r| &r.scope_metrics)
            .flat_map(|s| &s.metrics)
        {
            let keys: Vec<&str> = match m.data.as_ref() {
                Some(Data::Gauge(g)) => g
                    .data_points
                    .iter()
                    .flat_map(|p| &p.attributes)
                    .map(|a| a.key.as_str())
                    .collect(),
                Some(Data::Sum(s)) => s
                    .data_points
                    .iter()
                    .flat_map(|p| &p.attributes)
                    .map(|a| a.key.as_str())
                    .collect(),
                Some(Data::Histogram(h)) => h
                    .data_points
                    .iter()
                    .flat_map(|p| &p.attributes)
                    .map(|a| a.key.as_str())
                    .collect(),
                _ => Vec::new(),
            };
            assert!(
                !keys.contains(&"otel.metric.overflow"),
                "{} reached the overflow stream",
                m.name
            );
        }
    }
}

fn sample_sum(text: &str, name: &str) -> f64 {
    text.lines()
        .filter(|l| l.starts_with(name) && !l.starts_with(&format!("{name}_")))
        .filter(|l| l[name.len()..].starts_with(['{', ' ']))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// A frozen snapshot converts exactly: every family's kind, unit,
/// attributes and values.
#[test]
fn a_frozen_snapshot_converts_exactly() {
    let _w = watchdog("a_frozen_snapshot_converts_exactly", TEST_LIMIT);
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(&dir.path().join("frozen.redb"), exts());
    let frozen = server.running().unwrap().metrics_snapshot();
    let collector = FakeCollector::start();
    // A long interval: only the explicit collection below exports.
    let guard = guard(&collector, Duration::from_secs(3600));
    let otlp = guard.metrics().expect("metrics on");
    let source = frozen.clone();
    let _recorders = otlp.attach(Box::new(move || Some(source.clone())));
    otlp.collect_now().expect("export");
    assert!(collector.wait_for(FakeSignal::Metrics, 1, WAIT));
    let got = last_export(&collector);
    for m in METRIC_MAPPING {
        let Some(otel) = m.otel else { continue };
        if m.instrument == OtelInstrument::Histogram {
            continue; // synchronous: nothing recorded here
        }
        // Only seconds are float instruments: a float sample anywhere else
        // would be truncated to an integer.
        let family = frozen.family(m.prometheus).expect(m.prometheus);
        if family
            .samples
            .iter()
            .any(|s| matches!(s.value, SampleValue::Float(_)))
        {
            assert_eq!(m.unit, "s", "{} has float samples", m.prometheus);
        }
        let want = samples(&frozen, m.prometheus);
        if want.is_empty() {
            // A labelled family with nothing observed: no points either.
            assert!(got.get(otel).is_none_or(|x| number_points(x).is_empty()));
            continue;
        }
        let metric = got
            .get(otel)
            .unwrap_or_else(|| panic!("{otel} missing: {:?}", got.keys()));
        assert_kind(metric, m.instrument, m.unit);
        let points = number_points(metric);
        assert_eq!(points, want, "{otel} (from {})", m.prometheus);
        let mut keys: Vec<&str> = m.attributes.to_vec();
        keys.sort_unstable();
        for (a, _) in &points {
            let got_keys: Vec<&str> = a.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(got_keys, keys, "{otel} attributes");
        }
    }
    let role = number_points(&got["memory_graph.raft.role"]);
    assert_eq!(role.len(), 5, "one point per role");
    assert_eq!(role.iter().filter(|(_, v)| *v == 1.0).count(), 1);
    let build = number_points(&got["memory_graph.build.info"]);
    assert_eq!(build.len(), 1);
    assert_eq!(build[0].1, 1.0);
    let keys: Vec<&str> = build[0].0.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["protocol", "store_format", "version"]);
    let counts = otlp.collection_counts();
    assert_eq!(
        counts.builds, counts.collections,
        "one build per collection"
    );
    guard.shutdown_blocking();
}

/// A live node: every mapped metric arrives on the interval and matches a
/// scrape; the histograms use the buckets and count what `/metrics` does.
#[test]
fn a_live_node_exports_every_family_on_the_interval() {
    let _w = watchdog(
        "a_live_node_exports_every_family_on_the_interval",
        TEST_LIMIT,
    );
    let collector = FakeCollector::start();
    let guard = guard(&collector, Duration::from_millis(200));
    let otlp = guard.metrics().expect("metrics on");
    let dir = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_with(&dir.path().join("live.redb"), exts(), |c| {
        c.otlp_metrics = Some(otlp.clone());
    });
    let client = RemoteStore::connect(ClientConfig::new(server.endpoint())).unwrap();
    index_files(&client, "o", "r", &[small_file(0), small_file(1)]);
    for _ in 0..5 {
        assert!(client.health("").unwrap());
    }
    // An interval passes after the workload: every mapped metric arrives.
    let seen = collector.received().metrics.len();
    assert!(collector.wait_for(FakeSignal::Metrics, seen + 2, WAIT));
    let got = last_export(&collector);
    let scrape = client.admin_metrics().unwrap();
    let now = server.running().unwrap().metrics_snapshot();
    for m in METRIC_MAPPING {
        let Some(otel) = m.otel else { continue };
        if now
            .family(m.prometheus)
            .is_some_and(|f| f.samples.is_empty())
        {
            continue; // nothing observed yet (no peers, no MCP calls)
        }
        let metric = got
            .get(otel)
            .unwrap_or_else(|| panic!("{otel} missing: {:?}", got.keys()));
        assert_kind(metric, m.instrument, m.unit);
    }
    // Coarse agreement with the scrape taken just after.
    for (prom, otel) in [
        ("mg_raft_term", "memory_graph.raft.term"),
        ("mg_raft_leader_id", "memory_graph.raft.leader_id"),
        ("mg_raft_applied_index", "memory_graph.raft.applied_index"),
        ("mg_raft_role", "memory_graph.raft.role"),
        ("mg_build_info", "memory_graph.build.info"),
    ] {
        let otlp_sum: f64 = number_points(&got[otel]).iter().map(|(_, v)| v).sum();
        assert_eq!(otlp_sum, sample_sum(&scrape, prom), "{prom}");
    }
    for (prom, otel) in [
        ("mg_read_queries_total", "memory_graph.read.queries"),
        ("mg_queries_total", "memory_graph.queries"),
    ] {
        let otlp_sum: f64 = number_points(&got[otel]).iter().map(|(_, v)| v).sum();
        assert!(
            otlp_sum <= sample_sum(&scrape, prom),
            "{prom}: counters only grow"
        );
    }
    // The histograms: counts approximately those of /metrics (the scrape
    // itself and the calls since the export add a few).
    let rpc_otlp = histogram_count(&got["rpc.server.call.duration"]);
    let rpc_prom = sample_sum(&scrape, "mg_rpc_total") as u64;
    assert!(rpc_otlp >= 7, "the workload's calls: {rpc_otlp}");
    assert!(
        rpc_otlp <= rpc_prom && rpc_prom - rpc_otlp <= 20,
        "{rpc_otlp} vs {rpc_prom}"
    );
    let apply_otlp = histogram_count(&got["memory_graph.raft.apply.duration"]);
    let apply_prom = sample_sum(&scrape, "mg_apply_duration_seconds_count") as u64;
    assert!(
        apply_otlp >= 1 && apply_otlp <= apply_prom,
        "{apply_otlp} vs {apply_prom}"
    );
    no_overflow(&collector);
    let counts = otlp.collection_counts();
    assert!(counts.collections >= 2);
    assert_eq!(
        counts.builds, counts.collections,
        "one build per collection"
    );
    // A shutdown flushes a final export.
    server.stop();
    let before = collector.received().metrics.len();
    guard.shutdown_blocking();
    assert!(collector.wait_for(FakeSignal::Metrics, before + 1, WAIT));
}

/// A stalled collector never hurts serving; the failure counters rise and
/// export resumes once it answers again.
#[test]
fn a_stalled_collector_is_counted_and_export_resumes() {
    let _w = watchdog(
        "a_stalled_collector_is_counted_and_export_resumes",
        TEST_LIMIT,
    );
    let collector = FakeCollector::start();
    let guard = guard(&collector, Duration::from_millis(100));
    let otlp = guard.metrics().expect("metrics on");
    let dir = tempfile::tempdir().unwrap();
    let _server = TestServer::start_with(&dir.path().join("stall.redb"), exts(), |c| {
        c.otlp_metrics = Some(otlp.clone());
    });
    let client = RemoteStore::connect(ClientConfig::new(_server.endpoint())).unwrap();
    assert!(collector.wait_for(FakeSignal::Metrics, 1, WAIT));
    collector.stall();
    let failures = || guard.pipeline_counts("metrics").unwrap().failures;
    // Serving goes on while exports time out.
    wait_until("two metrics export failures", || {
        assert!(client.health("").unwrap());
        index_files(&client, "o", "r", &[small_file(2)]);
        failures() >= 2
    });
    let scrape = client.admin_metrics().unwrap();
    assert!(
        scrape
            .lines()
            .filter_map(|l| l.strip_prefix("mg_otel_export_failures_total{signal=\"metrics\"} "))
            .any(|v| v.parse::<u64>().unwrap() >= 2),
        "{scrape}"
    );
    assert!(
        sample_sum(&scrape, "mg_otel_dropped_total") >= 1.0,
        "{scrape}"
    );
    let c = guard.pipeline_counts("metrics").unwrap();
    assert!(
        c.dropped >= c.failures,
        "each failed export dropped its points: {c:?}"
    );
    collector.release();
    let before = collector.received().metrics.len();
    assert!(
        collector.wait_for(FakeSignal::Metrics, before + 2, WAIT),
        "export resumes"
    );
    guard.shutdown_blocking();
}

fn guard_at(endpoint: String, interval: Duration) -> TelemetryGuard {
    let options = TelemetryOptions {
        endpoint: Some(endpoint),
        signals: Some("metrics".into()),
        metrics_interval: Some(interval),
        ..Default::default()
    };
    let mut cfg = TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None)
        .expect("config");
    cfg.batch.export_timeout = Some(Duration::from_millis(500));
    telemetry::init(&cfg).expect("init").expect("enabled")
}

/// A collector that stops (connections closed) and comes back on the same
/// port: failures rise while it is gone, and export resumes with no restart.
#[test]
fn a_stopped_collector_is_counted_and_export_resumes_after_restart() {
    let _w = watchdog(
        "a_stopped_collector_is_counted_and_export_resumes_after_restart",
        TEST_LIMIT,
    );
    let mut collector = FakeCollector::start();
    let guard = guard_at(collector.endpoint(), Duration::from_millis(100));
    let otlp = guard.metrics().expect("metrics on");
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start_with(&dir.path().join("stop.redb"), exts(), |c| {
        c.otlp_metrics = Some(otlp.clone());
    });
    let client = RemoteStore::connect(ClientConfig::new(server.endpoint())).unwrap();
    assert!(collector.wait_for(FakeSignal::Metrics, 1, WAIT));
    collector.stop();
    let base = guard.pipeline_counts("metrics").unwrap().failures;
    wait_until("metrics export failures while stopped", || {
        assert!(client.health("").unwrap(), "serving is unaffected");
        guard.pipeline_counts("metrics").unwrap().failures >= base + 2
    });
    let before = collector.received().metrics.len();
    collector.restart();
    assert!(
        collector.wait_for(FakeSignal::Metrics, before + 2, WAIT),
        "export resumes after the restart"
    );
    guard.shutdown_blocking();
}

/// An endpoint nothing ever listened on: init and serving start at once,
/// and the failures count from the first interval.
#[test]
fn a_collector_that_never_existed_delays_nothing() {
    let _w = watchdog("a_collector_that_never_existed_delays_nothing", TEST_LIMIT);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port(); // the listener closes here: nothing listens on `port`
    let t = Instant::now();
    let guard = guard_at(
        format!("http://127.0.0.1:{port}"),
        Duration::from_millis(100),
    );
    let otlp = guard.metrics().expect("metrics on");
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start_with(&dir.path().join("none.redb"), exts(), |c| {
        c.otlp_metrics = Some(otlp.clone());
    });
    let client = RemoteStore::connect(ClientConfig::new(server.endpoint())).unwrap();
    assert!(client.health("").unwrap());
    // Generous: a start that waited on the collector would retry forever.
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    wait_until("the first failures", || {
        assert!(client.health("").unwrap());
        guard.pipeline_counts("metrics").unwrap().failures >= 1
    });
    let scrape = client.admin_metrics().unwrap();
    assert!(
        sample_sum(&scrape, "mg_otel_export_failures_total") >= 1.0,
        "{scrape}"
    );
    guard.shutdown_blocking();
}
