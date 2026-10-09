//! Tool conformance (ADR 0005 test plan 1, story 31): the same tool calls
//! against an embedded `V2Store` and a `RemoteStore` over a `TestServer`
//! must equal the `StoreRead` output converted to JSON, page for page. The
//! oracle below reads the store directly and pages by slicing the unpaged
//! answer, so the store-side `limit`/`offset` the tools use is checked
//! too. Every answer is also checked against its `outputSchema`
//! (`common::Client`).
mod common;

use common::{Client, HttpClient, Tools};
use graph_client::{ClientConfig, RemoteStore};
use graph_core::{Extractor, NodeKind, TokenClass};
use graph_mcp::StoreBackend;
use graph_server::testing::TestServer;
use graph_store::{open_store, Grain, Query, Store, StoreRead, SymbolQuery};
use serde_json::{json, Value};

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

const LIB: &str = "pub struct Point { pub x: i32, pub y: i32 }\n\
impl Point {\n    pub fn new(x: i32, y: i32) -> Self { Point { x, y } }\n    pub fn norm(&self) -> i32 { self.x * self.x + self.y * self.y }\n}\n\
pub fn origin() -> Point { Point::new(0, 0) }\n";
const UTIL: &str = "use crate::Point;\n\npub fn shift(p: Point) -> Point {\n    let q = Point::new(p.x + 1, p.y);\n    q\n}\n\npub trait Shape { fn area(&self) -> i32; }\n";
const MAIN: &str = "fn main() {\n    let p = lib::origin();\n    println!(\"{}\", p.norm());\n}\n";
const WIDE: &str = "pub fn widen(p: Point) -> Point { p }\n";
const NOTES: &str = "Point notes: new origin\nshift the Point\n";

