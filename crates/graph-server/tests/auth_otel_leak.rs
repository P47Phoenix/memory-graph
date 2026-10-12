//! ADR 0012 D8 over OpenTelemetry (issue #300): with traces and logs
//! exported to a `FakeCollector` at `trace` level, authenticated calls,
//! refusals of every kind and a cluster that forwards and replicates with
//! the cluster secret never put a token, its base64 body, its SHA-256 or a
//! `Bearer` header into an exported span (name, attribute, event, status)
//! or log record (body, attribute). The token id and role are exported
//! (`auth.token_id`, `auth.role`), which shows the spans looked at are the
//! authenticated ones.
//!
//! Its own test binary: it installs the process-wide subscriber.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_server::telemetry::{self, BatchTuning, NodeIdentity, TelemetryConfig, TelemetryOptions};
use graph_server::testing::auth::AuthFixture;
use graph_server::testing::{ClusterTestbed, FakeCollector, TestServer};
use graph_store::{Store, StoreRead};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// Everything a credential must never appear as.
fn needles(secret: &str) -> Vec<String> {
    vec![
        secret.to_string(),
        secret[4..].to_string(),
        graph_proto::auth::digest_hex(secret),
        format!("Bearer {}", &secret[..8]),
    ]
}

fn raw(ep: &str, path: &str, auth: Option<String>) -> tonic::Code {
    let ep = ep.to_string();
    let path = path.to_string();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut g = tonic::client::Grpc::new(ch);
            g.ready().await.unwrap();
            let mut req = tonic::Request::new(());
            if let Some(a) = auth {
                req.metadata_mut()
                    .insert("authorization", a.parse().unwrap());
            }
            let codec: tonic_prost::ProstCodec<(), ()> = Default::default();
            let p = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(path).unwrap();
            match g.unary(req, p, codec).await {
                Ok(_) => tonic::Code::Ok,
                Err(st) => st.code(),
            }
        })
}

#[test]
fn no_credential_reaches_an_exported_span_or_log_record() {
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
    let guard = telemetry::init(&cfg).unwrap().expect("traces and logs on");
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("trace"))
        .with(telemetry::tracing_layer(&guard).expect("a tracer"))
        .with(telemetry::log_layer(&guard).expect("a logger"))
        .init();

    let f = AuthFixture::new();
    let secrets = [
        f.write.as_str(),
        f.write2.as_str(),
        f.read.as_str(),
        f.expired.as_str(),
        f.secret.as_str(),
    ];

    // One server: valid calls with each role, and refusals of every kind.
    let d = tempfile::tempdir().unwrap();
    let ts = TestServer::start_with(&d.path().join("s.redb"), rust(), |c| c.auth = f.config());
    let ep = ts.endpoint();
    let mut wc = ClientConfig::new(ep.clone());
    wc.credentials = Some(f.creds(&f.write));
    let w = RemoteStore::connect(wc).unwrap();
    w.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    let mut rc = ClientConfig::new(ep.clone());
    rc.credentials = Some(f.creds(&f.read));
    let r = RemoteStore::connect(rc).unwrap();
    assert!(r.count_nodes(graph_core::NodeKind::File).unwrap() >= 1);
    assert!(r.vacuum().is_err());
    for (path, auth) in [
        ("/memory_graph.v1.Store/Get", None),
        (
            "/memory_graph.v1.Store/Get",
            Some(format!("Bearer {}", f.expired)),
        ),
        (
            "/memory_graph.v1.Store/Get",
            Some(format!("Bearer {}x", f.read)),
        ),
        (
            "/memory_graph.v1.Store/Get",
            Some(format!("Bearer {}", f.secret)),
        ),
        (
            "/memory_graph.v1.Write/Prune",
            Some(format!("Bearer {}", f.read)),
        ),
        (
            "/memory_graph.v1.Raft/Vote",
            Some(format!("Bearer {}", f.write)),
        ),
        (
            "/memory_graph.v1.Store/Nope",
            Some(format!("Bearer {}", f.write)),
        ),
    ] {
        assert_ne!(raw(&ep, path, auth), tonic::Code::Ok, "{path}");
    }

    // A cluster: a write through a follower is forwarded with the secret,
    // and the Raft RPCs carry it.
    let mut tb = ClusterTestbed::with_config(3, rust(), |_, c| c.auth = f.config());
    tb.set_client_credentials(Some(f.creds(&f.write)));
    tb.form();
    let follower = tb.ids().into_iter().find(|i| *i != tb.leader()).unwrap();
    let mut fc = ClientConfig::new(tb.node(follower).endpoint());
    fc.credentials = Some(f.creds(&f.write2));
    let c = RemoteStore::connect(fc).unwrap();
    c.index_bytes("o", "r", "b.rs", b"fn b() {}", None).unwrap();
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
    drop(tb);
    drop(ts);

    guard.tracer_provider().unwrap().force_flush().unwrap();
    guard.logger_provider().unwrap().force_flush().unwrap();
    let got = collector.received();
    let spans = got.spans();
    let logs = got.log_records();
    assert!(spans.len() > 20, "spans were exported ({})", spans.len());
    assert!(
        logs.len() > 20,
        "log records were exported ({})",
        logs.len()
    );
    // The authenticated spans are among them: the token id, never the token.
    assert!(
        spans.iter().any(|s| s.attr("auth.token_id") == Some("w")),
        "a span of the `w` token's calls"
    );
    assert!(
        logs.iter().any(|l| l.body.contains("auth: call refused")),
        "the refusals were logged"
    );
    for s in &spans {
        for text in &s.all_text {
            for secret in &secrets {
                for n in needles(secret) {
                    assert!(
                        !text.contains(&n),
                        "span `{}` carries a credential form `{}…`",
                        s.name,
                        &n[..6]
                    );
                }
            }
        }
    }
    for l in &logs {
        for text in &l.all_text {
            for secret in &secrets {
                for n in needles(secret) {
                    assert!(
                        !text.contains(&n),
                        "log record `{}` carries a credential form `{}…`",
                        l.body,
                        &n[..6]
                    );
                }
            }
        }
    }
}
