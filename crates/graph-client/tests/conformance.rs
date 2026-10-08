//! `RemoteStore` against an in-process `TestServer` (ADR 0004 stage A test
//! plan): the store conformance suite, the two differential harnesses
//! against an embedded store, a server restart mid-batch, snapshot handle
//! expiry and cap, default-limit paging, typed errors through a real RPC,
//! protocol version refusal, and two servers on one file.
use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::Extractor;
use graph_proto::{pb, status_to_store_error, PROTOCOL_VERSION};
use graph_server::testing::TestServer;
use graph_store::conformance::{self, Harness};
use graph_store::{
    open_store, BatchFile, IndexOptions, Query, Store, StoreError, StoreRead, ORIGIN_DIRECTORY,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn connect(server: &TestServer) -> RemoteStore {
    RemoteStore::connect(ClientConfig::new(server.endpoint())).expect("connect")
}

fn connect_mode(server: &TestServer, mode: ReadMode) -> RemoteStore {
    let mut cfg = ClientConfig::new(server.endpoint());
    cfg.read_mode = mode;
    RemoteStore::connect(cfg).expect("connect")
}

/// A harness whose `open` (re)starts one server on the same file with the
/// call's extractors and connects a fresh `RemoteStore` to it.
fn remote_harness() -> Harness {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let server: Arc<Mutex<Option<TestServer>>> = Arc::new(Mutex::new(None));
    let s2 = Arc::clone(&server);
    Harness {
        open: Box::new(move |ex| {
            let mut g = s2.lock().unwrap();
            if let Some(mut old) = g.take() {
                old.stop();
            }
            let ts = TestServer::start(&db, ex);
            let store = RemoteStore::connect(ClientConfig::new(ts.endpoint()))?;
            *g = Some(ts);
            Ok(Box::new(store) as Box<dyn Store>)
        }),
        exclusive: false,
        accepts_remote_prepared: true,
        guard: Some(Box::new((d, server))),
    }
}

/// ADR 0007 C8: a current server decodes with the hint, so the client's
/// old-server warning stays off (the threshold is the schema that added it).
#[test]
fn current_server_honours_encoding_hints() {
    assert_eq!(graph_client::ENCODING_STORE_FORMAT, 11);
    let d = tempfile::tempdir().unwrap();
    let ts = TestServer::start(&d.path().join("g.redb"), rust());
    assert!(!connect(&ts).ignores_encoding_hints());
}

#[test]
fn remote_store_passes_conformance_suite() {
    conformance::run_all(&remote_harness);
}

#[test]
fn remote_matches_embedded_differential() {
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let remote = connect(&server);
    conformance::run_differential(&*embedded, &remote);
}

/// ADR 0010 D3: a server from before `name_pos` (simulated by a hook that
/// strips the field from its answers) leaves it unset, and the client falls
/// back to the span start; a current server sends the declaration line.
#[test]
fn server_without_name_pos_falls_back_to_span_start() {
    use graph_core::{Extraction, Span, SymbolDecl, SymbolKind};
    use graph_store::{Grain, Position, SymbolQuery};
    let src = "[Serializable]\npublic class Shape { }\n";
    let decl = src.trim_end();
    let span = Span {
        start: 0,
        end: decl.len() as u32,
        start_line: 1,
        start_col: 1,
        end_line: 2,
        end_col: 23,
    };
    let ex = Extraction {
        has_errors: false,
        symbols: vec![SymbolDecl {
            owner: None,
            name: "Shape".into(),
            kind: SymbolKind::Type,
            lang_kind: None,
            span,
        }],
        tokens: graph_core::tokenizer::tokenize(src),
    };
    let start = Some(Position::start_of(&span));
    for omit in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let ts = TestServer::start_with(&d.path().join("s.redb"), vec![], |cfg| {
            cfg.testing.omit_name_pos = omit;
        });
        let remote = connect(&ts);
        remote
            .ingest_file("o", "r", "attr.cs", "csharp", &ex)
            .unwrap();
        let hits = remote.search_symbols(&SymbolQuery::new("Shape")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].span, Some(span));
        let mut q = Query::new("Serializable");
        q.grain = Grain::Class;
        let rows = remote.search(&q).unwrap();
        assert_eq!(rows.len(), 1);
        if omit {
            assert_eq!(hits[0].name_pos, start, "old server: span start");
            assert_eq!(rows[0].name_pos, start, "old server: span start");
        } else {
            let want = Some(Position {
                byte: 28,
                line: 2,
                col: 14,
            });
            assert_eq!(hits[0].name_pos, want);
            assert_eq!(rows[0].name_pos, want);
        }
    }
}