fn fill(store: &dyn Store) {
    store
        .index_bytes("acme", "geo", "src/lib.rs", LIB.as_bytes(), None)
        .unwrap();
    store
        .index_bytes("acme", "geo", "src/util.rs", UTIL.as_bytes(), None)
        .unwrap();
    store
        .index_bytes("acme", "geo", "NOTES.txt", NOTES.as_bytes(), None)
        .unwrap();
    store
        .index_bytes("acme", "app", "main.rs", MAIN.as_bytes(), None)
        .unwrap();
    store
        .index_bytes("zeta", "tools", "src/lib.rs", UTIL.as_bytes(), None)
        .unwrap();
    // ADR 0007 (story 43): a UTF-16LE file and a lossy one, so the
    // encodings show in describe, list_files, search and find_symbols.
    let wide: Vec<u8> = std::iter::once(0xFEFF_u16)
        .chain(WIDE.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    let st = store
        .index_bytes("acme", "geo", "src/wide.rs", &wide, None)
        .unwrap();
    assert_eq!(st.encoding.as_deref(), Some("UTF-16LE"));
    let opts = graph_store::IndexOptions {
        encoding: Some(encoding_rs::UTF_8),
        ..Default::default()
    };
    let st = store
        .index_bytes_opts(
            "acme",
            "geo",
            "LOSSY.txt",
            b"Point caf\xe9\n",
            None,
            None,
            opts,
        )
        .unwrap();
    assert!(st.lossy);
}

/// Every call the differential runs.
fn cases() -> Vec<(&'static str, Value)> {
    let mut v = vec![
        ("describe", json!({})),
        ("describe", json!({"org": "acme"})),
        ("describe", json!({"org": "acme", "repo": "geo"})),
        ("list_repos", json!({})),
        ("list_repos", json!({"org": "acme"})),
        ("list_repos", json!({"limit": 1, "offset": 1})),
        ("list_repos", json!({"limit": 2, "offset": 2})),
        ("list_repos", json!({"offset": 10})),
    ];
    for grain in ["token", "symbol", "method", "class", "file", "repo", "org"] {
        v.push(("search", json!({"text": "Point", "grain": grain})));
        v.push((
            "search",
            json!({"text": "x", "grain": grain, "limit": 2, "offset": 1}),
        ));
    }
    v.extend([
        ("search", json!({"text": "Point"})),
        ("search", json!({"text": "Point", "language": "RUST"})),
        ("search", json!({"text": "Point", "org": "acme", "repo": "geo", "grain": "file"})),
        ("search", json!({"text": "Point", "grain": "token", "token_class": "identifier"})),
        ("search", json!({"text": "fn", "grain": "token", "token_class": "keyword", "limit": 3})),
        ("search", json!({"text": "x", "grain": "symbol", "symbol_kind": "fn"})),
        ("search", json!({"text": "x", "grain": "method", "symbol_kind": "method"})),
        ("search", json!({"text": "x", "grain": "class", "symbol_kind": "impl"})),
        ("search", json!({"text": "absent-token"})),
        ("search", json!({"text": "Point", "grain": "token", "limit": 1})),
        ("search", json!({"text": "Point", "grain": "token", "limit": 500, "offset": 3})),
        ("find_symbols", json!({"pattern": "*"})),
        ("find_symbols", json!({"pattern": "n*"})),
        ("find_symbols", json!({"pattern": "Point"})),
        // ADR 0010 D4: case-insensitive by default, `exact_case` opts out.
        ("find_symbols", json!({"pattern": "point"})),
        ("find_symbols", json!({"pattern": "POINT*"})),
        ("find_symbols", json!({"pattern": "point", "exact_case": true})),
        ("find_symbols", json!({"pattern": "Point", "exact_case": true})),
        ("find_symbols", json!({"pattern": "*", "kind": "function"})),
        ("find_symbols", json!({"pattern": "*", "kind": "trait"})),
        ("find_symbols", json!({"pattern": "*", "language": "rust", "org": "acme", "repo": "geo"})),
        ("find_symbols", json!({"pattern": "*", "file": "src/util.rs"})),
        ("find_symbols", json!({"pattern": "*", "limit": 3, "offset": 2})),
        ("find_symbols", json!({"pattern": "*", "limit": 3, "offset": 99})),
        ("file_outline", json!({"org": "acme", "repo": "geo", "path": "src/lib.rs"})),
        ("file_outline", json!({"org": "acme", "repo": "geo", "path": "src/lib.rs", "limit": 2, "offset": 1})),
        ("file_outline", json!({"org": "acme", "repo": "geo", "path": "NOTES.txt"})),
        ("file_tokens", json!({"org": "acme", "repo": "geo", "path": "src/util.rs"})),
        ("file_tokens", json!({"org": "acme", "repo": "geo", "path": "src/util.rs", "start_line": 3, "end_line": 4})),
        ("file_tokens", json!({"org": "acme", "repo": "geo", "path": "src/util.rs", "start_line": 8})),
        ("file_tokens", json!({"org": "acme", "repo": "geo", "path": "src/lib.rs", "limit": 5, "offset": 7})),
        ("file_tokens", json!({"org": "acme", "repo": "geo", "path": "NOTES.txt", "end_line": 1})),
        ("list_files", json!({"org": "acme", "repo": "geo"})),
        ("list_files", json!({"org": "acme", "repo": "geo", "prefix": "src/"})),
        ("list_files", json!({"org": "acme", "repo": "geo", "limit": 1, "offset": 1})),
        ("list_files", json!({"org": "zeta", "repo": "tools", "prefix": "nope"})),
    ]);
    v
}

fn s(a: &Value, k: &str) -> Option<String> {
    a.get(k).and_then(Value::as_str).map(str::to_string)
}

fn to_values<T: serde::Serialize>(v: Vec<T>) -> Vec<Value> {
    v.into_iter()
        .map(|x| serde_json::to_value(x).unwrap())
        .collect()
}

/// `{items, next_offset}` of `all` paged by the call's limit/offset.
fn paged(all: Vec<Value>, args: &Value) -> Value {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let items: Vec<Value> = all.iter().skip(offset).take(limit).cloned().collect();
    let next = (offset + items.len() < all.len()).then(|| offset + items.len());
    json!({"items": items, "next_offset": next})
}

/// What a call must answer, straight from `StoreRead` (no `stale_possible`).
fn oracle(store: &dyn StoreRead, name: &str, a: &Value) -> Value {
    match name {
        "describe" => {
            json!({"repos": store.describe(s(a, "org").as_deref(), s(a, "repo").as_deref()).unwrap()})
        }
        "list_repos" => {
            let mut all = Vec::new();
            for o in store.roots().unwrap() {
                if s(a, "org").is_some_and(|x| x != o.name) {
                    continue;
                }
                for r in store.children(o.id).unwrap() {
                    all.push(json!({"org": o.name, "repo": r.name}));
                }
            }
            paged(all, a)
        }
        "search" => {
            let mut q = Query::new(s(a, "text").unwrap());
            q.grain = s(a, "grain").map_or(Grain::Symbol, |g| g.parse().unwrap());
            q.class = s(a, "token_class").map(|c| c.parse::<TokenClass>().unwrap());
            (q.language, q.org, q.repo, q.symbol_kind) = (
                s(a, "language"),
                s(a, "org"),
                s(a, "repo"),
                s(a, "symbol_kind"),
            );
            paged(to_values(store.search(&q).unwrap()), a)
        }
        "find_symbols" => {
            let mut q = SymbolQuery::new(s(a, "pattern").unwrap());
            (q.kind, q.language, q.org, q.repo, q.file) = (
                s(a, "kind"),
                s(a, "language"),
                s(a, "org"),
                s(a, "repo"),
                s(a, "file"),
            );
            q.exact_case = a
                .get("exact_case")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            paged(to_values(store.search_symbols(&q).unwrap()), a)
        }
        "file_outline" => {
            let mut q = SymbolQuery::new("*");
            (q.org, q.repo, q.file) = (s(a, "org"), s(a, "repo"), s(a, "path"));
            let mut hits = store.search_symbols(&q).unwrap();
            hits.sort_by_key(|h| h.span.map(|s| (s.start, std::cmp::Reverse(s.end))));
            paged(to_values(hits), a)
        }
        "file_tokens" => {
            let toks = store
                .file_tokens(
                    &s(a, "org").unwrap(),
                    &s(a, "repo").unwrap(),
                    &s(a, "path").unwrap(),
                )
                .unwrap()
                .unwrap();
            let lo = a.get("start_line").and_then(Value::as_u64).unwrap_or(0) as u32;
            let hi = a
                .get("end_line")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            let all = toks
                .into_iter()
                .filter(|t| {
                    let l = t.span.unwrap().start_line;
                    l >= lo && u64::from(l) <= hi
                })
                .map(|t| json!({"text": t.name, "token_class": t.token_class, "span": t.span}))
                .collect();
            paged(all, a)
        }
        "list_files" => {
            let org = store
                .roots()
                .unwrap()
                .into_iter()
                .find(|o| Some(&o.name) == s(a, "org").as_ref())
                .unwrap();
            let repo = store
                .children(org.id)
                .unwrap()
                .into_iter()
                .find(|r| Some(&r.name) == s(a, "repo").as_ref())
                .unwrap();
            let prefix = s(a, "prefix").unwrap_or_default();
            let all = store
                .children(repo.id)
                .unwrap()
                .into_iter()
                .filter(|f| f.kind == NodeKind::File && f.name.starts_with(&prefix))
                .map(|f| {
                    let mut v =
                        json!({"path": f.name, "language": f.language, "has_errors": f.has_errors});
                    if let Some(e) = f.encoding {
                        v["encoding"] = e.into();
                    }
                    if f.lossy {
                        v["lossy"] = true.into();
                    }
                    v
                })
                .collect();
            paged(all, a)
        }
        other => panic!("no oracle for {other}"),
    }
}

/// Run every case through `client` and compare with the precomputed
/// oracle answers.
fn check(client: &mut impl Tools, expected: &[Value], label: &str) {
    check_with(client, expected, label, true)
}

/// [`check`]; `fresh`: every answer must also say `stale_possible: false`.
fn check_with(client: &mut impl Tools, expected: &[Value], label: &str, fresh: bool) {
    for ((name, args), want) in cases().iter().zip(expected) {
        let mut got = client.ok(name, args.clone());
        assert!(got["stale_possible"].is_boolean(), "{label} {name} {args}");
        if fresh {
            assert_eq!(got["stale_possible"], false, "{label} {name} {args}");
        }
        got.as_object_mut().unwrap().remove("stale_possible");
        assert_eq!(&got, want, "{label}: {name} {args}");
    }
}

fn expected(store: &dyn StoreRead) -> Vec<Value> {
    let e: Vec<Value> = cases().iter().map(|(n, a)| oracle(store, n, a)).collect();
    // The fixture must make the cases meaningful, not all empty.
    let non_empty = e
        .iter()
        .filter(|v| {
            v["items"].as_array().is_some_and(|i| !i.is_empty())
                || v["repos"].as_array().is_some_and(|r| !r.is_empty())
        })
        .count();
    assert!(
        non_empty > cases().len() * 3 / 4,
        "{non_empty} of {}",
        cases().len()
    );
    e
}

/// Refusals are the same on both targets and list the valid values.
fn check_refusals(c: &mut impl Tools) {
    let e = c.tool_error("search", json!({"text": "x", "language": "cobolx"}));
    assert_eq!(e["code"], "invalid_argument");
    assert!(e["message"].as_str().unwrap().contains("rust"), "{e}");
    let e = c.tool_error(
        "search",
        json!({"text": "x", "grain": "symbol", "symbol_kind": "structz"}),
    );
    assert!(e["message"].as_str().unwrap().contains("struct"), "{e}");
    let e = c.tool_error("find_symbols", json!({"pattern": "*", "kind": "classy"}));
    assert!(e["message"].as_str().unwrap().contains("trait"), "{e}");
    let e = c.tool_error("search", json!({"text": "x", "org": "nobody"}));
    assert!(e["message"].as_str().unwrap().contains("acme/geo"), "{e}");
    let e = c.tool_error("list_files", json!({"org": "acme", "repo": "nope"}));
    assert!(e["message"].as_str().unwrap().contains("geo"), "{e}");
    let e = c.tool_error("list_repos", json!({"org": "nobody"}));
    assert!(e["message"].as_str().unwrap().contains("zeta"), "{e}");
    let e = c.tool_error(
        "file_tokens",
        json!({"org": "acme", "repo": "geo", "path": "missing.rs"}),
    );
    assert!(e["message"].as_str().unwrap().contains("missing.rs"), "{e}");
    let e = c.tool_error(
        "file_outline",
        json!({"org": "acme", "repo": "geo", "path": "missing.rs"}),
    );
    assert_eq!(e["code"], "invalid_argument");
    // A store refusal (an ambiguous pattern) is a typed isError result.
    let e = c.tool_error("find_symbols", json!({"pattern": "*", "exact_case": "yes"}));
    assert!(e["message"].as_str().unwrap().contains("boolean"), "{e}");
    let e = c.tool_error("find_symbols", json!({"pattern": "**"}));
    assert_eq!(e["code"], "rejected", "{e}");
    let e = c.tool_error("search", json!({"text": "x", "grain": "function"}));
    assert_eq!(e["code"], "invalid_params");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("token, symbol, method"),
        "{e}"
    );
    let e = c.tool_error("search", json!({"text": "x", "limit": 501}));
    assert_eq!(e["code"], "invalid_params");
    for args in [
        json!({"org": "nobody"}),
        json!({"org": "acme", "repo": "nope"}),
    ] {
        let e = c.tool_error("describe", args);
        assert_eq!(e["code"], "invalid_argument");
        assert!(e["message"].as_str().unwrap().contains("acme/geo"), "{e}");
    }
    let e = c.rpc_error("nope", json!({}));
    assert_eq!(e["code"], graph_mcp::INVALID_PARAMS);
}

