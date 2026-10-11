//! Authentication (ADR 0012 D10) against an in-process server: every RPC
//! refused without a valid credential (table-driven from the generated
//! services), the trimmed `Hello`, roles, the cluster secret, streaming,
//! snapshot handles, reloads, conformance with and without credentials,
//! forwarding and the Raft path in a cluster, MCP over HTTP and the
//! `--insecure-transport` guard.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_core::NodeKind;
use graph_proto::pb;
use graph_server::testing::auth::{stanza, AuthFixture};
use graph_server::testing::{ClusterTestbed, TestServer};
use graph_server::ServeConfig;
use graph_store::conformance::{self, Harness};
use graph_store::{open_store, Store, StoreError, StoreRead};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::Code;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn start(f: &AuthFixture, db: &std::path::Path) -> TestServer {
    TestServer::start_with(db, rust(), |c| c.auth = f.config())
}

fn client(
    ts: &TestServer,
    token: Option<&str>,
    f: &AuthFixture,
) -> Result<RemoteStore, StoreError> {
    let mut cfg = ClientConfig::new(ts.endpoint());
    cfg.credentials = token.map(|t| f.creds(t));
    RemoteStore::connect(cfg)
}

/// One call to `path` with an empty message and these metadata pairs; the
/// status code (`Ok` for an answer, or a call still running after 3 s: a
/// stream the server accepted).
fn raw(ep: &str, path: &str, md: &[(&str, String)]) -> Code {
    let ep = ep.to_string();
    let path = path.to_string();
    let md: Vec<(String, String)> = md.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    rt().block_on(async move {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut g = tonic::client::Grpc::new(ch);
        g.ready().await.unwrap();
        let mut req = tonic::Request::new(());
        for (k, v) in &md {
            let key = tonic::metadata::MetadataKey::from_bytes(k.as_bytes()).unwrap();
            req.metadata_mut().insert(key, v.parse().unwrap());
        }
        let codec: tonic_prost::ProstCodec<(), ()> = Default::default();
        let p = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(path).unwrap();
        match tokio::time::timeout(Duration::from_secs(3), g.unary(req, p, codec)).await {
            Ok(Ok(_)) | Err(_) => Code::Ok,
            Ok(Err(st)) => st.code(),
        }
    })
}

fn bearer(v: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {v}"))
}

const HEALTH: [&str; 1] = ["/grpc.health.v1.Health/Check"];

/// Methods a `read` token may call without side effects worth avoiding.
fn is_write(path: &str) -> bool {
    matches!(
        graph_server::auth::need(path),
        Some(graph_server::auth::Need::Write)
    )
}

#[test]
fn every_rpc_is_refused_without_a_valid_credential() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let ep = ts.endpoint();
    let other_secret = graph_server::auth::new_credential("mgc_").to_string();
    let not_listed = graph_server::auth::new_credential("mgt_").to_string();
    let creds: Vec<(&str, Vec<(&str, String)>)> = vec![
        ("none", vec![]),
        ("wrong", vec![bearer(&not_listed)]),
        ("expired", vec![bearer(&f.expired)]),
        ("mgc_ of another cluster", vec![bearer(&other_secret)]),
        ("oversized", vec![bearer(&"A".repeat(200))]),
        ("non-mgt", vec![bearer("abc")]),
        (
            "basic",
            vec![("authorization", format!("Basic {}", f.write))],
        ),
    ];
    let paths = graph_proto::rpc_paths();
    assert!(paths.len() > 40, "{}", paths.len());
    for (what, md) in &creds {
        for p in paths.iter() {
            let code = raw(&ep, p, md);
            if *p == "/memory_graph.v1.Store/Hello" {
                assert_ne!(code, Code::Unauthenticated, "{what} {p}");
            } else {
                assert_eq!(code, Code::Unauthenticated, "{what} {p}");
            }
        }
        for p in HEALTH {
            assert_eq!(raw(&ep, p, md), Code::Ok, "{what} {p}");
        }
        // A bad token with a bad protocol version: still UNAUTHENTICATED
        // (auth runs first and says nothing about versions).
        let mut with_version = md.clone();
        with_version.push(("mg-protocol-version", "99".into()));
        assert_eq!(
            raw(&ep, "/memory_graph.v1.Store/Get", &with_version),
            Code::Unauthenticated,
            "{what}"
        );
        // An unlisted method is refused for every credential.
        assert_eq!(
            raw(&ep, "/memory_graph.v1.Store/NotAMethod", md),
            Code::PermissionDenied,
            "{what}"
        );
    }
    for t in [&f.write, &f.read] {
        assert_eq!(
            raw(&ep, "/memory_graph.v1.Store/NotAMethod", &[bearer(t)]),
            Code::PermissionDenied
        );
        assert_eq!(
            raw(&ep, "/test.Unlisted/Call", &[bearer(t)]),
            Code::PermissionDenied
        );
    }
    // Nothing was written by any of it.
    let c = client(&ts, Some(&f.read), &f).unwrap();
    assert_eq!(c.count_nodes(NodeKind::Token).unwrap(), 0);
    // The refusals are counted by reason.
    let counts = ts.running().unwrap().auth().refusal_counts();
    let n = |r: &str| counts.iter().find(|(k, _)| *k == r).unwrap().1;
    assert!(n("missing") > 0 && n("malformed") > 0 && n("unknown") > 0 && n("expired") > 0);
    assert!(n("unlisted") > 0);
}

