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
    // `w` holds none. Decided once, by the read rule (PR 2 review): an
    // aggregate search answers empty; one naming an existing repo is an
    // isError result with permission_denied, HTTP 200 (the request helper
    // asserts the 200).
    let mut c = McpHttpClient::with_token(addr, Some(&f.write));
    let v = c.request(
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"text": "alpha"}}),
    );

    assert_eq!(v["result"]["isError"], false, "{v}");
    let decisions = |a: &str| {
        ts.running()
            .unwrap()
            .auth()
            .authz_decisions()
            .into_iter()
            .filter(|(x, _, _)| a.is_empty() || *x == a)
            .map(|(_, _, n)| n)
            .sum::<u64>()
    };
    // Every action (the tool names its repo through a `read` first).
    let before = decisions("");
    let v = c.request(
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"text": "alpha", "org": "acme", "repo": "api"}}),
    );
    assert_eq!(decisions(""), before + 1, "one decision per tool call: {v}");
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

/// ADR 0013 D2 (Q6): a revoking reload stops an open `Index` stream at the
/// next file; the files after it are refused and nothing of the stream is
/// applied (its pending batch never commits).
#[test]
fn a_revoking_reload_stops_an_index_stream_at_the_next_file() {
    use graph_proto::pb;
    use std::time::Duration;
    let e = env_with(POLICIES, false);
    let ep = e.ts.endpoint();
    let token = e.f.write.clone();
    let file = |p: &str| pb::IndexRequest {
        msg: Some(pb::index_request::Msg::File(pb::FileBytes {
            path: p.into(),
            bytes: format!("fn f_{}() {{}}", p.replace('.', "_")).into_bytes(),
            ..Default::default()
        })),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<pb::IndexRequest>(8);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let call = runtime.spawn(async move {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut c = pb::write_client::WriteClient::with_interceptor(ch, graph_proto::SendVersion);
        let mut req = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        req.metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        c.index(req).await.map(|r| r.into_inner())
    });
    runtime.block_on(async {
        tx.send(pb::IndexRequest {
            msg: Some(pb::index_request::Msg::Header(pb::IndexHeader {
                org: "acme".into(),
                repo: "web".into(),
                options: None,
            })),
        })
        .await
        .unwrap();
        tx.send(file("a.rs")).await.unwrap();
        tx.send(file("b.rs")).await.unwrap();
    });
    // The first files are admitted; now the grant goes away (the token
    // stays in the file: only the policy changes).
    std::thread::sleep(Duration::from_millis(400));
    std::fs::write(
        &e.policies,
        POLICIES.replace(
            "resource == Repo::\"acme/web\"",
            "resource == Repo::\"acme/other\"",
        ),
    )
    .unwrap();
    assert_eq!(reload(&e).0, Some(Reload::Reloaded));
    runtime.block_on(async {
        for p in ["c.rs", "d.rs"] {
            // The server may already have answered and closed the stream.
            let _ = tx.send(file(p)).await;
        }
        drop(tx);
    });
    let r = runtime.block_on(call).unwrap();
    match r {
        Err(st) => {
            assert_eq!(st.code(), tonic::Code::PermissionDenied, "{st:?}");
            assert!(st.message().contains("action `index`"), "{st:?}");
        }
        Ok(resp) => panic!(
            "the stream was not stopped: {} files applied",
            resp.results.len()
        ),
    }
    // Nothing of the stream was applied: not the refused files, and not
    // the admitted ones still pending in the uncommitted batch.
    let w2 = client(&e, &e.f.write2);
    for p in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        assert_eq!(w2.file_tokens("acme", "web", p).unwrap(), None, "{p}");
    }
    assert!(w2.describe(Some("acme"), Some("web")).unwrap().is_empty());
}

