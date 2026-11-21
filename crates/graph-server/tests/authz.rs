//! Policy authorization, ADR 0013 PR 1 (story 72), against an in-process
//! server: writes and cluster actions decided by `--auth-policies`, the
//! interim all-or-nothing read rule, `--auth-builtins`, the paired reload
//! (keep-last-good, the fail-safe mixed cases, revocation), `policy_hash`,
//! and MCP tool calls refused as `isError` results.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::{Extractor, NodeKind};
use graph_server::auth::Reload;
use graph_server::testing::auth::{stanza, AuthFixture};
use graph_server::testing::TestServer;
use graph_store::{Query, Store, StoreError, StoreRead};
use std::collections::HashSet;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

struct Env {
    f: AuthFixture,
    policies: std::path::PathBuf,
    _d: tempfile::TempDir,
    ts: TestServer,
}

/// The fixture's tokens with groups: `w` (write) is in `web-team`, `w2`
/// (write) in no group, `r` (read) in `readers`.
fn grouped_tokens(f: &AuthFixture) -> String {
    [
        stanza("w", "write", &f.write, None) + "groups = [\"web-team\"]\n",
        stanza("w2", "write", &f.write2, None),
        stanza("r", "read", &f.read, None) + "groups = [\"readers\"]\n",
    ]
    .join("\n")
}

const POLICIES: &str = r#"
@id("seed")
permit(principal == Token::"w2", action in [Action::"index", Action::"read", Action::"search"], resource);
@id("web-index")
permit(principal in Group::"web-team", action == Action::"index", resource == Repo::"acme/web");
@id("web-prune")
permit(principal in Group::"web-team", action in [Action::"prune", Action::"delete"], resource in Org::"acme");
@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource == Repo::"acme/legal");
@id("readers")
permit(principal in Group::"readers", action in [Action::"read", Action::"search", Action::"status"], resource);
"#;

fn env_with(policies: &str, builtins: bool) -> Env {
    let f = AuthFixture::new();
    f.write_tokens(&grouped_tokens(&f));
    let pol = f.dir.path().join("policies.cedar");
    std::fs::write(&pol, policies).unwrap();
    let d = tempfile::tempdir().unwrap();
    let mut cfg = f.config();
    cfg.policies = Some(pol.clone());
    cfg.builtins = builtins;
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| c.auth = cfg);
    Env {
        f,
        policies: pol,
        _d: d,
        ts,
    }
}

fn client(e: &Env, token: &str) -> RemoteStore {
    let mut cfg = ClientConfig::new(e.ts.endpoint());
    cfg.credentials = Some(e.f.creds(token));
    RemoteStore::connect(cfg).unwrap()
}

fn denied(r: Result<impl std::fmt::Debug, StoreError>, action: &str) {
    match r {
        Err(StoreError::PermissionDenied { msg, .. }) => {
            assert_eq!(msg, format!("permission denied for action `{action}`"));
            // D9: never a resource or a policy @id.
            for leak in ["acme", "web", "legal", "seed", "readers"] {
                assert!(!msg.contains(leak), "{msg}");
            }
        }
        other => panic!("expected PermissionDenied for `{action}`, got {other:?}"),
    }
}

fn reload(e: &Env) -> (Option<Reload>, Option<Reload>) {
    e.ts.running().unwrap().auth().reload_now()
}