/// A ~20 KB `authorization` header never reaches the auth layer: the HTTP
/// layer refuses the oversized header block (431 under hyper's limits),
/// which a gRPC client sees as `UNKNOWN`. Never an answer with data.
#[test]
fn a_huge_authorization_header_is_refused_by_the_transport() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let huge = format!("Bearer {}", "A".repeat(20 * 1024));
    let code = raw(
        &ts.endpoint(),
        "/memory_graph.v1.Store/CountNodes",
        &[("authorization", huge)],
    );
    assert_eq!(code, Code::Unknown);
    // The server is still fine afterwards.
    assert_eq!(
        raw(
            &ts.endpoint(),
            "/memory_graph.v1.Admin/Status",
            &[bearer(&f.read)]
        ),
        Code::Ok
    );
}

#[test]
fn read_tokens_read_and_every_write_method_is_permission_denied() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let ep = ts.endpoint();
    for p in graph_proto::rpc_paths().iter() {
        let code = raw(&ep, p, &[bearer(&f.read)]);
        match graph_server::auth::need(p).unwrap() {
            graph_server::auth::Need::Write => {
                assert_eq!(code, Code::PermissionDenied, "{p}")
            }
            graph_server::auth::Need::Node => assert_eq!(code, Code::Unauthenticated, "{p}"),
            _ => assert!(
                code != Code::PermissionDenied && code != Code::Unauthenticated,
                "{p}: {code:?}"
            ),
        }
    }
    // A client `write` token never authorises a node-to-node method.
    for p in [
        "/memory_graph.v1.Raft/AppendEntries",
        "/memory_graph.v1.Raft/Vote",
        "/memory_graph.v1.Raft/InstallSnapshot",
        "/memory_graph.v1.Admin/Join",
        "/memory_graph.v1.Admin/UpdateAdvertise",
    ] {
        assert_eq!(
            raw(&ep, p, &[bearer(&f.write)]),
            Code::Unauthenticated,
            "{p}"
        );
        // The cluster secret passes the layer (the handler may refuse the
        // empty message for its own reasons).
        let c = raw(&ep, p, &[bearer(&f.secret)]);
        assert!(
            c != Code::Unauthenticated && c != Code::PermissionDenied,
            "{p}: {c:?}"
        );
    }
    // The cluster secret on a direct Store or Write call: PERMISSION_DENIED.
    for p in [
        "/memory_graph.v1.Store/Get",
        "/memory_graph.v1.Write/Prune",
        "/memory_graph.v1.Admin/Shutdown",
    ] {
        assert_eq!(
            raw(&ep, p, &[bearer(&f.secret)]),
            Code::PermissionDenied,
            "{p}"
        );
    }
    // ... and the join probes are allowed.
    for p in [
        "/memory_graph.v1.Admin/Status",
        "/memory_graph.v1.Admin/Members",
    ] {
        assert_eq!(raw(&ep, p, &[bearer(&f.secret)]), Code::Ok, "{p}");
    }
    // Through the client: exit code 9's error, never retried.
    let r = client(&ts, Some(&f.read), &f).unwrap();
    let auth = ts.running().unwrap().auth();
    let refused = || {
        auth.refusal_counts()
            .iter()
            .filter(|(r, _)| *r != "invalid_on_open")
            .map(|(_, n)| n)
            .sum::<u64>()
    };
    let before = refused();
    let e = r.vacuum().unwrap_err();
    assert!(matches!(e, StoreError::PermissionDenied { .. }), "{e:?}");
    assert_eq!(refused() - before, 1, "sent once: never retried");
    let mut cfg = ClientConfig::new(ts.endpoint());
    cfg.credentials = Some(f.creds(&f.write));
    let w = RemoteStore::connect(cfg).unwrap();
    // An unknown token on a connected client: one refused call, no retry.
    let before = refused();
    let e = graph_client::RemoteStore::connect({
        let mut c = ClientConfig::new(ts.endpoint());
        c.credentials = Some(f.creds(&f.expired));
        c
    })
    .err()
    .unwrap();
    assert!(matches!(e, StoreError::Unauthenticated(_)), "{e:?}");
    assert_eq!(refused(), before, "Hello is open: nothing refused");
    let open_bad = auth
        .refusal_counts()
        .into_iter()
        .find(|(r, _)| *r == "invalid_on_open")
        .unwrap()
        .1;
    assert!(open_bad >= 1, "an invalid token on Hello leaves a signal");
    drop(w);
    assert!(!is_write("/memory_graph.v1.Store/Get"));
}