/// #203: a file whose symbols fail span validation is stored tokens-only by
/// the server exactly as by an embedded store, warning included.
#[test]
fn remote_matches_embedded_for_degraded_files() {
    let d = tempfile::tempdir().unwrap();
    let ex = conformance::invalid_symbol_span_extractors;
    let embedded = open_store(&d.path().join("e.redb"), ex()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), ex());
    let remote = connect(&server);
    conformance::run_invalid_symbol_span_differential(&*embedded, &remote);
}

/// #120: a Windows client sends `\` paths to a server; the server's answers
/// equal an embedded index of the same tree with `/` paths (no
/// `dir\.gitignore` read as language `gitignore` on a Linux host).
#[test]
fn backslash_batch_through_server_equals_embedded() {
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let remote = connect(&server);
    let files = [
        ("dir/.gitignore", &b"target\nfoo\n"[..]),
        ("src/lib.rs", &b"fn foo() { bar(); }\n"[..]),
        ("src/deep/x.txt", &b"foo bar\n"[..]),
    ];
    let batch = |sep: &str| -> Vec<(String, &[u8])> {
        files
            .iter()
            .map(|(p, b)| (p.replace('/', sep), *b))
            .collect()
    };
    let run = |s: &dyn Store, sep: &str| {
        let owned = batch(sep);
        let fs: Vec<_> = owned
            .iter()
            .map(|(p, b)| BatchFile {
                path: p,
                bytes: b,
                language: None,
                origin: Some(ORIGIN_DIRECTORY),
                ..Default::default()
            })
            .collect();
        for r in s
            .index_batch("o", "r", &fs, IndexOptions::default())
            .unwrap()
        {
            r.unwrap();
        }
    };
    run(&*embedded, "/");
    run(&remote, "\\");
    let describe = |s: &dyn Store| format!("{:?}", s.describe(None, None).unwrap());
    assert_eq!(describe(&*embedded), describe(&remote));
    assert!(
        !describe(&remote).contains("gitignore"),
        "{}",
        describe(&remote)
    );
    let hits = |s: &dyn Store| {
        let mut v: Vec<_> = s
            .search(&Query::new("foo"))
            .unwrap()
            .into_iter()
            .map(|h| (h.file, h.language, h.symbol))
            .collect();
        v.sort();
        v
    };
    assert_eq!(hits(&*embedded), hits(&remote));
    assert_eq!(hits(&remote).len(), 3);
}

#[test]
fn remote_matches_embedded_differential_linearizable_reads() {
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let remote = connect_mode(&server, ReadMode::Linearizable);
    conformance::run_differential(&*embedded, &remote);
}

#[test]
fn crash_rerun_differential_remote_vs_embedded() {
    let d = tempfile::tempdir().unwrap();
    let fresh = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let crashed = connect(&server);
    conformance::run_crash_rerun_differential(&*fresh, &crashed);

    let d = tempfile::tempdir().unwrap();
    let crashed = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let fresh = connect(&server);
    conformance::run_crash_rerun_differential(&fresh, &*crashed);
}