#[test]
fn writes_follow_the_policies() {
    let e = env_with(POLICIES, false);
    let w = client(&e, &e.f.write);
    let src = b"fn a() {}";
    w.index_bytes("acme", "web", "a.rs", src, None).unwrap();
    denied(w.index_bytes("acme", "api", "a.rs", src, None), "index");
    denied(w.index_bytes("other", "web", "a.rs", src, None), "index");
    // A repo that does not exist yet is decided the same way.
    denied(w.index_bytes("acme", "legal", "a.rs", src, None), "index");
    // w2 may index anywhere (seed) except the frozen repo.
    let w2 = client(&e, &e.f.write2);
    w2.index_bytes("acme", "api", "b.rs", src, None).unwrap();
    denied(w2.index_bytes("acme", "legal", "b.rs", src, None), "index");
    // Prune: one repo, on the Repo; the forbid wins.
    let keep = HashSet::new();
    w.prune_files("acme", "web", &keep, true).unwrap();
    w.prune_files("acme", "api", &keep, false).unwrap();
    denied(w.prune_files("acme", "legal", &keep, true), "prune");
    denied(w2.prune_files("acme", "web", &keep, true), "prune");
    // Delete: org-wide needs the Org grant and no forbid on a repo inside.
    denied(w.delete_scope("acme", None, true), "delete");
    w.delete_scope("acme", Some("api"), true).unwrap();
    denied(w2.delete_scope("acme", Some("api"), true), "delete");
    // An index-but-not-read token: it indexed acme/web but cannot read it.
    let roots = w.roots().unwrap();
    assert!(roots.is_empty(), "aggregate reads answer empty: {roots:?}");
    denied(w.describe(Some("acme"), Some("web")), "read");
}

#[test]
fn org_wide_delete_is_allowed_by_an_org_grant() {
    let e = env_with(
        r#"permit(principal == Token::"w", action in [Action::"index", Action::"prune", Action::"delete"], resource in Org::"acme");
permit(principal == Token::"w", action in [Action::"read", Action::"search"], resource);
forbid(principal == Token::"w2", action, resource);"#,
        false,
    );
    let w = client(&e, &e.f.write);
    w.index_bytes("acme", "web", "a.rs", b"fn a() {}", None)
        .unwrap();
    w.delete_scope("acme", None, false).unwrap();
    assert!(w.describe(Some("acme"), None).unwrap().is_empty());
}

#[test]
fn cluster_actions_follow_the_policies() {
    let e = env_with(POLICIES, false);
    // `r` has status via the readers policy; `w` has no status grant.
    let r = client(&e, &e.f.read);
    let st = r.admin_status().unwrap();
    assert_eq!(st.policy_hash.len(), 64, "policy_hash in Admin.Status");
    let w = client(&e, &e.f.write);
    denied(w.admin_status(), "status");
    denied(w.admin_compact(), "admin");
    denied(w.admin_list_backups(), "status");
    denied(w.vacuum(), "admin");
    // The role is a ceiling: a read token never gets admin, even with a
    // permit for it.
    let e = env_with(
        r#"permit(principal == Token::"r", action, resource);"#,
        false,
    );
    let r = client(&e, &e.f.read);
    r.admin_status().unwrap();
    assert!(matches!(
        r.admin_compact(),
        Err(StoreError::PermissionDenied { .. })
    ));
}

#[test]
fn the_interim_read_rule_is_all_or_nothing() {
    let e = env_with(POLICIES, false);
    let w2 = client(&e, &e.f.write2);
    w2.index_bytes("acme", "api", "a.rs", b"fn hello() {}", None)
        .unwrap();
    // A principal with an unconstrained read permit reads everything.
    let r = client(&e, &e.f.read);
    let roots = r.roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(r.search(&Query::new("hello")).unwrap().len(), 1);
    assert_eq!(r.describe(Some("acme"), Some("api")).unwrap().len(), 1);
    // One without: aggregate reads answer empty ...
    let w = client(&e, &e.f.write);
    assert!(w.roots().unwrap().is_empty());
    assert_eq!(w.count_nodes(NodeKind::Token).unwrap(), 0);
    assert!(w.search(&Query::new("hello")).unwrap().is_empty());
    assert!(w.describe(None, None).unwrap().is_empty());
    // ... named reads of something that exists are refused ...
    let org = roots[0].id;
    denied(w.get(org), "read");
    denied(w.children(org), "read");
    denied(w.describe(Some("acme"), None), "read");
    denied(w.file_tokens("acme", "api", "a.rs"), "read");
    let mut q = Query::new("hello");
    q.org = Some("acme".into());
    denied(w.search(&q), "search");
    // ... and a missing one keeps today's answer.
    assert_eq!(w.get(org + 1_000_000).unwrap(), None);
    assert!(w.children(org + 1_000_000).unwrap().is_empty());
    assert!(w.describe(Some("nope"), None).unwrap().is_empty());
    assert_eq!(w.file_tokens("acme", "api", "zz.rs").unwrap(), None);
    q.org = Some("nope".into());
    assert!(w.search(&q).unwrap().is_empty());
}