#[test]
fn hello_is_trimmed_for_an_unauthenticated_caller() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let hello = |token: Option<&str>| {
        let ep = ts.endpoint();
        let token = token.map(str::to_string);
        rt().block_on(async move {
            let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut req = tonic::Request::new(pb::HelloRequest {
                protocol_version: graph_proto::PROTOCOL_VERSION,
                client_version: "t".into(),
            });
            if let Some(t) = token {
                req.metadata_mut()
                    .insert("authorization", format!("Bearer {t}").parse().unwrap());
            }
            pb::store_client::StoreClient::new(ch)
                .hello(req)
                .await
                .unwrap()
                .into_inner()
        })
    };
    let trimmed = pb::HelloResponse {
        protocol_version: graph_proto::PROTOCOL_VERSION,
        auth_required: true,
        ..Default::default()
    };
    assert_eq!(hello(None), trimmed);
    assert_eq!(hello(Some(&f.expired)), trimmed);
    assert_eq!(hello(Some(&f.secret[..10])), trimmed);
    let full = hello(Some(&f.read));
    assert!(full.auth_required && !full.server_version.is_empty() && full.node_id == 1);
    // The client says what is missing, and never retries.
    let e = client(&ts, None, &f).err().unwrap();
    assert!(
        matches!(&e, StoreError::Unauthenticated(m) if m.contains("--token-file")),
        "{e:?}"
    );
    let mut cfg = ClientConfig::new(ts.endpoint());
    cfg.credentials = Some(f.creds(&f.expired));
    let e = RemoteStore::connect(cfg).err().unwrap();
    assert!(
        matches!(&e, StoreError::Unauthenticated(m) if m.contains("refused")),
        "{e:?}"
    );
    // Health answers SERVING without a token.
    let c = client(&ts, Some(&f.read), &f).unwrap();
    assert!(c.health("").unwrap());
    let mut no_creds = ClientConfig::new(ts.endpoint());
    no_creds.retry.budget = Duration::from_millis(200);
    assert_eq!(
        raw(&ts.endpoint(), "/grpc.health.v1.Health/Check", &[]),
        Code::Ok
    );
}

#[test]
fn an_index_stream_without_a_write_token_changes_nothing() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let w = client(&ts, Some(&f.write), &f).unwrap();
    w.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    let before = (
        w.count_nodes(NodeKind::Token).unwrap(),
        format!("{:?}", w.describe(None, None).unwrap()),
    );
    let r = client(&ts, Some(&f.read), &f).unwrap();
    let e = r
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(matches!(e, StoreError::PermissionDenied { .. }), "{e:?}");
    // A raw Index stream with no token at all.
    assert_eq!(
        raw(&ts.endpoint(), "/memory_graph.v1.Write/Index", &[]),
        Code::Unauthenticated
    );
    let after = (
        w.count_nodes(NodeKind::Token).unwrap(),
        format!("{:?}", w.describe(None, None).unwrap()),
    );
    assert_eq!(before, after);
}