/// The poisoned batch (a NUL in a language) fails on the server, the server
/// is restarted (the store closes and reopens, the Raft log is replayed),
/// the good batch is re-run through the same client, and the result equals
/// a fresh embedded index. Also: the served file reopened embedded answers
/// identically.
#[test]
fn server_restart_mid_batch_equals_fresh() {
    let d = tempfile::tempdir().unwrap();
    let fresh = open_store(&d.path().join("e.redb"), rust()).unwrap();
    let served = d.path().join("s.redb");
    let mut server = TestServer::start(&served, rust());
    let remote = connect(&server);

    let names = ["a.txt", "b.txt", "c.txt", "d.txt"];
    let bodies: [&[u8]; 4] = [
        b"foo bar",
        b"foo (bar) qux",
        b"let x = foo;",
        b"fn dup() { dup(); }",
    ];
    let good: Vec<BatchFile<'_>> = names
        .iter()
        .zip(bodies)
        .map(|(n, b)| BatchFile {
            path: n,
            bytes: b,
            language: Some("text"),
            origin: Some(ORIGIN_DIRECTORY),
            ..Default::default()
        })
        .collect();
    let mut poisoned = good[..2].to_vec();
    poisoned.push(BatchFile {
        path: "nul.txt",
        bytes: b"foo",
        language: Some("a\0b"),
        origin: Some(ORIGIN_DIRECTORY),
        ..Default::default()
    });
    poisoned.extend_from_slice(&good[2..]);
    let err = remote
        .index_batch("o", "r", &poisoned, IndexOptions::default())
        .expect_err("the poisoned batch must fail");
    assert!(matches!(err, StoreError::Rejected(_)), "{err:?}");

    server.restart();
    assert!(server.is_running());

    let rerun = remote
        .index_batch("o", "r", &good, IndexOptions::default())
        .unwrap();
    assert!(rerun.iter().all(|r| r.is_ok()), "{rerun:?}");
    let once = fresh
        .index_batch("o", "r", &good, IndexOptions::default())
        .unwrap();
    assert!(once.iter().all(|r| r.is_ok()));
    conformance::run_differential(&*fresh, &remote);

    // The served file, reopened embedded, answers the same.
    drop(remote);
    server.stop();
    let reopened = open_store(&served, rust()).unwrap();
    conformance::run_differential(&*fresh, &*reopened);
}

#[test]
fn snapshot_handle_expires_at_the_max_age() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start_with(&d.path().join("s.redb"), vec![], |cfg| {
        cfg.snapshot_max_age = Duration::from_millis(600);
    });
    let remote = connect(&server);
    remote.index_bytes("o", "r", "a.txt", b"foo", None).unwrap();
    let snap = remote.snapshot().unwrap();
    assert_eq!(snap.search(&Query::new("foo")).unwrap().len(), 1);
    std::thread::sleep(Duration::from_millis(900));
    let err = snap.search(&Query::new("foo")).unwrap_err();
    assert!(matches!(err, StoreError::SnapshotExpired { .. }), "{err:?}");
    // A fresh handle works.
    let snap2 = remote.snapshot().unwrap();
    assert_eq!(snap2.search(&Query::new("foo")).unwrap().len(), 1);
}

#[test]
fn sixty_fifth_snapshot_handle_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let remote = connect(&server);
    remote.index_bytes("o", "r", "a.txt", b"foo", None).unwrap();
    let mut held = Vec::new();
    for _ in 0..graph_server::SNAPSHOT_HANDLES_PER_CONNECTION {
        held.push(remote.snapshot().unwrap());
    }
    let err = remote.snapshot().err().expect("65th handle refused");
    assert!(
        matches!(&err, StoreError::Rejected(m) if m.contains("64")),
        "{err:?}"
    );
    // Closing one frees a slot.
    drop(held.pop());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match remote.snapshot() {
            Ok(s) => {
                held.push(s);
                break;
            }
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("slot not freed: {e}"),
        }
    }
    assert_eq!(remote.snapshot_stats().open_count, 64);
    drop(held);
}

/// A search without a limit returns every hit (>1000), paged under one
/// snapshot handle, and equals the embedded answer.
#[test]
fn default_limit_paging_returns_everything() {
    let d = tempfile::tempdir().unwrap();
    let embedded = open_store(&d.path().join("e.redb"), vec![]).unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let remote = connect(&server);
    let src: String = (0..2500).map(|i| format!("foo v{i}\n")).collect();
    for s in [&*embedded, &remote as &dyn Store] {
        s.index_bytes("o", "r", "big.txt", src.as_bytes(), Some("text"))
            .unwrap();
    }
    let q = Query::new("foo");
    let e = embedded.search(&q).unwrap();
    let r = remote.search(&q).unwrap();
    assert_eq!(e.len(), 2500);
    assert_eq!(e, r);
    // With an offset and no limit: the tail.
    let mut q2 = Query::new("foo");
    q2.offset = Some(1234);
    assert_eq!(embedded.search(&q2).unwrap(), remote.search(&q2).unwrap());
    assert_eq!(remote.search(&q2).unwrap().len(), 2500 - 1234);
    // An explicit limit above the default is honoured as given.
    let mut q3 = Query::new("foo");
    q3.limit = Some(1500);
    assert_eq!(remote.search(&q3).unwrap().len(), 1500);
    // Under a snapshot handle, the same.
    let snap = remote.snapshot().unwrap();
    assert_eq!(snap.search(&q).unwrap().len(), 2500);
    assert_eq!(
        remote.snapshot_stats().open_count,
        1,
        "paging closed its own handle"
    );
}