#[test]
fn embedded_tools_equal_store_read() {
    let d = tempfile::tempdir().unwrap();
    let store = open_store(&d.path().join("g.redb"), rust()).unwrap();
    fill(&*store);
    let want = expected(&*store);
    let mut c = Client::new(StoreBackend::embedded(store));
    check(&mut c, &want, "embedded");
    check_refusals(&mut c);
    // ADR 0007 (story 43): the encodings are in the tool answers (and the
    // answers validated against their outputSchemas in `Client`).
    let d = c.ok("describe", json!({"org": "acme", "repo": "geo"}));
    assert_eq!(d["repos"][0]["encodings"], json!({"UTF-16LE": 1}), "{d}");
    assert_eq!(d["repos"][0]["lossy"], 1, "{d}");
    let files = c.ok("list_files", json!({"org": "acme", "repo": "geo"}));
    let by_path = |p: &str| {
        files["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == p)
            .cloned()
            .unwrap()
    };
    assert_eq!(by_path("src/wide.rs")["encoding"], "UTF-16LE");
    assert!(by_path("src/wide.rs").get("lossy").is_none());
    assert_eq!(by_path("LOSSY.txt")["lossy"], true);
    assert!(by_path("LOSSY.txt").get("encoding").is_none());
    assert!(by_path("src/lib.rs").get("encoding").is_none());
    // ADR 0010 D4 / #269: `point` finds `Point`; `exact_case` keeps the old answer.
    let hits = c.ok("find_symbols", json!({"pattern": "point"}));
    assert!(
        hits["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["name"] == "Point")
            && !hits["items"].as_array().unwrap().is_empty(),
        "{hits}"
    );
    let hits = c.ok(
        "find_symbols",
        json!({"pattern": "point", "exact_case": true}),
    );
    assert!(hits["items"].as_array().unwrap().is_empty(), "{hits}");
    let hits = c.ok("find_symbols", json!({"pattern": "widen"}));
    assert_eq!(hits["items"][0]["encoding"], "UTF-16LE", "{hits}");
    // ADR 0010 D3: the declaration (name) position, `widen` after `pub fn `,
    // while the span starts at `pub`.
    let p = &hits["items"][0];
    assert_eq!(p["span"]["start_col"], 1, "{hits}");
    assert_eq!(
        (&p["name_pos"]["line"], &p["name_pos"]["col"]),
        (&1.into(), &8.into()),
        "{hits}"
    );
    let rows = c.ok("search", json!({"text": "widen", "grain": "symbol"}));
    let r = &rows["items"][0];
    assert_eq!(r["name_pos"]["col"], 8, "{rows}");
    let rows = c.ok("search", json!({"text": "widen", "grain": "token"}));
    assert!(rows["items"][0].get("name_pos").is_none(), "{rows}");
}

#[test]
fn remote_tools_equal_store_read_and_the_embedded_answers() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    {
        let store = open_store(&db, rust()).unwrap();
        fill(&*store);
    }
    // The oracle from the same file opened embedded, before the server
    // owns it: remote answers must equal embedded StoreRead answers.
    let want = {
        let store = open_store(&db, rust()).unwrap();
        expected(&*store)
    };
    let mut server = TestServer::start(&db, rust());
    let oracle_client = RemoteStore::connect(ClientConfig::new(server.endpoint())).unwrap();
    assert_eq!(
        cases()
            .iter()
            .map(|(n, a)| oracle(&oracle_client, n, a))
            .collect::<Vec<_>>(),
        want,
        "StoreRead over the wire equals StoreRead embedded"
    );
    let mut cfg = ClientConfig::new(server.endpoint());
    cfg.retry.budget = std::time::Duration::from_millis(500);
    let store = RemoteStore::connect(cfg).unwrap();
    let log = store.read_log();
    let mut c = Client::new(StoreBackend::remote(
        Box::new(store),
        Box::new(move || log.stale_reads()),
    ));
    check(&mut c, &want, "remote");
    check_refusals(&mut c);
    // A connection lost mid-session is retryable, not a storage failure.
    server.stop();
    let e = c.tool_error("describe", json!({}));
    assert_eq!(e["code"], "unavailable", "{e}");
    assert_eq!(e["retryable"], true);
}

/// `stale_possible` is per call: set only when a read of that call was.
#[test]
fn stale_possible_follows_the_counter_per_call() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    let d = tempfile::tempdir().unwrap();
    let store = open_store(&d.path().join("g.redb"), rust()).unwrap();
    fill(&*store);
    let n = Arc::new(AtomicU64::new(0));
    let bump = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (n2, b2) = (Arc::clone(&n), Arc::clone(&bump));
    let counter = move || {
        if b2.load(Ordering::SeqCst) {
            n2.fetch_add(1, Ordering::SeqCst);
        }
        n2.load(Ordering::SeqCst)
    };
    let mut c = Client::new(StoreBackend::remote(store, Box::new(counter)));
    assert_eq!(c.ok("describe", json!({}))["stale_possible"], false);
    bump.store(true, Ordering::SeqCst);
    assert_eq!(c.ok("describe", json!({}))["stale_possible"], true);
    bump.store(false, Ordering::SeqCst);
    assert_eq!(c.ok("list_repos", json!({}))["stale_possible"], false);
}