#[test]
fn a_snapshot_handle_is_bound_to_the_token_that_opened_it() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let ep = ts.endpoint();
    let w = client(&ts, Some(&f.write), &f).unwrap();
    w.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    // Open with token A on one connection, then read on it, with token B,
    // without a token, and from another connection with token A.
    let (own, other, anon, other_conn) = rt().block_on({
        let (a, b) = (f.write.clone(), f.write2.clone());
        async move {
            let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut c = pb::store_client::StoreClient::new(ch.clone());
            let id = c
                .open_snapshot(with(Some(&a), pb::OpenSnapshotRequest {}))
                .await
                .unwrap()
                .into_inner()
                .snapshot_id;
            let get = |v| pb::CountNodesRequest {
                view: Some(pb::View {
                    v: Some(pb::view::V::SnapshotId(v)),
                }),
                kind: pb::NodeKind::Token as i32,
            };
            let own = c.count_nodes(with(Some(&a), get(id))).await.map(|_| ());
            let other = c.count_nodes(with(Some(&b), get(id))).await.map(|_| ());
            let anon = c.count_nodes(with(None, get(id))).await.map(|_| ());
            let ch2 = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let other_conn = pb::store_client::StoreClient::new(ch2)
                .count_nodes(with(Some(&a), get(id)))
                .await
                .map(|_| ());
            (own, other, anon, other_conn)
        }
    });
    assert!(own.is_ok(), "{own:?}");
    let expired = |r: Result<(), tonic::Status>| {
        let st = r.unwrap_err();
        matches!(
            graph_proto::status_to_store_error(&st),
            StoreError::SnapshotExpired { .. }
        )
    };
    assert!(expired(other));
    assert_eq!(anon.unwrap_err().code(), Code::Unauthenticated);
    assert!(expired(other_conn));
}

fn with<T>(t: Option<&str>, msg: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    if let Some(t) = t {
        r.metadata_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    r
}

fn authed_harness(f: Arc<AuthFixture>) -> Harness {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let server: Arc<Mutex<Option<TestServer>>> = Arc::new(Mutex::new(None));
    let s2 = Arc::clone(&server);
    let f2 = Arc::clone(&f);
    Harness {
        open: Box::new(move |ex| {
            let mut g = s2.lock().unwrap();
            if let Some(mut old) = g.take() {
                old.stop();
            }
            let ts = TestServer::start_with(&db, ex, |c| c.auth = f2.config());
            let mut cfg = ClientConfig::new(ts.endpoint());
            cfg.credentials = Some(f2.creds(&f2.write));
            let store = RemoteStore::connect(cfg)?;
            *g = Some(ts);
            Ok(Box::new(store) as Box<dyn Store>)
        }),
        exclusive: false,
        accepts_remote_prepared: true,
        guard: Some(Box::new((d, server, f))),
    }
}

#[test]
fn conformance_with_a_write_token() {
    let f = Arc::new(AuthFixture::new());
    conformance::run_all(&|| authed_harness(Arc::clone(&f)));
}

#[test]
fn differentials_with_a_write_token() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let ts = start(&f, &d.path().join("s.redb"));
    let remote = client(&ts, Some(&f.write), &f).unwrap();
    conformance::run_differential(&*embedded, &remote);

    let d = tempfile::tempdir().unwrap();
    let fresh = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let ts = start(&f, &d.path().join("s.redb"));
    let crashed = client(&ts, Some(&f.write), &f).unwrap();
    conformance::run_crash_rerun_differential(&*fresh, &crashed);
}