#[test]
fn store_errors_round_trip_through_a_real_rpc() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let remote = connect(&server);
    // Per-file rejection in its slot.
    let out = remote
        .index_batch(
            "o",
            "r",
            &[BatchFile {
                path: "bin.c",
                bytes: b"\x00\xff\x00",
                language: None,
                origin: None,
                ..Default::default()
            }],
            IndexOptions::default(),
        )
        .unwrap();
    assert!(
        matches!(&out[0], Err(StoreError::Binary(m)) if m.contains("bin.c")),
        "{out:?}"
    );
    // ADR 0007: the single-file RPC (`index-file --server`) refuses a
    // binary file too, and a strict `utf-8` hint gives today's `NotUtf8`.
    let err = remote
        .index_bytes("o", "r", "img.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", None)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::Binary(m) if m.contains("img.png")),
        "{err:?}"
    );
    let strict = IndexOptions {
        encoding: Some(graph_core::encoding::Encoding::for_label(b"utf-8").unwrap()),
        strict_encoding: true,
        ..Default::default()
    };
    let err = remote
        .index_bytes_opts("o", "r", "bad.txt", b"a \xff b", None, None, strict)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::NotUtf8(m) if m.contains("bad.txt")),
        "{err:?}"
    );
    // A whole-call refusal.
    let err = remote
        .index_bytes("", "r", "a.txt", b"foo", None)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::Rejected(m) if m.contains("org and repo")),
        "{err:?}"
    );
    // An invalid span in a caller-supplied extraction.
    let ex = graph_core::Extraction {
        symbols: vec![],
        tokens: vec![graph_core::TokenDecl {
            text: "x".into(),
            class: graph_core::TokenClass::Identifier,
            span: graph_core::Span {
                start: 5,
                end: 2,
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 1,
            },
        }],
        has_errors: false,
    };
    let err = remote
        .ingest_file("o", "r", "x.txt", "text", &ex)
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSpan(_)), "{err:?}");
}

/// The server normalizes on receipt, not only the Rust client: raw
/// `IndexFile`, `IngestExtraction` and `Prune` requests carrying `\` paths
/// (as another client might send) are stored and matched as `/` (#120).
#[test]
fn server_normalizes_raw_backslash_paths() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", server.endpoint()))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut w = pb::write_client::WriteClient::new(ch);
        w.index_file(pb::IndexFileRequest {
            org: "o".into(),
            repo: "r".into(),
            file: Some(pb::FileBytes {
                path: r"dir\.gitignore".into(),
                bytes: b"foo\n".to_vec(),
                language: None,
                origin: Some(ORIGIN_DIRECTORY.into()),
                ..Default::default()
            }),
            options: Some(IndexOptions::default().into()),
        })
        .await
        .unwrap();
        let ex = graph_core::Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: graph_core::tokenizer::tokenize("foo"),
        };
        w.ingest_extraction(pb::IngestExtractionRequest {
            org: "o".into(),
            repo: "r".into(),
            path: r"src\x.txt".into(),
            language: "text".into(),
            extraction: Some(ex.into()),
            origin: Some(ORIGIN_DIRECTORY.into()),
        })
        .await
        .unwrap();
        let removed = w
            .prune(pb::PruneRequest {
                org: "o".into(),
                repo: "r".into(),
                keep: vec![r"dir\.gitignore".into(), r"src\x.txt".into()],
                dry_run: true,
            })
            .await
            .unwrap()
            .into_inner()
            .removed;
        assert!(removed.is_empty(), "{removed:?}");
    });
    let remote = connect(&server);
    for f in ["dir/.gitignore", "src/x.txt"] {
        assert!(remote.file_tokens("o", "r", f).unwrap().is_some(), "{f}");
    }
    let info = format!("{:?}", remote.describe(None, None).unwrap());
    assert!(!info.contains("gitignore"), "{info}");
    let mut files: Vec<_> = remote
        .search(&Query::new("foo"))
        .unwrap()
        .into_iter()
        .filter_map(|h| h.file)
        .collect();
    files.sort();
    assert_eq!(files, ["dir/.gitignore", "src/x.txt"]);
}