#[test]
fn a_resource_constrained_read_policy_is_refused_at_load() {
    for (text, id) in [
        (
            "@id(\"scoped-read\")\npermit(principal, action == Action::\"read\", resource in Org::\"acme\");",
            "scoped-read",
        ),
        (
            "@id(\"scoped-forbid\")\nforbid(principal, action == Action::\"search\", resource == Repo::\"acme/web\");",
            "scoped-forbid",
        ),
    ] {
        let f = AuthFixture::new();
        let pol = f.dir.path().join("p.cedar");
        std::fs::write(&pol, text).unwrap();
        let mut cfg = f.config();
        cfg.policies = Some(pol);
        let e = graph_server::auth::Auth::load(&cfg, false).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains(&format!("@id `{id}`")) && msg.contains("line 1"), "{msg}");
    }
}

#[test]
fn builtins_are_off_by_default_and_on_with_the_flag() {
    // An empty policy file: nothing allowed.
    let e = env_with("", false);
    let w = client(&e, &e.f.write);
    denied(w.index_bytes("o", "r", "a.rs", b"fn a() {}", None), "index");
    denied(w.admin_status(), "status");
    // With --auth-builtins: ADR 0012 roles.
    let e = env_with("// nothing\n", true);
    let w = client(&e, &e.f.write);
    w.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    w.admin_status().unwrap();
    let r = client(&e, &e.f.read);
    assert_eq!(r.roots().unwrap().len(), 1);
    assert!(matches!(
        r.index_bytes("o", "r", "b.rs", b"fn b() {}", None),
        Err(StoreError::PermissionDenied { .. })
    ));
}

#[test]
fn the_pair_reloads_together_and_keeps_the_last_good_pair() {
    let e = env_with(POLICIES, false);
    let w = client(&e, &e.f.write);
    let src = b"fn a() {}";
    assert_eq!(reload(&e).0, Some(Reload::Unchanged));
    let h0 = e.ts.running().unwrap().auth().policy_hash();
    // A widening policy file alone.
    std::fs::write(
        &e.policies,
        format!(
            "{POLICIES}\npermit(principal == Token::\"w\", action == Action::\"index\", resource);"
        ),
    )
    .unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    assert_ne!(e.ts.running().unwrap().auth().policy_hash(), h0);
    w.index_bytes("acme", "api", "a.rs", src, None).unwrap();
    // An invalid policy file keeps the last good pair (the widening stays).
    std::fs::write(
        &e.policies,
        "permit(principal, action, resource) when { true };",
    )
    .unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Kept));
    w.index_bytes("acme", "api", "b.rs", src, None).unwrap();
    // A half-written token file, too.
    let toks = grouped_tokens(&e.f);
    std::fs::write(&e.policies, POLICIES).unwrap();
    e.f.write_tokens(&toks[..toks.len() / 2]);
    assert_eq!(reload(&e).0, Some(Reload::Kept));
    w.index_bytes("acme", "api", "c.rs", src, None).unwrap();
    // A deleted policy file.
    e.f.write_tokens(&toks);
    std::fs::remove_file(&e.policies).unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Kept));
    w.index_bytes("acme", "api", "d.rs", src, None).unwrap();
    // Both good again: the narrowing applies.
    std::fs::write(&e.policies, POLICIES).unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    denied(w.index_bytes("acme", "api", "e.rs", src, None), "index");
    assert_eq!(e.ts.running().unwrap().auth().policy_hash(), h0);
}