/// D4 end to end: a `read` token with a permit for everything is still
/// refused writes and `admin`/`snapshot` cluster actions by the server.
#[test]
fn the_role_ceiling_holds_end_to_end() {
    let e = env_with(
        r#"permit(principal == Token::"r", action, resource);"#,
        false,
    );
    let r = client(&e, &e.f.read);
    assert!(matches!(
        r.index_bytes("acme", "web", "a.rs", b"fn a() {}", None),
        Err(StoreError::PermissionDenied { .. })
    ));
    assert!(matches!(
        r.delete_scope("acme", None, true),
        Err(StoreError::PermissionDenied { .. })
    ));
    assert!(matches!(
        r.vacuum(),
        Err(StoreError::PermissionDenied { .. })
    ));
    assert!(matches!(
        r.admin_compact(),
        Err(StoreError::PermissionDenied { .. })
    ));
    assert!(matches!(
        r.admin_trigger_snapshot(None),
        Err(StoreError::PermissionDenied { .. })
    ));
    // Within the ceiling the permit applies.
    r.admin_status().unwrap();
    r.admin_list_backups().ok();
    assert!(r.roots().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// ADR 0013 PR 2 (story 73): forwarded calls, D7.

/// One raw unary call with `md` appended as given (so a header can repeat,
/// or carry bytes that are not ASCII); the status code (`Ok` for success).
fn raw_call(ep: &str, path: &str, md: &[(&str, Vec<u8>)]) -> tonic::Code {
    let ep = ep.to_string();
    let path = path.to_string();
    let md: Vec<(String, Vec<u8>)> = md.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut g = tonic::client::Grpc::new(ch);
        g.ready().await.unwrap();
        let mut h = tonic::codegen::http::HeaderMap::new();
        h.insert(
            graph_proto::PROTOCOL_VERSION_HEADER,
            graph_proto::PROTOCOL_VERSION.to_string().parse().unwrap(),
        );
        for (k, v) in &md {
            h.append(
                tonic::codegen::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                tonic::codegen::http::HeaderValue::from_bytes(v).unwrap(),
            );
        }
        let mut req = tonic::Request::new(());
        *req.metadata_mut() = tonic::metadata::MetadataMap::from_headers(h);
        let codec: tonic_prost::ProstCodec<(), ()> = Default::default();
        let p = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(path).unwrap();
        match g.unary(req, p, codec).await {
            Ok(_) => tonic::Code::Ok,
            Err(st) => st.code(),
        }
    })
}

