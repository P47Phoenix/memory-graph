//! ADR 0012 D8, the leak test: with every target logging at `trace` into a
//! buffer, an authenticated conformance run, refusals of every kind, a
//! cluster that forwards and replicates with the cluster secret, and a
//! flood of bad tokens never write a token, its base64 body, its SHA-256
//! or a `Bearer` header line anywhere: not in the logs, not in a status
//! message, not in a `Debug` or `Display`.
//!
//! Its own test binary: it installs the process-wide subscriber.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_server::testing::auth::AuthFixture;
use graph_server::testing::{ClusterTestbed, TestServer};
use graph_store::{conformance, open_store, Store, StoreRead};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything a secret must never appear as.
fn needles(secret: &str) -> Vec<String> {
    vec![
        secret.to_string(),
        secret[4..].to_string(),
        graph_proto::auth::digest_hex(secret),
        format!("Bearer {}", &secret[..8]),
    ]
}

fn assert_clean(what: &str, text: &str, secrets: &[&str]) {
    for s in secrets {
        for n in needles(s) {
            assert!(
                !text.contains(&n),
                "{what} contains a credential form `{}…`",
                &n[..6]
            );
        }
    }
}

fn raw(ep: &str, path: &str, auth: Option<String>) -> tonic::Status {
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
                Ok(_) => tonic::Status::ok(""),
                Err(st) => st,
            }
        })
}

#[test]
fn no_credential_is_ever_logged_or_echoed() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_ansi(false)
        .with_writer(move || w.clone())
        .init();

    let f = AuthFixture::new();
    let secrets = [
        f.write.as_str(),
        f.write2.as_str(),
        f.read.as_str(),
        f.expired.as_str(),
        f.secret.as_str(),
    ];
    let mut seen = Vec::<(String, String)>::new();

    // An authenticated differential run.
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let ts = TestServer::start_with(&d.path().join("s.redb"), rust(), |c| c.auth = f.config());
    let mut cfg = ClientConfig::new(ts.endpoint());
    cfg.credentials = Some(f.creds(&f.write));
    seen.push(("ClientConfig Debug".into(), format!("{cfg:?}")));
    seen.push((
        "Credentials Debug".into(),
        format!("{:?}", f.creds(&f.write)),
    ));
    let remote = RemoteStore::connect(cfg).unwrap();
    conformance::run_differential(&*embedded, &remote);

    // Refusals of every kind, and the errors and statuses they produce.
    let ep = ts.endpoint();
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
        let st = raw(&ep, path, auth);
        seen.push(("Status".into(), format!("{st:?} {}", st.message())));
        let e = graph_proto::status_to_store_error(&st);
        seen.push(("StoreError".into(), format!("{e} {e:?}")));
    }
    let mut ro = ClientConfig::new(ts.endpoint());
    ro.credentials = Some(f.creds(&f.read));
    let r = RemoteStore::connect(ro).unwrap();
    let e = r.vacuum().unwrap_err();
    seen.push(("client error".into(), format!("{e} {e:?}")));
    let mut bad = ClientConfig::new(ts.endpoint());
    bad.credentials = Some(f.creds(&f.expired));
    let e = RemoteStore::connect(bad).err().unwrap();
    seen.push(("connect error".into(), format!("{e} {e:?}")));

    // A flood of bad tokens: bounded refusal lines, every one counted.
    let auth = ts.running().unwrap().auth();
    let before: u64 = auth.refusal_counts().iter().map(|(_, n)| n).sum();
    let lines_before = count_refusal_lines(&buf);
    let flood = 300;
    for _ in 0..flood {
        raw(
            &ep,
            "/memory_graph.v1.Store/Get",
            Some(format!("Bearer {}", f.expired)),
        );
    }
    let after: u64 = auth.refusal_counts().iter().map(|(_, n)| n).sum();
    assert_eq!(after - before, flood);
    let logged = count_refusal_lines(&buf) - lines_before;
    assert!(
        logged <= 2,
        "{logged} refusal lines for {flood} refusals (rate limit)"
    );

    // A cluster forwarding and replicating with the secret.
    let mut tb = ClusterTestbed::with_config(3, rust(), |_, c| c.auth = f.config());
    tb.set_client_credentials(Some(f.creds(&f.write)));
    tb.form();
    let follower = tb.ids().into_iter().find(|i| *i != tb.leader()).unwrap();
    let mut fc = ClientConfig::new(tb.node(follower).endpoint());
    fc.credentials = Some(f.creds(&f.write));
    let c = RemoteStore::connect(fc).unwrap();
    c.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
    assert!(c.count_nodes(graph_core::NodeKind::File).unwrap() >= 1);

    // A panic in a handler thread (what a backtrace would print).
    let p = std::panic::catch_unwind(|| {
        panic!(
            "handler failed for {:?}",
            graph_server::testing::auth::AuthFixture::new().config()
        )
    })
    .unwrap_err();
    seen.push((
        "panic".into(),
        p.downcast_ref::<String>().cloned().unwrap_or_default(),
    ));

    drop(tb);
    drop(ts);
    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
    assert!(
        logs.len() > 100_000,
        "trace logs were captured ({} bytes)",
        logs.len()
    );
    assert!(logs.contains("auth: call refused"), "refusals were logged");
    assert_clean("the trace logs", &logs, &secrets);
    for (what, text) in &seen {
        assert_clean(what, text, &secrets);
    }
}

fn count_refusal_lines(buf: &Buf) -> usize {
    String::from_utf8_lossy(&buf.0.lock().unwrap())
        .matches("auth: call refused")
        .count()
}