#[test]
fn revocation_always_takes_effect_and_mixed_pairs_fail_safe() {
    let e = env_with(POLICIES, false);
    let w = client(&e, &e.f.write);
    w.index_bytes("acme", "web", "a.rs", b"fn a() {}", None)
        .unwrap();
    // Remove `w` from the token file; the policy file still names its
    // group (and `w2` by id): the reload succeeds (unknown principals only
    // warn) and `w` is refused.
    let without_w = [
        stanza("w2", "write", &e.f.write2, None),
        stanza("r", "read", &e.f.read, None) + "groups = [\"readers\"]\n",
    ]
    .join("\n");
    e.f.write_tokens(&without_w);
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    assert!(matches!(
        w.index_bytes("acme", "web", "b.rs", b"fn b() {}", None),
        Err(StoreError::Unauthenticated(_))
    ));
    // A new token with no policy yet gets nothing (default deny).
    let fresh = graph_server::auth::new_credential("mgt_").to_string();
    e.f.write_tokens(&format!(
        "{without_w}\n{}",
        stanza("new", "write", &fresh, None)
    ));
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    let n = client(&e, &fresh);
    denied(
        n.index_bytes("acme", "web", "c.rs", b"fn c() {}", None),
        "index",
    );
    // A policy for a token not yet in the file matches nothing, and loads.
    std::fs::write(
        &e.policies,
        format!(
            "{POLICIES}\n@id(\"future\") permit(principal == Token::\"later\", action, resource);"
        ),
    )
    .unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    denied(
        n.index_bytes("acme", "web", "c.rs", b"fn c() {}", None),
        "index",
    );
}

#[test]
fn an_index_stream_is_checked_per_file() {
    // Many files in one `Index` stream: each is checked as it is read; a
    // repo the token may not index is refused before any file is applied.
    let e = env_with(POLICIES, false);
    let w = client(&e, &e.f.write);
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("f{i}.rs"), format!("fn f{i}() {{}}").into_bytes()))
        .collect();
    let r = index_stream(&w, "acme", "api", &files);
    denied(r, "index");
    index_stream(&w, "acme", "web", &files).unwrap();
}

/// One `Index` stream of `files` through `RemoteStore::index_batch`.
fn index_stream(
    s: &RemoteStore,
    org: &str,
    repo: &str,
    files: &[(String, Vec<u8>)],
) -> Result<(), StoreError> {
    let batch: Vec<graph_store::BatchFile<'_>> = files
        .iter()
        .map(|(p, b)| graph_store::BatchFile {
            path: p,
            bytes: b,
            ..Default::default()
        })
        .collect();
    for r in s.index_batch(org, repo, &batch, graph_store::IndexOptions::default())? {
        r?;
    }
    Ok(())
}

#[test]
fn without_a_policy_file_status_has_no_policy_hash() {
    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let cfg = f.config();
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| c.auth = cfg);
    let mut cc = ClientConfig::new(ts.endpoint());
    cc.credentials = Some(f.creds(&f.read));
    let r = RemoteStore::connect(cc).unwrap();
    assert_eq!(r.admin_status().unwrap().policy_hash, "");
    assert!(!ts.running().unwrap().auth().policies_on());
}

#[test]
fn auth_policies_needs_tokens_and_builtins_needs_policies() {
    let cfg = graph_server::AuthConfig {
        policies: Some("p.cedar".into()),
        ..Default::default()
    };
    assert!(graph_server::auth::Auth::load(&cfg, false)
        .unwrap_err()
        .to_string()
        .contains("--auth-policies needs --auth-tokens"));
    let f = AuthFixture::new();
    let mut cfg = f.config();
    cfg.builtins = true;
    assert!(graph_server::auth::Auth::load(&cfg, false)
        .unwrap_err()
        .to_string()
        .contains("--auth-builtins needs --auth-policies"));
}

