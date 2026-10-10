//! Traces (ADR 0009 D5, D9; epic story 51) in process: one global
//! subscriber with the OpenTelemetry layer exporting to a `FakeCollector`,
//! shared by every test here. Each test finds its own spans by trace id or
//! by name; nothing waits a fixed time (the collector wakes `wait_spans`).
//!
//! The three-process trace (client, follower, forward, leader, apply) is in
//! `graph-cli/tests/cluster_e2e.rs`; failures and recovery of the
//! collector in `otel_export_failures.rs`.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::mcp::McpConfig;
use graph_server::telemetry::{
    self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryGuard, TelemetryOptions,
};
use graph_server::testing::mcp_http::McpHttpClient;
use graph_server::testing::{
    ClusterTestbed, CollectedSpan, FakeCollector, TestServer, CLUSTER_WAIT,
};
use graph_server::RaftSettings;
use graph_store::{Query, Store, StoreRead};
use serde_json::json;
use std::sync::OnceLock;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const WAIT: Duration = Duration::from_secs(60);

struct Setup {
    collector: FakeCollector,
    guard: TelemetryGuard,
}

/// The process's collector, provider and global subscriber (once).
fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let collector = FakeCollector::start();
        let options = TelemetryOptions {
            endpoint: Some(collector.endpoint()),
            signals: Some("traces".into()),
            ..Default::default()
        };
        let mut cfg =
            TelemetryConfig::resolve_with_env(&options, &NodeIdentity::default(), |_| None)
                .unwrap();
        cfg.batch = BatchTuning {
            schedule_delay: Some(Duration::from_millis(50)),
            max_queue_size: Some(1 << 16),
            export_timeout: Some(Duration::from_secs(5)),
        };
        let guard = telemetry::init(&cfg).unwrap().expect("traces on");
        let layer = telemetry::tracing_layer(&guard).expect("a tracer");
        tracing_subscriber::registry().with(layer).init();
        Setup { collector, guard }
    })
}

/// Every span exported so far, after pushing out what is queued.
fn flushed() -> Vec<CollectedSpan> {
    let s = setup();
    s.guard.tracer_provider().unwrap().force_flush().unwrap();
    s.collector.received().spans()
}

fn in_trace<'a>(spans: &'a [CollectedSpan], trace: &str) -> Vec<&'a CollectedSpan> {
    spans.iter().filter(|s| s.trace_id == trace).collect()
}

/// A root span of the test's own (its target passes the layer's filter,
/// which exports this workspace's crates only).
fn test_span() -> tracing::Span {
    tracing::info_span!(target: "graph_server_test", "test")
}

fn trace_id_of(span: &tracing::Span) -> String {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.context().span().span_context().trace_id().to_string()
}

#[test]
fn an_incoming_traceparent_parents_the_rpc_span_with_rpc_attributes() {
    let s = setup();
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start(&d.path().join("g.redb"), exts());
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let parent = "00f067aa0ba902b7";
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", srv.endpoint()))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut c = graph_proto::pb::store_client::StoreClient::new(ch);
        let mut req = tonic::Request::new(graph_proto::pb::HelloRequest {
            protocol_version: graph_proto::PROTOCOL_VERSION,
            client_version: "test".into(),
        });
        req.metadata_mut().insert(
            "traceparent",
            format!("00-{trace}-{parent}-01").parse().unwrap(),
        );
        c.hello(req).await.unwrap();
    });
    let spans = s.collector.wait_spans(WAIT, |all| {
        all.iter().any(|x| x.trace_id == trace && x.name == "rpc")
    });
    let rpc = spans
        .iter()
        .find(|x| x.trace_id == trace && x.name == "rpc")
        .expect("the rpc span joined the caller's trace");
    assert_eq!(rpc.parent_span_id, parent, "a child of the caller's span");
    assert_eq!(rpc.attr("rpc.system"), Some("grpc"));
    assert_eq!(rpc.attr("rpc.service"), Some("memory_graph.v1.Store"));
    assert_eq!(rpc.attr("rpc.method"), Some("Hello"));
    assert_eq!(rpc.attr("rpc.grpc.status_code"), Some("0"));
    assert_eq!(rpc.attr("server.address"), Some("127.0.0.1"));
    assert_eq!(rpc.attr("method"), Some("Store/Hello"));
}