/// Auth off: a client that sends a token is accepted (the token ignored).
#[test]
fn auth_off_ignores_a_token() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let ts = TestServer::start(&d.path().join("s.redb"), rust());
    let remote = client(&ts, Some(&f.write), &f).unwrap();
    assert!(!remote.hello().auth_required);
    conformance::run_differential(&*embedded, &remote);
    assert!(ts
        .running()
        .unwrap()
        .auth()
        .refusal_counts()
        .iter()
        .all(|(_, n)| *n == 0));
}

#[test]
fn reloads_take_effect_on_the_next_call_and_bad_files_keep_the_set() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let auth = ts.running().unwrap().auth();
    let ep = ts.endpoint();
    let get = |t: &str| raw(&ep, "/memory_graph.v1.Admin/Status", &[bearer(t)]);
    let new = graph_server::auth::new_credential("mgt_").to_string();
    assert_eq!(get(&new), Code::Unauthenticated);
    // Old and new at once.
    f.write_tokens(&format!(
        "{}\n{}",
        f.default_tokens(),
        stanza("new", "read", &new, None)
    ));
    auth.reload_now();
    assert_eq!(get(&new), Code::Ok);
    assert_eq!(get(&f.read), Code::Ok);
    // `read` removed: refused on the next call.
    f.write_tokens(&stanza("new", "read", &new, None));
    auth.reload_now();
    assert_eq!(get(&f.read), Code::Unauthenticated);
    // Half-written, invalid, deleted: the loaded set stays.
    let good = stanza("new", "read", &new, None);
    for bad in [&good[..good.len() / 2], "[[token]]\nscope = 1\n"] {
        f.write_tokens(bad);
        auth.reload_now();
        assert_eq!(get(&new), Code::Ok);
    }
    std::fs::remove_file(&f.tokens).unwrap();
    auth.reload_now();
    assert_eq!(get(&new), Code::Ok);
    // An expiry already passed: refused at the first call after the reload.
    f.write_tokens(&stanza("new", "read", &new, Some("2001-01-01T00:00:00Z")));
    auth.reload_now();
    assert_eq!(get(&new), Code::Unauthenticated);
}

/// A swap of the file behind a symlink (Kubernetes' `..data` pattern) is
/// picked up by content.
#[cfg(unix)]
#[test]
fn a_symlink_swap_is_picked_up() {
    let f = AuthFixture::new();
    let dir = f.dir.path();
    let v1 = dir.join("v1.toml");
    let v2 = dir.join("v2.toml");
    std::fs::write(&v1, stanza("r", "read", &f.read, None)).unwrap();
    std::fs::write(&v2, stanza("w", "write", &f.write, None)).unwrap();
    let link = dir.join("current.toml");
    std::os::unix::fs::symlink(&v1, &link).unwrap();
    let d = tempfile::tempdir().unwrap();
    let mut cfg = f.config();
    cfg.tokens = Some(link.clone());
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), |c| c.auth = cfg);
    let ep = ts.endpoint();
    let get = |t: &str| raw(&ep, "/memory_graph.v1.Admin/Status", &[bearer(t)]);
    assert_eq!(get(&f.read), Code::Ok);
    let tmp = dir.join("next");
    std::os::unix::fs::symlink(&v2, &tmp).unwrap();
    std::fs::rename(&tmp, &link).unwrap();
    ts.running().unwrap().auth().reload_now();
    assert_eq!(get(&f.read), Code::Unauthenticated);
    assert_eq!(get(&f.write), Code::Ok);
}