// ------------------------------------------------ the in-serve HTTP adapter

use graph_server::mcp::{McpConfig, McpRead};
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use std::time::{Duration, Instant};

/// A 3-node cluster whose every node serves MCP on a free loopback port
/// with `read`, filled through the leader and caught up everywhere.
fn mcp_cluster(read: impl Fn(u64) -> McpRead) -> ClusterTestbed {
    let mut tb = ClusterTestbed::with_config(3, rust(), |id, cfg| {
        let mut m = McpConfig::new("127.0.0.1:0".parse().unwrap());
        m.read = read(id);
        cfg.mcp = Some(m);
    });
    tb.form();
    tb.write_via_leader(|c| fill(c));
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    tb
}

fn mcp_addr(tb: &ClusterTestbed, id: u64) -> std::net::SocketAddr {
    tb.node(id).running().unwrap().mcp_addr.expect("MCP is on")
}

fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + CLUSTER_WAIT;
    while !probe() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The third backend of ADR 0005 test plan 1: the `/mcp` endpoint inside
/// `serve`, over the in-process Store service, on every node of a 3-node
/// cluster (leader and followers, local and linearizable reads), equals
/// `StoreRead` embedded, call for call.
#[test]
fn in_serve_http_adapter_on_a_cluster_equals_store_read() {
    let d = tempfile::tempdir().unwrap();
    let want = {
        let store = open_store(&d.path().join("g.redb"), rust()).unwrap();
        fill(&*store);
        expected(&*store)
    };
    // Node 2 reads linearizably, the others locally.
    let tb = mcp_cluster(|id| {
        if id == 2 {
            McpRead::Linearizable
        } else {
            McpRead::Local
        }
    });
    let leader = tb.leader();
    for id in tb.ids() {
        let label = format!("http node {id} (leader {leader})");
        let mut c = HttpClient::new(mcp_addr(&tb, id));
        // A linearizable read is never stale_possible; a local one on a
        // caught-up member of a healthy cluster may be, briefly.
        check_with(&mut c, &want, &label, id == 2);
        check_refusals(&mut c);
        // Per-tool call and error counters on that node.
        let metrics = tb.client(id).admin_metrics().unwrap();
        for line in [
            "mg_mcp_tool_calls_total{tool=\"unknown\",outcome=\"rejected\"} 1",
            "mg_mcp_tool_calls_total{tool=\"find_symbols\",outcome=\"error\"}",
            "mg_mcp_tool_calls_total{tool=\"search\",outcome=\"ok\"}",
        ] {
            assert!(
                metrics.contains(line),
                "node {id}: no `{line}` in\n{metrics}"
            );
        }
    }
}