/// Raw metadata: names and value bytes, in order.
type Headers = Vec<(&'static str, Vec<u8>)>;

fn h(k: &'static str, v: &str) -> (&'static str, Vec<u8>) {
    (k, v.as_bytes().to_vec())
}

const BY: &str = "mg-forwarded-by";
const ROLE: &str = "mg-forwarded-role";
const ID: &str = "mg-forwarded-token-id";
const ACTION: &str = "mg-forwarded-action";
const INDEX_FILE: &str = "/memory_graph.v1.Write/IndexFile";
const READ_INDEX: &str = "/memory_graph.v1.Admin/ReadIndex";

/// D7 header parsing: every `mg-forwarded-*` header exactly once, ASCII,
/// non-empty, at most 128 bytes, well formed; anything else is
/// `PERMISSION_DENIED`, never another principal. A valid set passes (the
/// empty `IndexFile` then fails on its missing file, a protocol error, `FAILED_PRECONDITION`), which
/// shows each refusal is the header's.
#[test]
fn forwarded_headers_are_parsed_strictly() {
    use tonic::Code;
    let e = env_with(POLICIES, false);
    let ep = e.ts.endpoint();
    let secret = (
        "authorization",
        format!("Bearer {}", e.f.secret).into_bytes(),
    );
    let base = |id: &str| vec![secret.clone(), h(BY, "2"), h(ROLE, "write"), h(ID, id)];
    // `w2` may index anywhere (seed): the valid forward reaches the body.
    assert_eq!(
        raw_call(&ep, INDEX_FILE, &base("w2")),
        Code::FailedPrecondition
    );
    let long = "x".repeat(129);
    let cases: Vec<(&str, Headers)> = vec![
        (
            "no token id",
            vec![secret.clone(), h(BY, "2"), h(ROLE, "write")],
        ),
        ("no role", vec![secret.clone(), h(BY, "2"), h(ID, "w2")]),
        ("repeated token id", {
            let mut v = base("w2");
            v.push(h(ID, "w2"));
            v
        }),
        ("repeated role", {
            let mut v = base("w2");
            v.push(h(ROLE, "write"));
            v
        }),
        ("repeated by", {
            let mut v = base("w2");
            v.push(h(BY, "3"));
            v
        }),
        ("empty token id", base("")),
        ("oversized token id", base(&long)),
        (
            "non-ASCII token id",
            vec![
                secret.clone(),
                h(BY, "2"),
                h(ROLE, "write"),
                (ID, b"w\xe92".to_vec()),
            ],
        ),
        ("blank around the id", base("w2 ")),
        ("unknown id", base("nobody")),
        (
            "bad role",
            vec![secret.clone(), h(BY, "2"), h(ROLE, "admin"), h(ID, "w2")],
        ),
        (
            "bad by",
            vec![secret.clone(), h(BY, "two"), h(ROLE, "write"), h(ID, "w2")],
        ),
        ("bad action", {
            let mut v = base("w2");
            v.push(h(ACTION, "status"));
            v
        }),
        ("repeated action", {
            let mut v = base("w2");
            v.push(h(ACTION, "read"));
            v.push(h(ACTION, "read"));
            v
        }),
    ];
    for (what, md) in &cases {
        assert_eq!(
            raw_call(&ep, INDEX_FILE, md),
            Code::PermissionDenied,
            "{what}"
        );
    }
    // Exactly 128 bytes is accepted by the parser (then unknown: denied by
    // the lookup, like any id the leader does not know).
    assert_eq!(
        graph_server::auth::parse_forwarded(&{
            let mut m = tonic::codegen::http::HeaderMap::new();
            for (k, v) in [(BY, "1"), (ROLE, "read"), (ID, &*"y".repeat(128))] {
                m.insert(k, v.parse().unwrap());
            }
            m
        })
        .map(|f| f.token_id.len()),
        Ok(128)
    );
    // ... and 129 bytes is refused by the parser itself, whatever the id
    // (the lookup alone would also refuse an unknown long id).
    assert_eq!(
        graph_server::auth::parse_forwarded(&{
            let mut m = tonic::codegen::http::HeaderMap::new();
            for (k, v) in [(BY, "1"), (ROLE, "read"), (ID, &*"y".repeat(129))] {
                m.insert(k, v.parse().unwrap());
            }
            m
        })
        .map(|f| f.token_id.len()),
        Err("a forwarded header is too long")
    );
    // The cluster secret alone (no valid token id) is never enough, for
    // every action class: a read, `status` (also a join-probe method),
    // a write, `admin` (a node-originated `Remove`, `--bootstrap-or-join`).
    let no_id = vec![secret.clone(), h(BY, "2"), h(ROLE, "write")];
    for p in [
        "/memory_graph.v1.Store/Get",
        "/memory_graph.v1.Admin/Status",
        "/memory_graph.v1.Admin/Members",
        INDEX_FILE,
        "/memory_graph.v1.Admin/Compact",
        "/memory_graph.v1.Admin/Remove",
        "/memory_graph.v1.Admin/TriggerSnapshot",
    ] {
        assert_eq!(raw_call(&ep, p, &no_id), Code::PermissionDenied, "{p}");
    }
    // The join probes (no `mg-forwarded-by`) still work with the secret.
    assert_eq!(
        raw_call(
            &ep,
            "/memory_graph.v1.Admin/Status",
            std::slice::from_ref(&secret)
        ),
        Code::Ok
    );
}

/// D7: the leader's role ceiling is its own role for the id intersected
/// with the forwarded role; a client token's forwarding headers are
/// ignored; the read-barrier marker makes `ReadIndex` part of the read,
/// honoured only on a cluster-secret call.
#[test]
fn the_leader_resolves_the_principal_and_the_barrier_marker() {
    use tonic::Code;
    let e = env_with(
        &(POLICIES.to_string()
            + r#"
@id("r-index")
permit(principal == Token::"r", action == Action::"index", resource);
"#),
        false,
    );
    let ep = e.ts.endpoint();
    let secret = (
        "authorization",
        format!("Bearer {}", e.f.secret).into_bytes(),
    );
    let fwd = |id: &str, role: &str| vec![secret.clone(), h(BY, "2"), h(ROLE, role), h(ID, id)];
    // `r` is `read` here: a forwarded `write` cannot raise it (the ceiling
    // is this node's role) ...
    assert_eq!(
        raw_call(&ep, INDEX_FILE, &fwd("r", "write")),
        Code::PermissionDenied
    );
    // ... and a forwarded `read` lowers `w2`'s `write`.
    assert_eq!(
        raw_call(&ep, INDEX_FILE, &fwd("w2", "read")),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, INDEX_FILE, &fwd("w2", "write")),
        Code::FailedPrecondition
    );
    // A client token's forwarding headers are ignored: `w` (no grant on the
    // empty repo) naming `w2` is still `w`; `w2` naming `w` is still `w2`.
    let bearer = |t: &str| ("authorization", format!("Bearer {t}").into_bytes());
    assert_eq!(
        raw_call(
            &ep,
            INDEX_FILE,
            &[
                bearer(&e.f.write),
                h(BY, "2"),
                h(ROLE, "write"),
                h(ID, "w2")
            ]
        ),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, INDEX_FILE, &[bearer(&e.f.write2), h(ID, "w")]),
        Code::FailedPrecondition
    );
    // `w2` reads and searches but has no `status`: a standalone ReadIndex is
    // refused, a forwarded barrier marked `read`/`search` is admitted, and
    // a client's own marker is ignored (still standalone).
    assert_eq!(
        raw_call(&ep, READ_INDEX, &[bearer(&e.f.write2)]),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, READ_INDEX, &[bearer(&e.f.write2), h(ACTION, "read")]),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, READ_INDEX, &fwd("w2", "write")),
        Code::PermissionDenied
    );
    for a in ["read", "search"] {
        let mut md = fwd("w2", "write");
        md.push(h(ACTION, a));
        assert_eq!(raw_call(&ep, READ_INDEX, &md), Code::Ok, "{a}");
    }
    // `r` holds `status`: its standalone ReadIndex passes.
    assert_eq!(raw_call(&ep, READ_INDEX, &[bearer(&e.f.read)]), Code::Ok);
    // D9: refusals are counted by action, and the detail names the action.
    let auth = e.ts.running().unwrap().auth();
    let n = |a: &str, d: &str| {
        auth.authz_decisions()
            .into_iter()
            .find(|(x, y, _)| *x == a && *y == d)
            .unwrap()
            .2
    };
    assert!(n("index", "deny") >= 1 && n("index", "allow") >= 1);
    assert!(n("status", "deny") >= 2);
    let w = client(&e, &e.f.write);
    match w.index_bytes("acme", "api", "a.rs", b"fn a() {}", None) {
        Err(StoreError::PermissionDenied { action, .. }) => assert_eq!(action, "index"),
        other => panic!("{other:?}"),
    }
}