/// SIGHUP reloads at once (Unix); the poll is what Windows has.
#[cfg(unix)]
#[test]
fn sighup_reloads() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let ep = ts.endpoint();
    let new = graph_server::auth::new_credential("mgt_").to_string();
    f.write_tokens(&stanza("new", "read", &new, None));
    unsafe {
        libc_kill();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while raw(&ep, "/memory_graph.v1.Admin/Status", &[bearer(&new)]) != Code::Ok {
        assert!(
            std::time::Instant::now() < deadline,
            "SIGHUP did not reload within 4 s (the poll is 5 s)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
unsafe fn libc_kill() {
    extern "C" {
        fn getpid() -> i32;
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGHUP: i32 = 1;
    kill(getpid(), SIGHUP);
}

#[test]
fn the_poll_reloads_without_a_signal() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = start(&f, &d.path().join("g.redb"));
    let ep = ts.endpoint();
    let new = graph_server::auth::new_credential("mgt_").to_string();
    f.write_tokens(&stanza("new", "read", &new, None));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while raw(&ep, "/memory_graph.v1.Admin/Status", &[bearer(&new)]) != Code::Ok {
        assert!(
            std::time::Instant::now() < deadline,
            "no reload by the poll"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn start_refuses_bad_auth_files() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    for text in ["", "[[token]]\nid = \"x\"\n"] {
        f.write_tokens(text);
        let e =
            TestServer::try_start_with(&d.path().join("g.redb"), rust(), |c| c.auth = f.config())
                .err()
                .unwrap();
        assert!(e.to_string().contains("--auth-tokens"), "{e}");
    }
    f.write_tokens(&f.default_tokens());
    // A --data-dir node needs the cluster secret.
    let mut cfg = ServeConfig::for_data_dir(
        d.path().join("n"),
        "127.0.0.1:0".parse().unwrap(),
        graph_server::InitMode::Bootstrap { restore: None },
        Some(1),
    );
    cfg.auth = f.config();
    cfg.auth.cluster_secret = None;
    let e = TestServer::try_start_config(cfg, rust()).err().unwrap();
    assert!(e.to_string().contains("--cluster-secret-file"), "{e}");
}

#[test]
fn insecure_transport_matrix() {
    use graph_server::server::check_transport;
    let base = |listen: &str| {
        let mut c = ServeConfig::new("x.redb", listen.parse().unwrap());
        c.auth.tokens = Some("t".into());
        c
    };
    for ok in ["127.0.0.1:7000", "[::1]:7000", "127.5.6.7:1"] {
        assert!(check_transport(&base(ok), true).is_ok(), "{ok}");
    }
    for bad in ["0.0.0.0:7000", "[::]:7000", "192.168.1.10:7000"] {
        let e = check_transport(&base(bad), true).unwrap_err().to_string();
        assert!(e.contains("--insecure-transport"), "{e}");
        assert!(
            check_transport(&base(bad), false).is_ok(),
            "auth off: {bad}"
        );
        let mut c = base(bad);
        c.insecure_transport = true;
        assert!(check_transport(&c, true).is_ok(), "{bad}");
    }
    type Tweak = Box<dyn Fn(&mut ServeConfig)>;
    let cases: Vec<(&str, Tweak)> = vec![
        (
            "--advertise",
            Box::new(|c| c.advertise = Some("10.0.0.1:7000".into())),
        ),
        (
            "--advertise",
            Box::new(|c| c.advertise = Some("[::]:7000".into())),
        ),
        (
            "--mcp-listen",
            Box::new(|c| {
                c.mcp = Some(graph_server::mcp::McpConfig::new(
                    "0.0.0.0:7001".parse().unwrap(),
                ))
            }),
        ),
        (
            "--metrics-listen",
            Box::new(|c| c.metrics_listen = Some("192.168.1.10:9000".parse().unwrap())),
        ),
    ];
    for (flag, set) in &cases {
        let mut c = base("127.0.0.1:7000");
        set(&mut c);
        let e = check_transport(&c, true).unwrap_err().to_string();
        assert!(e.contains(flag), "{flag}: {e}");
        c.insecure_transport = true;
        assert!(check_transport(&c, true).is_ok(), "{flag}");
    }
    for ok in [
        "localhost:7000",
        "127.0.0.1:7000",
        "[::1]:7000",
        "http://localhost:7000",
    ] {
        let mut c = base("127.0.0.1:7000");
        c.advertise = Some(ok.into());
        assert!(check_transport(&c, true).is_ok(), "{ok}");
    }
}

#[test]
fn mcp_over_http_needs_a_token() {
    use graph_server::testing::mcp_http::{http, McpHttpClient};
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), |c| {
        c.auth = f.config();
        c.mcp = Some(graph_server::mcp::McpConfig::new(
            "127.0.0.1:0".parse().unwrap(),
        ));
    });
    let w = client(&ts, Some(&f.write), &f).unwrap();
    w.index_bytes("o", "r", "a.rs", b"fn alpha() {}", None)
        .unwrap();
    let addr = ts.running().unwrap().mcp_addr.unwrap();
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0],
                   "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
    })
    .to_string();
    let post = |hdrs: &[(&str, &str)]| {
        let mut h = vec![("Content-Type", "application/json")];
        h.extend_from_slice(hdrs);
        http(addr, "POST", "/mcp", &h, init.as_bytes()).unwrap()
    };
    let r = post(&[]);
    assert_eq!(r.status, 401, "{r:?}");
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let wrong = format!(
        "Bearer {}",
        graph_server::auth::new_credential("mgt_").as_str()
    );
    assert_eq!(post(&[("Authorization", &wrong)]).status, 401);
    let secret = format!("Bearer {}", f.secret);
    assert_eq!(
        post(&[("Authorization", &secret)]).status,
        401,
        "never the secret"
    );
    // Origin and Host are still checked with a valid token.
    let good = format!("Bearer {}", f.read);
    assert_eq!(
        post(&[("Authorization", &good), ("Origin", "http://evil.example")]).status,
        403
    );
    assert_eq!(
        post(&[("Authorization", &good), ("Host", "evil.example")]).status,
        403
    );
    // A read token searches.
    let mut c = McpHttpClient::with_token(addr, Some(&f.read));
    let v = c.request(
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"text": "alpha"}}),
    );
    assert!(v["result"].is_object(), "{v}");
    assert!(v.to_string().contains("alpha"), "{v}");
    // Every tool today needs `read`; story 33's write tools will need
    // `write` (and a read token then gets 403).
    assert_eq!(
        graph_server::mcp::tool_need("search"),
        graph_server::Role::Read
    );
    assert!(!graph_server::Role::Read.covers(graph_server::Role::Write));
}