/// Linearizable MCP reads on a node cut off in a minority fail with the
/// retryable `no_leader`, as a gRPC client's do; never a stale answer.
#[test]
fn http_linearizable_read_in_a_minority_is_no_leader() {
    let tb = mcp_cluster(|_| McpRead::Linearizable);
    let leader = tb.leader();
    let m = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    let majority: Vec<u64> = tb.ids().into_iter().filter(|i| *i != m).collect();
    let mut c = HttpClient::new(mcp_addr(&tb, m));
    assert_eq!(c.ok("list_repos", json!({}))["stale_possible"], false);
    tb.partition(&[m], &majority);
    let e = c.tool_error("list_repos", json!({}));
    assert_eq!(e["code"], "no_leader", "{e}");
    assert_eq!(e["retryable"], true, "{e}");
    tb.heal();
}

/// Local MCP reads on a partitioned follower keep answering, and say
/// `stale_possible` as a gRPC `LOCAL` read there does.
#[test]
fn http_local_read_in_a_minority_answers_stale_possible() {
    let tb = mcp_cluster(|_| McpRead::Local);
    let leader = tb.leader();
    let m = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    let majority: Vec<u64> = tb.ids().into_iter().filter(|i| *i != m).collect();
    let mut c = HttpClient::new(mcp_addr(&tb, m));
    let grpc = tb.client(m);
    tb.partition(&[m], &majority);
    wait_until("a stale_possible MCP read in the minority", || {
        let got = c.ok("list_repos", json!({}));
        assert_eq!(got["items"].as_array().unwrap().len(), 3, "{got}");
        got["stale_possible"] == true
    });
    grpc.roots().unwrap();
    assert!(
        grpc.read_log().last().unwrap().stale_possible,
        "a gRPC LOCAL read there agrees"
    );
    tb.heal();
}