/// D6/D9 for the MCP write tools: `index_path` and `prune` are decided on
/// their repo for the request's token (on this node, which proposes them
/// directly): a policy refusal is an `isError` result with
/// `permission_denied` and HTTP 200 and writes nothing; a role refusal
/// stays HTTP 403.
#[test]
fn mcp_write_tools_are_decided_per_repo() {
    use graph_server::testing::mcp_http::McpHttpClient;
    let f = AuthFixture::new();
    f.write_tokens(&grouped_tokens(&f));
    let pol = f.dir.path().join("policies.cedar");
    std::fs::write(&pol, POLICIES).unwrap();
    let d = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("web")).unwrap();
    std::fs::write(root.path().join("web/a.rs"), b"fn gamma_from_mcp() {}\n").unwrap();
    let mut cfg = f.config();
    cfg.policies = Some(pol);
    let root_path = root.path().to_path_buf();
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| {
        c.auth = cfg;
        let mut m = graph_server::mcp::McpConfig::new("127.0.0.1:0".parse().unwrap());
        m.index_root = Some(root_path);
        c.mcp = Some(m);
    });
    let addr = ts.running().unwrap().mcp_addr.unwrap();
    let call = |c: &mut McpHttpClient, name: &str, args: serde_json::Value| {
        c.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        )
    };
    let refused = |v: &serde_json::Value| {
        assert_eq!(v["result"]["isError"], true, "{v}");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("permission_denied"), "{text}");
        for leak in ["acme", "legal", "web-index", "seed"] {
            assert!(!text.contains(leak), "{text}");
        }
    };
    let mut w = McpHttpClient::with_token(addr, Some(&f.write));
    // `w` (web-team) may index acme/web, not acme/api.
    let v = call(
        &mut w,
        "index_path",
        serde_json::json!({"org": "acme", "repo": "api", "path": "web"}),
    );
    refused(&v);
    let v = call(
        &mut w,
        "index_path",
        serde_json::json!({"org": "acme", "repo": "web", "path": "web"}),
    );
    assert_eq!(v["result"]["structuredContent"]["indexed"], 1, "{v}");
    // Prune: `w` holds prune in acme but the forbid on acme/legal wins;
    // `w2` holds no prune at all.
    let v = call(
        &mut w,
        "prune",
        serde_json::json!({"org": "acme", "repo": "legal", "path": "web", "dry_run": true}),
    );
    refused(&v);
    let v = call(
        &mut w,
        "prune",
        serde_json::json!({"org": "acme", "repo": "web", "path": "web", "dry_run": true}),
    );
    assert_eq!(v["result"]["isError"], false, "{v}");
    let mut w2 = McpHttpClient::with_token(addr, Some(&f.write2));
    let v = call(
        &mut w2,
        "prune",
        serde_json::json!({"org": "acme", "repo": "web", "path": "web", "dry_run": true}),
    );
    refused(&v);
    // Only acme/web was written.
    let mut cc = ClientConfig::new(ts.endpoint());
    cc.credentials = Some(f.creds(&f.write2));
    let repos: Vec<String> = RemoteStore::connect(cc)
        .unwrap()
        .describe(None, None)
        .unwrap()
        .into_iter()
        .map(|r| format!("{}/{}", r.org, r.repo))
        .collect();
    assert_eq!(repos, ["acme/web"]);
    // A read token on a write tool: the role refusal, HTTP 403.
    let reader = McpHttpClient::with_token(addr, Some(&f.read));
    let r = reader.post(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "index_path", "arguments": {"org": "acme", "repo": "web", "path": "web"}}}));
    assert_eq!(r.status, 403, "{r:?}");
}

