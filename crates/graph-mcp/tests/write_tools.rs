//! The opt-in write tools (epic story 33): off by default (not listed, an
//! unknown tool), and when on, a differential against the `Store` they
//! write to: `index_path` through MCP leaves the store exactly as
//! `Store::index_batch` of the same files does, and `prune` answers and
//! removes exactly what `Store::prune_files` does. Run against an embedded
//! `V2Store` and against a `RemoteStore` over a `TestServer`; every answer
//! is checked against its `outputSchema` (`common::Client`).
mod common;

use common::{Client, Tools};
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_mcp::{StoreBackend, StoreWriter, WriteAccess};
use graph_server::testing::TestServer;
use graph_store::{open_store, BatchFile, IndexOptions, Query, Store, StoreRead, SymbolQuery};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// The tree under `root/proj` the tests index: what `index_path` should
/// store (path, bytes), plus an ignored file and a `.git` directory it
/// must leave out.
fn make_tree(root: &Path) -> Vec<(&'static str, &'static [u8])> {
    let files: Vec<(&'static str, &'static [u8])> = vec![
        (".gitignore", b"ignored.rs\n"),
        ("README.md", b"Point notes\n"),
        (
            "src/lib.rs",
            b"pub struct Point { pub x: i32 }\npub fn origin() -> Point { Point { x: 0 } }\n",
        ),
        (
            "src/util/shift.rs",
            b"use crate::Point;\npub fn shift(p: Point) -> Point { p }\n",
        ),
        ("blob.bin", b"\x00\x01\x02binary\x00"),
    ];
    let proj = root.join("proj");
    for (p, b) in &files {
        let f = proj.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b).unwrap();
    }
    std::fs::write(proj.join("ignored.rs"), b"fn hidden() {}\n").unwrap();
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::write(proj.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    files
}

/// The oracle: the same files, straight into `store` with `index_batch`
/// (origin `directory`, as a directory run).
fn oracle_index(store: &dyn Store, files: &[(&'static str, &'static [u8])]) -> usize {
    let batch: Vec<BatchFile<'_>> = files
        .iter()
        .map(|(p, b)| BatchFile {
            path: p,
            bytes: b,
            language: None,
            origin: Some("directory"),
            encoding: None,
            strict_encoding: false,
        })
        .collect();
    let r = store
        .index_batch("acme", "proj", &batch, IndexOptions::default())
        .unwrap();
    r.iter().filter(|x| x.is_err()).count()
}

/// Everything a reader can see of the store, as JSON.
fn state(s: &dyn StoreRead) -> Value {
    let mut files = Vec::new();
    for o in s.roots().unwrap() {
        for r in s.children(o.id).unwrap() {
            for f in s.children(r.id).unwrap() {
                let toks = s.file_tokens(&o.name, &r.name, &f.name).unwrap();
                files.push(json!({
                    "org": o.name, "repo": r.name, "path": f.name, "language": f.language,
                    "origin": f.origin,
                    "tokens": toks.map(|t| t.iter().map(|n| (n.name.clone(), n.span)).collect::<Vec<_>>()),
                }));
            }
        }
    }
    let syms = s.search_symbols(&SymbolQuery::new("*")).unwrap();
    let hits = s.search(&Query::new("Point")).unwrap();
    json!({
        "describe": serde_json::to_value(s.describe(None, None).unwrap()).unwrap(),
        "files": files,
        "symbols": serde_json::to_value(syms).unwrap(),
        "hits": serde_json::to_value(hits).unwrap(),
    })
}