#[test]
fn unknown_protocol_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let status = rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", server.endpoint()))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut c = pb::store_client::StoreClient::new(ch);
        c.hello(pb::HelloRequest {
            protocol_version: PROTOCOL_VERSION + 1,
            client_version: "test".into(),
        })
        .await
        .unwrap_err()
    });
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    let e = status_to_store_error(&status);
    assert!(
        matches!(&e, StoreError::Protocol(m) if m.contains("protocol version")),
        "{e:?}"
    );
    // The right version is welcome, and says who answered.
    let remote = connect(&server);
    let h = remote.hello();
    assert_eq!(h.protocol_version, PROTOCOL_VERSION);
    assert_eq!(h.node_id, 1);
    assert_eq!(h.leader_id, Some(1));
    assert_eq!(h.store_format_version, graph_store::SCHEMA_VERSION);
    assert_eq!(h.extractors_hash.len(), 64);
}

#[test]
fn two_servers_on_one_file_the_second_is_locked() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("s.redb");
    let first = TestServer::start(&db, vec![]);
    let err = TestServer::try_start_with(&db, vec![], |_| {})
        .err()
        .expect("second refused");
    assert!(matches!(err, StoreError::Locked(_)), "{err:?}");
    // An embedded open is refused too, and the sidecar names the holder.
    let err = open_store(&db, vec![])
        .err()
        .expect("embedded open refused");
    assert!(matches!(err, StoreError::Locked(_)), "{err:?}");
    let (info, alive) = graph_server::lock::holder(&db).expect("LOCK sidecar");
    assert!(alive);
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.listen, first.endpoint());
    drop(first);
    assert!(
        !graph_server::lock::lock_path(&db).exists(),
        "sidecar removed on stop"
    );
    assert!(
        open_store(&db, vec![]).is_ok(),
        "free once the server stopped"
    );
}

#[test]
fn admin_and_health_surface() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), rust());
    let remote = connect(&server);
    assert!(remote.health("").unwrap());
    assert!(remote.health(graph_server::READY_SERVICE).unwrap());
    remote
        .index_bytes("o", "r", "lib.rs", b"fn a() { foo(); }", None)
        .unwrap();
    let st = remote.admin_status().unwrap();
    assert_eq!(st.node_id, 1);
    assert_eq!(st.leader_id, Some(1));
    assert!(st.applied_index >= 1, "{st:?}");
    assert_eq!(st.state, "Leader");
    assert_eq!(st.protocol_version, PROTOCOL_VERSION);
    let json: serde_json::Value = serde_json::from_str(&remote.admin_sysinfo().unwrap()).unwrap();
    assert!(json.get("db").is_some());
    let before = remote.search(&Query::new("foo")).unwrap();
    let stats = remote.admin_compact().unwrap();
    assert!(stats.after_bytes > 0);
    assert_eq!(
        remote.search(&Query::new("foo")).unwrap(),
        before,
        "compact keeps the data"
    );
    assert_eq!(remote.count_nodes(graph_core::NodeKind::File).unwrap(), 1);
    // Shutdown over Admin stops the server: the LOCK sidecar goes away.
    remote.admin_shutdown(Duration::from_secs(1)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while graph_server::lock::read(server.db()).is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        graph_server::lock::read(server.db()).is_none(),
        "LOCK removed after shutdown"
    );
}

#[test]
fn a_second_client_reads_while_another_indexes() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let writer = connect(&server);
    let reader = connect(&server);
    let src: String = (0..20000).map(|i| format!("foo w{i}\n")).collect();
    let files: Vec<(String, Vec<u8>)> = (0..8)
        .map(|i| (format!("f{i}.txt"), src.as_bytes().to_vec()))
        .collect();
    let t = std::thread::spawn(move || {
        let batch: Vec<BatchFile<'_>> = files
            .iter()
            .map(|(p, b)| BatchFile {
                path: p,
                bytes: b,
                language: Some("text"),
                origin: None,
                ..Default::default()
            })
            .collect();
        writer
            .index_batch("o", "r", &batch, IndexOptions::default())
            .unwrap()
    });
    let mut seen = Vec::new();
    while !t.is_finished() {
        seen.push(reader.count_nodes(graph_core::NodeKind::File).unwrap());
    }
    let out = t.join().unwrap();
    assert!(out.iter().all(|r| r.is_ok()));
    assert_eq!(reader.count_nodes(graph_core::NodeKind::File).unwrap(), 8);
    assert!(!seen.is_empty(), "reads were answered during the index run");
}