/// D5 spans for a `RemoteStore` call, an index batch and an MCP
/// `tools/call`; D9: a sentinel in the indexed source, the search and the
/// MCP arguments reaches no exported name, attribute or event.
#[test]
fn client_index_batch_and_mcp_spans_and_no_query_text() {
    const SENTINEL: &str = "Zq7SentinelQueryText";
    let s = setup();
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start_with(&d.path().join("g.redb"), exts(), |c| {
        c.mcp = Some(McpConfig::new("127.0.0.1:0".parse().unwrap()));
    });
    let mcp = srv.running().unwrap().mcp_addr.expect("MCP listens");
    let root = test_span();
    let trace = trace_id_of(&root);
    {
        let _in = root.enter();
        let c = RemoteStore::connect(ClientConfig::new(srv.endpoint())).unwrap();
        let src = format!("fn {SENTINEL}() {{ let x = \"{SENTINEL}\"; }}\n");
        index_files(
            &c,
            "o",
            "r",
            &[(format!("{SENTINEL}.rs"), src.into_bytes())],
        );
        assert!(!c.search(&Query::new(SENTINEL)).unwrap().is_empty());
    }
    // Ended, so exported.
    drop(root);
    let mut m = McpHttpClient::new(mcp);
    let r = m.request(
        "tools/call",
        json!({"name": "search", "arguments": {"text": SENTINEL, "grain": "token"}}),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");

    let spans = s.collector.wait_spans(WAIT, |all| {
        let t = in_trace(all, &trace);
        t.iter().any(|x| x.name == "index_batch")
            && t.iter().filter(|x| x.name == "client").count() >= 2
            && all.iter().any(|x| x.name == "mcp.tools_call")
    });
    let t = in_trace(&spans, &trace);
    let test = t
        .iter()
        .find(|x| x.name == "test")
        .map(|x| x.span_id.clone());
    let clients: Vec<_> = t.iter().filter(|x| x.name == "client").collect();
    assert!(clients.len() >= 2, "a client span per call: {t:#?}");
    for c in &clients {
        assert_eq!(
            Some(&c.parent_span_id),
            test.as_ref(),
            "client under the caller"
        );
    }
    let batch = t
        .iter()
        .find(|x| x.name == "index_batch")
        .expect("an index_batch span");
    let rpc = t
        .iter()
        .find(|x| x.span_id == batch.parent_span_id)
        .expect("index_batch's parent");
    assert_eq!(
        (rpc.name.as_str(), rpc.attr("rpc.method")),
        ("rpc", Some("Index"))
    );
    assert!(
        clients.iter().any(|c| c.span_id == rpc.parent_span_id),
        "the Index rpc is a client call's child"
    );
    assert_eq!(batch.attr("memory_graph.files"), Some("1"));
    let tool = spans
        .iter()
        .find(|x| x.name == "mcp.tools_call")
        .expect("an mcp.tools_call span");
    assert_eq!(tool.attr("memory_graph.tool"), Some("search"));
    assert_eq!(tool.attr("outcome"), Some("ok"));
    // D9: the sentinel is nowhere, in any span exported so far.
    for sp in flushed() {
        for text in &sp.all_text {
            assert!(!text.contains(SENTINEL), "{text:?} in {sp:#?}");
        }
        for v in sp.attributes.keys() {
            assert!(!v.contains(SENTINEL));
        }
    }
}

/// D5: a snapshot install is one `install_snapshot` span on each side (the
/// receiver's a child of the sender's, nothing under either), and no Raft
/// service `rpc` span (heartbeats, AppendEntries) is ever exported.
#[test]
fn snapshot_install_is_one_span_each_side_and_raft_rpcs_are_not_exported() {
    setup();
    let snappy = RaftSettings {
        snapshot_log_entries: 5,
        log_keep_entries: 2,
        purge_batch_size: 1,
        ..graph_server::testing::TEST_RAFT
    };
    let mut tb = ClusterTestbed::with_config(3, exts(), |_, c| c.raft = Some(snappy));
    tb.form();
    let leader = tb.leader();
    let laggard = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    let c = tb.client(leader);
    index_files(&c, "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let behind = tb.node(laggard).applied_index();
    tb.node_mut(laggard).stop();
    for i in 1..20 {
        c.index_bytes("o", "r", &small_file(i).0, &small_file(i).1, None)
            .unwrap();
    }
    let raft = tb.node(leader).raft().unwrap().raft.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        raft.wait(Some(CLUSTER_WAIT))
            .metrics(
                |m| m.purged.is_some_and(|p| p.index > behind),
                "the leader purged past the laggard",
            )
            .await
            .unwrap()
    });
    tb.node_mut(laggard).restart();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert!(tb.node(laggard).raft().unwrap().snapshots_installed() >= 1);

    // Both sides' spans end once the install answered.
    setup().collector.wait_spans(WAIT, |all| {
        let n = |root: bool| {
            all.iter()
                .filter(|s| s.name == "install_snapshot" && s.parent_span_id.is_empty() == root)
                .count()
        };
        n(true) >= 1 && n(false) >= n(true)
    });
    let spans = flushed();
    let installs: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "install_snapshot")
        .collect();
    let senders: Vec<_> = installs
        .iter()
        .filter(|s| s.parent_span_id.is_empty())
        .collect();
    let receivers: Vec<_> = installs
        .iter()
        .filter(|s| !s.parent_span_id.is_empty())
        .collect();
    let mut names: Vec<_> = spans.iter().map(|s| s.name.clone()).collect();
    names.sort();
    names.dedup();
    assert!(
        !senders.is_empty(),
        "the leader's span: {installs:#?} among {names:?}"
    );
    assert_eq!(
        senders.len(),
        receivers.len(),
        "one span on each side per install: {installs:#?}"
    );
    for r in &receivers {
        let s = senders
            .iter()
            .find(|s| s.span_id == r.parent_span_id)
            .expect("the receiver's parent is the sender's span");
        assert_eq!(s.trace_id, r.trace_id);
        assert_eq!(
            s.attr("memory_graph.snapshot_bytes"),
            r.attr("memory_graph.snapshot_bytes")
        );
    }
    for i in &installs {
        let under: Vec<_> = spans
            .iter()
            .filter(|s| s.parent_span_id == i.span_id && s.name != "install_snapshot")
            .collect();
        assert!(under.is_empty(), "no per-chunk spans: {under:#?}");
    }
    let raft_rpcs: Vec<_> = spans
        .iter()
        .filter(|s| s.attr("rpc.service") == Some("memory_graph.v1.Raft"))
        .collect();
    assert!(
        raft_rpcs.is_empty(),
        "Raft rpc spans exported: {raft_rpcs:#?}"
    );
    drop(tb);
}

/// Without a span of the caller's, a `RemoteStore` call is the root of its
/// trace and the server's `rpc` is its child.
#[test]
fn a_remote_store_call_without_a_caller_span_is_a_root() {
    let s = setup();
    let d = tempfile::tempdir().unwrap();
    let srv = TestServer::start(&d.path().join("g.redb"), exts());
    let c = RemoteStore::connect(ClientConfig::new(srv.endpoint())).unwrap();
    let before = s.collector.received().spans().len();
    c.count_nodes(graph_core::NodeKind::File).unwrap();
    let spans = s.collector.wait_spans(WAIT, |all| {
        all[before.min(all.len())..].iter().any(|x| {
            x.name == "rpc"
                && x.attr("rpc.method") == Some("CountNodes")
                && all
                    .iter()
                    .any(|p| p.span_id == x.parent_span_id && p.name == "client")
        })
    });
    let rpc = spans
        .iter()
        .find(|x| x.name == "rpc" && x.attr("rpc.method") == Some("CountNodes"))
        .unwrap();
    let client = spans
        .iter()
        .find(|p| p.span_id == rpc.parent_span_id)
        .expect("the client span");
    assert_eq!(client.name, "client");
    assert!(client.parent_span_id.is_empty(), "a root");
}
