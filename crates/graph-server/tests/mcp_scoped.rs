//! ADR 0013 PR 3c (story 74c): the MCP read tools on the scoped view, over
//! `/mcp` (HTTP) and over the stdio transport's backend (`memory-graph mcp
//! --server` is a `StoreBackend` over a `RemoteStore`, driven here
//! in-process through `McpServer::handle_bytes`, the same code
//! `serve_stdio` runs per line).
//!
//! Aggregate tools answer exactly as the same tool on an embedded index of
//! only the visible repos (the scoped differential); named tools on an
//! existing hidden repo are `isError` with `permission_denied` (HTTP 200);
//! `tools/list` hides the write tools a token has no permit for.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_mcp::{McpServer, StoreBackend};
use graph_server::testing::auth::{stanza, AuthFixture};
use graph_server::testing::mcp_http::McpHttpClient;
use graph_server::testing::TestServer;
use graph_store::{conformance, ReadScope, ScopeRules, StoreRead};
use serde_json::{json, Value};

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// `r` reads and searches `o1` except `o1/r2`, plus `o2/r1`; `ix` may
/// index (not prune) anywhere; `z` has no read grant at all.
const POLICIES: &str = r#"
permit(principal == Token::"w2", action, resource);
permit(principal == Token::"r", action in [Action::"read", Action::"search"], resource in Org::"o1");
forbid(principal == Token::"r", action in [Action::"read", Action::"search"], resource == Repo::"o1/r2");
permit(principal == Token::"r", action in [Action::"read", Action::"search"], resource == Repo::"o2/r1");
permit(principal == Token::"ix", action == Action::"index", resource);
"#;

fn scope() -> ReadScope {
    ReadScope::from_rules(ScopeRules {
        orgs: ["o1".to_string()].into(),
        repos: [("o2".to_string(), "r1".to_string())].into(),
        deny_repos: [("o1".to_string(), "r2".to_string())].into(),
        ..Default::default()
    })
}

/// A tool call through an in-process `McpServer` (the stdio path).
struct Stdio(McpServer<StoreBackend>);

impl Stdio {
    fn new(store: Box<dyn StoreRead + Send>) -> Self {
        let mut s = McpServer::new(StoreBackend::embedded(store), "t", "0");
        let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0],
            "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        s.handle_bytes(init.to_string().as_bytes()).unwrap();
        s.handle_bytes(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
                .to_string()
                .as_bytes(),
        );
        Self(s)
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}});
        serde_json::from_str(&self.0.handle_bytes(req.to_string().as_bytes()).unwrap()).unwrap()
    }
}

fn http_call(c: &mut McpHttpClient, name: &str, args: Value) -> Value {
    c.request("tools/call", json!({"name": name, "arguments": args}))
}

/// The answer without transport-dependent freshness flags.
fn content(v: &Value) -> Value {
    let mut s = v["result"]["structuredContent"].clone();
    if let Some(m) = s.as_object_mut() {
        m.remove("stale_possible");
    }
    s
}