// ---------------------------------------------------------------------------
// A cluster with auth: forwarding and the Raft path.

fn auth_cluster(f: &AuthFixture, per_node: impl Fn(u64) -> std::path::PathBuf) -> ClusterTestbed {
    let mut tb = ClusterTestbed::with_config(3, rust(), |id, c| {
        c.auth = f.config();
        c.auth.tokens = Some(per_node(id));
    });
    tb.set_client_credentials(Some(f.creds(&f.write)));
    tb.form();
    tb
}

fn client_of(tb: &ClusterTestbed, id: u64, token: &str, f: &AuthFixture) -> RemoteStore {
    let mut cfg = ClientConfig::new(tb.node(id).endpoint());
    cfg.write_deadline = Duration::from_secs(15);
    cfg.credentials = Some(f.creds(token));
    RemoteStore::connect(cfg).unwrap()
}

#[test]
fn forwarding_carries_the_role_and_never_the_token() {
    let f = AuthFixture::new();
    // Rotation skew: `write2` is only on the followers' files.
    let leader_file = f.dir.path().join("leader.toml");
    std::fs::write(
        &leader_file,
        [
            stanza("w", "write", &f.write, None),
            stanza("r", "read", &f.read, None),
        ]
        .join("\n"),
    )
    .unwrap();
    let tb = auth_cluster(&f, |id| {
        if id == 1 {
            leader_file.clone()
        } else {
            f.tokens.clone()
        }
    });
    let leader = tb.leader();
    assert_eq!(leader, 1, "node 1 bootstrapped and leads");
    let follower = 2;
    // A write token's write through a follower is applied by the leader.
    let w = client_of(&tb, follower, &f.write, &f);
    w.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    // A token only the follower knows is honoured for a forwarded write.
    let w2 = client_of(&tb, follower, &f.write2, &f);
    w2.index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap();
    // ... and refused by the leader itself.
    assert_eq!(
        raw(
            &tb.node(leader).endpoint(),
            "/memory_graph.v1.Admin/Status",
            &[bearer(&f.write2)]
        ),
        Code::Unauthenticated
    );
    // A read token is refused at the follower, before any forward.
    let leader_role_refusals = || {
        tb.node(leader)
            .running()
            .unwrap()
            .auth()
            .refusal_counts()
            .into_iter()
            .find(|(r, _)| *r == "role")
            .unwrap()
            .1
    };
    let before = leader_role_refusals();
    let r = client_of(&tb, follower, &f.read, &f);
    let e = r
        .index_bytes("o", "r", "c.rs", b"fn c() {}", None)
        .unwrap_err();
    assert!(matches!(e, StoreError::PermissionDenied { .. }), "{e:?}");
    assert_eq!(leader_role_refusals(), before, "refused at the edge");
    // A forwarded linearizable read barrier needs only `read`.
    let mut cfg = ClientConfig::new(tb.node(follower).endpoint());
    cfg.credentials = Some(f.creds(&f.read));
    cfg.read_mode = graph_client::ReadMode::Linearizable;
    let lin = RemoteStore::connect(cfg).unwrap();
    assert!(lin.count_nodes(NodeKind::File).unwrap() >= 2);
    // A forwarded membership change needs `write`.
    let e = r.admin_promote(3).unwrap_err();
    assert!(matches!(e, StoreError::PermissionDenied { .. }), "{e:?}");
    // Spoofed forwarding headers.
    let spoof = |t: Option<&str>| {
        let mut md = vec![
            ("mg-forwarded-by", "2".to_string()),
            ("mg-forwarded-role", "write".to_string()),
        ];
        if let Some(t) = t {
            md.push(bearer(t));
        }
        raw(
            &tb.node(leader).endpoint(),
            "/memory_graph.v1.Write/Prune",
            &md,
        )
    };
    assert_eq!(spoof(Some(&f.read)), Code::PermissionDenied);
    assert_eq!(spoof(None), Code::Unauthenticated);
    // A wrong cluster secret cannot vote, append, install, join or move.
    let rogue = graph_server::auth::new_credential("mgc_").to_string();
    for p in [
        "/memory_graph.v1.Raft/Vote",
        "/memory_graph.v1.Raft/AppendEntries",
        "/memory_graph.v1.Raft/InstallSnapshot",
        "/memory_graph.v1.Admin/Join",
        "/memory_graph.v1.Admin/UpdateAdvertise",
    ] {
        assert_eq!(
            raw(&tb.node(leader).endpoint(), p, &[bearer(&rogue)]),
            Code::Unauthenticated,
            "{p}"
        );
    }
    // Replication worked throughout (the Raft path carries the secret).
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
}