/// ADR 0007 C2, C8: a raw `Index` / `IndexFile` RPC (as another client might
/// send) decodes through the server's `prepare_file` with the hint it
/// carries, exactly as `index_bytes_opts` through `RemoteStore` and the
/// embedded store do: same encoding, fingerprint and tokens. A hint that is
/// not a usable label is refused (a malformed message) before anything is
/// written.
#[test]
fn raw_index_rpc_honours_the_encoding_hint() {
    let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("CustomerId \u{65e5}\u{672c}\n");
    let sjis = sjis.into_owned();
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("s.redb"), vec![]);
    let remote = connect(&server);
    let embedded = graph_store::open_store(&d.path().join("e.redb"), vec![]).unwrap();
    let opts = IndexOptions {
        encoding: Some(encoding_rs::SHIFT_JIS),
        ..Default::default()
    };
    let via_trait = |s: &dyn Store, path: &str| {
        let st = s
            .index_bytes_opts("o", "r", path, &sjis, None, None, opts)
            .unwrap();
        s.get(st.file_id).unwrap().unwrap()
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let file = |path: &str, hint: &str| pb::FileBytes {
        path: path.into(),
        bytes: sjis.clone(),
        encoding_hint: Some(hint.into()),
        ..Default::default()
    };
    rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", server.endpoint()))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut w = pb::write_client::WriteClient::new(ch);
        w.index_file(pb::IndexFileRequest {
            org: "o".into(),
            repo: "r".into(),
            file: Some(file("raw1.txt", "sjis")),
            options: None,
        })
        .await
        .unwrap();
        let msgs = vec![
            pb::IndexRequest {
                msg: Some(pb::index_request::Msg::Header(pb::IndexHeader {
                    org: "o".into(),
                    repo: "r".into(),
                    options: None,
                })),
            },
            pb::IndexRequest {
                msg: Some(pb::index_request::Msg::File(file("raw2.txt", "shift_jis"))),
            },
        ];
        w.index(tokio_stream::iter(msgs)).await.unwrap();
        for bad in ["no-such-encoding", "replacement"] {
            let err = w
                .index_file(pb::IndexFileRequest {
                    org: "o".into(),
                    repo: "r".into(),
                    file: Some(file("bad.txt", bad)),
                    options: None,
                })
                .await
                .unwrap_err();
            // A malformed message, like every other `ConvertError`.
            assert_eq!(
                err.code(),
                tonic::Code::FailedPrecondition,
                "{bad}: {err:?}"
            );
            assert!(err.message().contains("encoding_hint"), "{err:?}");
        }
    });
    assert!(remote.file_tokens("o", "r", "bad.txt").unwrap().is_none());
    let want = via_trait(&*embedded, "e.txt");
    assert_eq!(want.encoding.as_deref(), Some("Shift_JIS"));
    let toks = |s: &dyn Store, p: &str| {
        s.file_tokens("o", "r", p)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|n| (n.name, n.span))
            .collect::<Vec<_>>()
    };
    let want_toks = toks(&*embedded, "e.txt");
    assert_eq!(want_toks[1].0, "\u{65e5}\u{672c}");
    let remote_node = via_trait(&remote, "t.txt");
    for (path, n) in [
        ("t.txt", remote_node),
        (
            "raw1.txt",
            remote
                .get(toks_parent(&remote, "raw1.txt"))
                .unwrap()
                .unwrap(),
        ),
        (
            "raw2.txt",
            remote
                .get(toks_parent(&remote, "raw2.txt"))
                .unwrap()
                .unwrap(),
        ),
    ] {
        assert_eq!(n.encoding, want.encoding, "{path}");
        assert_eq!(n.lossy, want.lossy, "{path}");
        assert_eq!(n.fingerprint, want.fingerprint, "{path}");
        assert_eq!(toks(&remote, path), want_toks, "{path}");
    }
}

fn toks_parent(s: &dyn Store, path: &str) -> graph_core::NodeId {
    s.file_tokens("o", "r", path).unwrap().unwrap()[0]
        .parent
        .unwrap()
}