fn refused(v: &Value, what: &str) {
    assert_eq!(v["result"]["isError"], true, "{what}: {v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("permission_denied"), "{what}: {text}");
    for leak in ["o1", "r2", "Repo::"] {
        assert!(!text.contains(leak), "{what}: {text}");
    }
}

#[test]
fn mcp_read_tools_follow_the_scope_on_both_transports() {
    let f = AuthFixture::new();
    let ix = graph_server::auth::new_credential("mgt_").to_string();
    let z = graph_server::auth::new_credential("mgt_").to_string();
    f.write_tokens(
        &[
            stanza("w2", "write", &f.write2, None),
            stanza("r", "read", &f.read, None),
            stanza("ix", "write", &ix, None),
            stanza("z", "read", &z, None),
        ]
        .join("\n"),
    );
    let pol = f.dir.path().join("policies.cedar");
    std::fs::write(&pol, POLICIES).unwrap();
    let d = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("x")).unwrap();
    std::fs::write(root.path().join("x/a.txt"), b"foo").unwrap();
    let mut cfg = f.config();
    cfg.policies = Some(pol);
    let root_path = root.path().to_path_buf();
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| {
        c.auth = cfg;
        let mut m = graph_server::mcp::McpConfig::new("127.0.0.1:0".parse().unwrap());
        m.index_root = Some(root_path);
        c.mcp = Some(m);
    });
    let remote = |token: &str| {
        let mut cc = ClientConfig::new(ts.endpoint());
        cc.credentials = Some(f.creds(token));
        RemoteStore::connect(cc).unwrap()
    };
    let scope = scope();
    let bd = tempfile::tempdir().unwrap();
    let b = graph_store::open_store(&bd.path().join("b.redb"), rust()).unwrap();
    // The oracle: B holds exactly the repos POLICIES lets `r` read,
    // written out by hand (o1 minus o1/r2, plus o2/r1; o1/doomed is
    // indexed and deleted in both), not derived from the store's scope.
    let visible = [("o1", "r1"), ("o1", "r3"), ("o1", "doomed"), ("o2", "r1")];
    for (o, r) in conformance::SCOPED_REPOS {
        assert_eq!(
            scope.repo_visible(o, r),
            visible.contains(&(*o, *r)),
            "{o}/{r}"
        );
    }
    conformance::seed_scoped_with(&remote(&f.write2), &*b, &visible);
    let addr = ts.running().unwrap().mcp_addr.unwrap();

    let mut want = Stdio::new(Box::new(std::sync::Arc::<dyn graph_store::Store>::from(b)));
    let mut stdio = Stdio::new(Box::new(remote(&f.read)));
    let mut http = McpHttpClient::with_token(addr, Some(&f.read));

    // Aggregate tools: the scoped differential's answers.
    let aggregates = [
        ("list_repos", json!({})),
        ("describe", json!({})),
        ("search", json!({"text": "foo"})),
        (
            "search",
            json!({"text": "foo", "grain": "token", "limit": 3}),
        ),
        ("search", json!({"text": "samecontent", "grain": "file"})),
        ("find_symbols", json!({"pattern": "*"})),
        ("find_symbols", json!({"pattern": "dup*", "limit": 2})),
        // Named, visible: answered like B; repo-only `r1` keeps o2/r1 only.
        ("describe", json!({"org": "o1"})),
        ("search", json!({"text": "foo", "repo": "r1"})),
        ("list_files", json!({"org": "o1", "repo": "r1"})),
        (
            "file_outline",
            json!({"org": "o1", "repo": "r1", "path": "lib.rs"}),
        ),
        (
            "file_tokens",
            json!({"org": "o2", "repo": "r1", "path": "same.txt"}),
        ),
        // Missing: today's answer.
        ("describe", json!({"org": "nope"})),
        ("list_files", json!({"org": "o1", "repo": "nope"})),
    ];
    for (tool, args) in &aggregates {
        let w = content(&want.call(tool, args.clone()));
        let s = stdio.call(tool, args.clone());
        assert_eq!(content(&s), w, "stdio {tool} {args}");
        let h = http_call(&mut http, tool, args.clone());
        assert_eq!(content(&h), w, "http {tool} {args}");
    }
    // Named tools on an existing hidden repo (or org): refused, HTTP 200.
    for (tool, args) in [
        ("list_files", json!({"org": "o1", "repo": "r2"})),
        (
            "file_outline",
            json!({"org": "o1", "repo": "r2", "path": "m.txt"}),
        ),
        (
            "file_tokens",
            json!({"org": "o1", "repo": "r2", "path": "m.txt"}),
        ),
        ("search", json!({"text": "foo", "org": "o1", "repo": "r2"})),
        ("find_symbols", json!({"pattern": "*", "org": "o3"})),
        ("describe", json!({"org": "o4"})),
        ("list_repos", json!({"org": "o4"})),
    ] {
        refused(&stdio.call(tool, args.clone()), &format!("stdio {tool}"));
        refused(
            &http_call(&mut http, tool, args.clone()),
            &format!("http {tool}"),
        );
    }
    // A zero-grant token: empty aggregates on both transports.
    let mut zs = Stdio::new(Box::new(remote(&z)));
    let mut zh = McpHttpClient::with_token(addr, Some(&z));
    for (tool, args) in [
        ("list_repos", json!({})),
        ("search", json!({"text": "foo"})),
        ("find_symbols", json!({"pattern": "*"})),
    ] {
        for v in [
            zs.call(tool, args.clone()),
            http_call(&mut zh, tool, args.clone()),
        ] {
            assert_eq!(v["result"]["isError"], false, "{v}");
            let c = content(&v);
            let rows = c
                .as_object()
                .unwrap()
                .values()
                .find_map(Value::as_array)
                .unwrap();
            assert!(rows.is_empty(), "{tool}: {c}");
        }
    }
    refused(
        &http_call(&mut zh, "list_files", json!({"org": "o1", "repo": "r1"})),
        "zero grant, named",
    );

    // tools/list hides the write tools a token has no permit for anywhere.
    let names = |c: &mut McpHttpClient| -> Vec<String> {
        c.request("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let w2 = names(&mut McpHttpClient::with_token(addr, Some(&f.write2)));
    assert!(w2.iter().any(|n| n == "index_path") && w2.iter().any(|n| n == "prune"));
    let ixn = names(&mut McpHttpClient::with_token(addr, Some(&ix)));
    assert!(ixn.iter().any(|n| n == "index_path"), "{ixn:?}");
    assert!(
        !ixn.iter().any(|n| n == "prune"),
        "no prune permit: {ixn:?}"
    );
    let rn = names(&mut http);
    assert!(
        !rn.iter().any(|n| n == "index_path" || n == "prune"),
        "{rn:?}"
    );
    // Hidden, still refused when called: `ix` has no prune permit.
    let mut ixc = McpHttpClient::with_token(addr, Some(&ix));
    refused(
        &http_call(
            &mut ixc,
            "prune",
            json!({"org": "o1", "repo": "r1", "path": "x"}),
        ),
        "hidden prune",
    );
}