/// D8: `UploadSnapshot` replaces the store, so it is `admin`;
/// `TriggerSnapshot` is `snapshot`; `ListBackups` is `status`.
#[test]
fn snapshot_and_backup_actions() {
    use tonic::Code;
    let e = env_with(
        r#"@id("snap")
permit(principal == Token::"w", action == Action::"snapshot", resource == Cluster::"this");
@id("st")
permit(principal == Token::"w2", action == Action::"status", resource);"#,
        false,
    );
    let ep = e.ts.endpoint();
    let bearer = |t: &str| ("authorization", format!("Bearer {t}").into_bytes());
    let up = "/memory_graph.v1.Admin/UploadSnapshot";
    let trig = "/memory_graph.v1.Admin/TriggerSnapshot";
    let list = "/memory_graph.v1.Admin/ListBackups";
    // `w` may snapshot, but upload needs `admin`.
    assert_eq!(
        raw_call(&ep, up, &[bearer(&e.f.write)]),
        Code::PermissionDenied
    );
    assert_ne!(
        raw_call(&ep, trig, &[bearer(&e.f.write)]),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, list, &[bearer(&e.f.write)]),
        Code::PermissionDenied
    );
    // `w2` has `status` only.
    assert_eq!(
        raw_call(&ep, trig, &[bearer(&e.f.write2)]),
        Code::PermissionDenied
    );
    assert_ne!(
        raw_call(&ep, list, &[bearer(&e.f.write2)]),
        Code::PermissionDenied
    );
    assert_eq!(
        raw_call(&ep, up, &[bearer(&e.f.write2)]),
        Code::PermissionDenied
    );
}
/// D9: the `mg_authz_*` families exist and carry only their bounded labels
/// (`action`, `decision`, `result`, and `le` on the histogram): no token,
/// group, resource or policy id, whatever was decided.
#[test]
fn authz_metrics_carry_only_bounded_labels() {
    let e = env_with(POLICIES, false);
    let w = client(&e, &e.f.write);
    w.index_bytes("acme", "web", "a.rs", b"fn a() {}", None)
        .unwrap();
    denied(
        w.index_bytes("acme", "legal", "a.rs", b"fn a() {}", None),
        "index",
    );
    reload(&e);
    let text = client(&e, &e.f.read).admin_metrics().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines().filter(|l| l.starts_with("mg_authz_")) {
        let name = line.split(['{', ' ']).next().unwrap();
        seen.insert(
            name.trim_end_matches("_bucket")
                .trim_end_matches("_sum")
                .trim_end_matches("_count")
                .to_string(),
        );
        if let Some(labels) = line
            .split_once('{')
            .map(|(_, r)| r.split_once('}').unwrap().0)
        {
            for kv in labels.split(',') {
                let k = kv.split('=').next().unwrap();
                assert!(
                    ["action", "decision", "result", "le"].contains(&k),
                    "{line}"
                );
            }
        }
        for leak in ["acme", "web", "legal", "seed", "readers", "\"w\"", "\"w2\""] {
            assert!(!line.contains(leak), "{line}");
        }
    }
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        [
            "mg_authz_decision_seconds",
            "mg_authz_decisions_total",
            "mg_authz_policies",
            "mg_authz_reload_total"
        ]
    );
    assert!(
        text.contains("mg_authz_decisions_total{action=\"index\",decision=\"deny\"} 1"),
        "{text}"
    );
    assert!(text.contains(&format!("mg_authz_policies {}", 5)), "{text}");
}