#[test]
fn mcp_tool_calls_follow_the_policies() {
    use graph_server::testing::mcp_http::McpHttpClient;
    let f = AuthFixture::new();
    f.write_tokens(&grouped_tokens(&f));
    let pol = f.dir.path().join("policies.cedar");
    std::fs::write(&pol, POLICIES).unwrap();
    let d = tempfile::tempdir().unwrap();
    let mut cfg = f.config();
    cfg.policies = Some(pol);
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| {
        c.auth = cfg;
        c.mcp = Some(graph_server::mcp::McpConfig::new(
            "127.0.0.1:0".parse().unwrap(),
        ));
    });
    let mut cc = ClientConfig::new(ts.endpoint());
    cc.credentials = Some(f.creds(&f.write2));
    RemoteStore::connect(cc)
        .unwrap()
        .index_bytes("acme", "api", "a.rs", b"fn alpha() {}", None)
        .unwrap();
    let addr = ts.running().unwrap().mcp_addr.unwrap();
    // `r` holds an unconstrained search permit: answered.
    let mut c = McpHttpClient::with_token(addr, Some(&f.read));
    let v = c.request(
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"text": "alpha"}}),
    );
    assert!(v.to_string().contains("alpha"), "{v}");
    // `w` holds none: an isError result with permission_denied, HTTP 200
    // (the request helper asserts the 200).
    let mut c = McpHttpClient::with_token(addr, Some(&f.write));
    let v = c.request(
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"text": "alpha"}}),
    );
    assert_eq!(v["result"]["isError"], true, "{v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("permission_denied") && !text.contains("acme"),
        "{text}"
    );
}

/// The leader refuses a malformed `Index` stream (a `File` before the
/// `Header`, or a second `Header`) before anything is written, with or
/// without policies.
#[test]
fn a_malformed_index_stream_is_refused_before_any_write() {
    use graph_proto::pb;
    let e = env_with(POLICIES, false);
    let ep = e.ts.endpoint();
    let token = e.f.write2.clone();
    let header = || pb::IndexRequest {
        msg: Some(pb::index_request::Msg::Header(pb::IndexHeader {
            org: "acme".into(),
            repo: "api".into(),
            options: None,
        })),
    };
    let file = |p: &str| pb::IndexRequest {
        msg: Some(pb::index_request::Msg::File(pb::FileBytes {
            path: p.into(),
            bytes: b"fn a() {}".to_vec(),
            ..Default::default()
        })),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (what, msgs) in [
        ("file before header", vec![file("a.rs"), header()]),
        (
            "second header",
            vec![header(), file("a.rs"), header(), file("b.rs")],
        ),
    ] {
        let (ep, token) = (ep.clone(), token.clone());
        let r = rt.block_on(async move {
            let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut c =
                pb::write_client::WriteClient::with_interceptor(ch, graph_proto::SendVersion);
            let mut req = tonic::Request::new(tokio_stream::iter(msgs));
            req.metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            c.index(req).await
        });
        let st = r.expect_err(what);
        assert_eq!(st.code(), tonic::Code::FailedPrecondition, "{what}: {st:?}");
        assert!(st.message().contains("malformed message"), "{what}: {st:?}");
        let w2 = client(&e, &e.f.write2);
        assert!(
            w2.describe(None, None).unwrap().is_empty(),
            "{what}: nothing written"
        );
    }
}

/// ADR D1: a write token is in `Role::"read"` too (a `forbid` on the read
/// role therefore applies to it).
#[test]
fn role_groups_follow_d1() {
    let e = env_with(
        r#"permit(principal in Role::"read", action == Action::"status", resource);"#,
        false,
    );
    client(&e, &e.f.write).admin_status().unwrap();
    client(&e, &e.f.read).admin_status().unwrap();
}