/// Two-line secret rotation across three nodes: the cluster stays
/// available at every step.
#[test]
fn cluster_secret_rotation_keeps_the_cluster_available() {
    let f = AuthFixture::new();
    let mut tb = auth_cluster(&f, |_| f.tokens.clone());
    let next = graph_server::auth::new_credential("mgc_").to_string();
    let step = |tb: &mut ClusterTestbed, text: String, n: &str| {
        std::fs::write(&f.secret_file, text).unwrap();
        for id in tb.ids() {
            tb.node(id).running().unwrap().auth().reload_now();
        }
        let c = tb.client(tb.leader());
        c.index_bytes("o", "r", &format!("{n}.rs"), b"fn x() {}", None)
            .unwrap_or_else(|e| panic!("step {n}: {e}"));
        tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
    };
    step(&mut tb, format!("{}\n{next}\n", f.secret), "add");
    step(&mut tb, format!("{next}\n{}\n", f.secret), "swap");
    step(&mut tb, format!("{next}\n"), "drop");
    // Restarting a follower: it rejoins with the new secret.
    tb.node_mut(3).stop();
    tb.node_mut(3).restart();
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
}

/// Every internal client of a node sends the cluster secret (D3): the
/// source files that build a client to another node are listed, and each
/// stamps the secret.
#[test]
fn every_internal_client_sends_the_cluster_secret() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for entry in walk(&src) {
        let text = std::fs::read_to_string(&entry).unwrap();
        if entry.components().any(|c| c.as_os_str() == "testing") {
            continue;
        }
        if text.contains("Endpoint::from_shared") {
            found.push(
                entry
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            assert!(
                text.contains("stamp_secret")
                    || text.contains("NodeHeaders")
                    || text.contains("HealthClient"),
                "{} builds a client to another node without the cluster secret",
                entry.display()
            );
        }
    }
    found.sort();
    assert_eq!(
        found,
        [
            "forward.rs",
            "join.rs",
            "raft/network.rs",
            "services/admin.rs"
        ],
        "a new internal client: make it send the cluster secret, then list it here"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