/// D4 + D7: the role a forwarded principal is decided with is the
/// intersection, also for policies on `Role::"write"`: a `write` token
/// forwarded as `read` is not in `Role::"write"` at the leader.
#[test]
fn a_forwarded_principal_is_decided_with_the_intersected_role() {
    use tonic::Code;
    let e = env_with(
        r#"@id("writers-status")
permit(principal in Role::"write", action == Action::"status", resource);"#,
        false,
    );
    let ep = e.ts.endpoint();
    let secret = (
        "authorization",
        format!("Bearer {}", e.f.secret).into_bytes(),
    );
    let fwd = |role: &str| vec![secret.clone(), h(BY, "2"), h(ROLE, role), h(ID, "w2")];
    // A standalone `ReadIndex` is `status` (and needs only the `read` role).
    assert_eq!(raw_call(&ep, READ_INDEX, &fwd("write")), Code::Ok);
    assert_eq!(
        raw_call(&ep, READ_INDEX, &fwd("read")),
        Code::PermissionDenied
    );
}

/// ADR 0013 D7, owner decision 2026-11-21: a node's own calls (the
/// `--bootstrap-or-join` removal of its lost incarnation) present
/// `--auth-node-token-id`, a token of the node's own file, and the leader
/// decides `admin` on `Cluster::"this"` for it: allowed with an admin
/// grant, refused without one, refused for an id the leader does not know.
#[test]
fn a_nodes_own_call_presents_its_node_token() {
    use graph_server::auth::{Auth, NodeHeaders};
    let remove_code = |e: &Env, node_tokens: &std::path::Path, id: &str| {
        let mut cfg = e.f.config();
        cfg.tokens = Some(node_tokens.to_path_buf());
        cfg.node_token_id = Some(id.to_string());
        let auth = Auth::load(&cfg, true).unwrap();
        let ep = e.ts.endpoint();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let ch = tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let headers = NodeHeaders {
                auth,
                forwarded_by: Some(9),
            };
            let mut c = graph_proto::pb::admin_client::AdminClient::with_interceptor(ch, headers);
            match c
                .remove(graph_proto::pb::RemoveRequest {
                    node_id: 9,
                    force: true,
                })
                .await
            {
                Ok(_) => tonic::Code::Ok,
                Err(st) => st.code(),
            }
        })
    };
    let admin = r#"@id("node-admin")
permit(principal == Token::"w2", action == Action::"admin", resource == Cluster::"this");"#;
    // Granted: the call passes authorization (this `--db` server then
    // refuses the removal itself, which is not a permission refusal).
    let e = env_with(admin, false);
    let code = remove_code(&e, &e.f.tokens, "w2");
    assert_ne!(code, tonic::Code::PermissionDenied, "{code:?}");
    assert_ne!(code, tonic::Code::Unauthenticated, "{code:?}");
    // No admin grant for the node token: refused.
    let e = env_with(POLICIES, false);
    assert_eq!(
        remove_code(&e, &e.f.tokens, "w2"),
        tonic::Code::PermissionDenied
    );
    // An id in the node's own file that the leader does not know: refused.
    let e = env_with(admin, false);
    let own = e.f.dir.path().join("node-tokens.toml");
    let node_token = graph_server::auth::new_credential("mgt_").to_string();
    std::fs::write(&own, stanza("node9", "write", &node_token, None)).unwrap();
    assert_eq!(
        remove_code(&e, &own, "node9"),
        tonic::Code::PermissionDenied
    );
    // The id must be in the node's own token file.
    let mut cfg = e.f.config();
    cfg.node_token_id = Some("nobody".into());
    assert!(Auth::load(&cfg, true).is_err());
}