/// The differential, for a store under test (`mcp`, written through the
/// tools) and an oracle store of the same kind.
fn differential(mcp: Arc<dyn Store>, oracle: Arc<dyn Store>, tmp: &Path) {
    let files = make_tree(tmp);
    let access = WriteAccess::new(tmp, Box::new(StoreWriter(Arc::clone(&mcp)))).unwrap();
    let mut c = Client::new(StoreBackend::embedded(Box::new(Arc::clone(&mcp))).with_writes(access));

    let listed = c.request("tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.ends_with(&["index_path", "prune"]), "{names:?}");

    let failed = oracle_index(&*oracle, &files);
    let v = c.ok(
        "index_path",
        json!({"org": "acme", "repo": "proj", "path": "proj"}),
    );
    assert_eq!(v["path"], "proj");
    assert_eq!(v["failed_count"], failed, "{v}");
    assert_eq!(v["indexed"], files.len() - failed, "{v}");
    assert_eq!(v["unchanged"], 0, "{v}");
    assert_eq!(v["failed"][0]["path"], "blob.bin", "{v}");
    assert_eq!(v["failed"][0]["code"], "binary", "{v}");
    assert_eq!(state(&*mcp), state(&*oracle), "index_path == index_batch");
    let mcp_syms = mcp.search_symbols(&SymbolQuery::new("*")).unwrap();
    assert_eq!(v["symbols"], mcp_syms.len(), "{v}");

    // Again: every file is unchanged, by fingerprint.
    let again = c.ok(
        "index_path",
        json!({"org": "acme", "repo": "proj", "path": "proj"}),
    );
    assert_eq!(again["indexed"], 0, "{again}");
    assert_eq!(again["unchanged"], files.len() - failed, "{again}");
    // reindex re-parses them; the state does not move.
    let re = c.ok(
        "index_path",
        json!({"org": "acme", "repo": "proj", "path": "proj", "reindex": true}),
    );
    assert_eq!(re["indexed"], files.len() - failed, "{re}");
    assert_eq!(state(&*mcp), state(&*oracle));

    // A file goes away: prune (dry run by default) names what
    // `prune_files` names, removes nothing, then removes it.
    std::fs::remove_file(tmp.join("proj/src/util/shift.rs")).unwrap();
    let keep: HashSet<String> = files
        .iter()
        .map(|(p, _)| p.to_string())
        .filter(|p| p != "src/util/shift.rs")
        .collect();
    let dry = c.ok(
        "prune",
        json!({"org": "acme", "repo": "proj", "path": "proj"}),
    );
    let expect = oracle.prune_files("acme", "proj", &keep, true).unwrap();
    assert_eq!(expect, vec!["src/util/shift.rs".to_string()]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["removed"], json!(expect), "{dry}");
    assert_eq!(dry["removed_count"], 1);
    assert_eq!(state(&*mcp), state(&*oracle), "a dry run removes nothing");
    let wet = c.ok(
        "prune",
        json!({"org": "acme", "repo": "proj", "path": "proj", "dry_run": false}),
    );
    let removed = oracle.prune_files("acme", "proj", &keep, false).unwrap();
    assert_eq!(wet["removed"], json!(removed), "{wet}");
    assert_eq!(state(&*mcp), state(&*oracle), "prune == prune_files");
    let none = c.ok(
        "prune",
        json!({"org": "acme", "repo": "proj", "path": "proj", "dry_run": false}),
    );
    assert_eq!(none["removed_count"], 0, "{none}");

    // Confinement and refusals are tool errors; nothing changes.
    let before = state(&*mcp);
    for path in ["..", "proj/../..", "missing", "proj/README.md"] {
        let e = c.tool_error(
            "index_path",
            json!({"org": "acme", "repo": "proj", "path": path}),
        );
        assert_eq!(e["code"], "invalid_argument", "{path}: {e}");
    }
    let outside = tmp.parent().unwrap().to_string_lossy().into_owned();
    let e = c.tool_error(
        "prune",
        json!({"org": "acme", "repo": "proj", "path": outside, "dry_run": false}),
    );
    assert_eq!(e["code"], "invalid_argument", "{e}");
    // An empty directory would prune everything: refused.
    std::fs::create_dir_all(tmp.join("empty")).unwrap();
    let e = c.tool_error(
        "prune",
        json!({"org": "acme", "repo": "proj", "path": "empty", "dry_run": false}),
    );
    assert!(e["message"].as_str().unwrap().contains("no files"), "{e}");
    assert_eq!(state(&*mcp), before);
    // The read tools see the writes.
    let files_v = c.ok("list_files", json!({"org": "acme", "repo": "proj"}));
    assert_eq!(files_v["items"].as_array().unwrap().len(), 3, "{files_v}");
}

#[test]
fn write_tools_are_off_by_default() {
    let d = tempfile::tempdir().unwrap();
    let store = open_store(&d.path().join("g.redb"), rust()).unwrap();
    let mut c = Client::new(StoreBackend::embedded(store));
    let listed = c.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), graph_mcp::tools::TOOL_NAMES.len());
    assert!(tools
        .iter()
        .all(|t| t["annotations"]["readOnlyHint"] == true));
    for name in graph_mcp::WRITE_TOOL_NAMES {
        let e = c.rpc_error(name, json!({"org": "o", "repo": "r", "path": "."}));
        assert_eq!(e["code"], graph_mcp::INVALID_PARAMS, "{name}: {e}");
        assert!(e["message"].as_str().unwrap().contains("off"), "{e}");
    }
    let init = c
        .server
        .handle(json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}));
    assert!(init.is_some());
}

#[test]
fn write_tools_equal_the_embedded_store() {
    let d = tempfile::tempdir().unwrap();
    let tree = tempfile::tempdir().unwrap();
    let mcp: Arc<dyn Store> = Arc::from(open_store(&d.path().join("a.redb"), rust()).unwrap());
    let oracle: Arc<dyn Store> = Arc::from(open_store(&d.path().join("b.redb"), rust()).unwrap());
    differential(mcp, oracle, tree.path());
}

#[test]
fn write_tools_equal_the_store_over_a_server() {
    let d = tempfile::tempdir().unwrap();
    let tree = tempfile::tempdir().unwrap();
    let a = TestServer::start(&d.path().join("a.redb"), rust());
    let b = TestServer::start(&d.path().join("b.redb"), rust());
    let connect = |ts: &TestServer| -> Arc<dyn Store> {
        Arc::new(RemoteStore::connect(ClientConfig::new(ts.endpoint())).unwrap())
    };
    differential(connect(&a), connect(&b), tree.path());
}
