//! ADR 0013 PR 3, story 74a, against an in-process server: search, symbol
//! search and the describe family scoped by `--auth-policies` (D5).
//!
//! Each conformance scope is written as the equivalent policy file for a
//! `read` token; store A is the server (seeded by an unrestricted writer),
//! store B an embedded index of only the visible repos in A's order, and
//! the scoped differential's strong form and named probes run through
//! `RemoteStore` (the scope is never sent: the server derives it).
use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::Extractor;
use graph_server::testing::auth::{stanza, AuthFixture};
use graph_server::testing::TestServer;
use graph_store::{conformance, Query, ReadScope, ScopeRules, Store, StoreError, StoreRead};

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// `rules` as policies for `Token::"r"` on `read` and `search`.
fn policy_text(rules: &ScopeRules) -> String {
    let mut out = String::from(
        "permit(principal == Token::\"w2\", action, resource);\n\
         permit(principal == Token::\"r\", action == Action::\"status\", resource);\n",
    );
    let reads = "action in [Action::\"read\", Action::\"search\"]";
    let mut line = |effect: &str, res: String| {
        out.push_str(&format!(
            "{effect}(principal == Token::\"r\", {reads}, {res});\n"
        ));
    };
    if rules.any {
        line("permit", "resource".into());
    }
    if rules.deny_all {
        line("forbid", "resource".into());
    }
    for o in &rules.orgs {
        line("permit", format!("resource in Org::\"{o}\""));
    }
    for o in &rules.org_only {
        line("permit", format!("resource == Org::\"{o}\""));
    }
    for (o, r) in &rules.repos {
        line("permit", format!("resource == Repo::\"{o}/{r}\""));
    }
    for o in &rules.deny_orgs {
        line("forbid", format!("resource in Org::\"{o}\""));
    }
    for o in &rules.deny_org_only {
        line("forbid", format!("resource == Org::\"{o}\""));
    }
    for (o, r) in &rules.deny_repos {
        line("forbid", format!("resource == Repo::\"{o}/{r}\""));
    }
    out
}

fn rules_of(scope: &ReadScope) -> ScopeRules {
    match scope {
        ReadScope::All => ScopeRules {
            any: true,
            ..Default::default()
        },
        ReadScope::Only(v) => {
            assert_eq!(v.len(), 1, "one principal's rules");
            v[0].clone()
        }
    }
}

struct Env {
    f: AuthFixture,
    _d: tempfile::TempDir,
    ts: TestServer,
}

fn env(policies: &str) -> Env {
    let f = AuthFixture::new();
    f.write_tokens(
        &[
            stanza("w2", "write", &f.write2, None),
            stanza("r", "read", &f.read, None),
        ]
        .join("\n"),
    );
    let pol = f.dir.path().join("policies.cedar");
    std::fs::write(&pol, policies).unwrap();
    let d = tempfile::tempdir().unwrap();
    let mut cfg = f.config();
    cfg.policies = Some(pol);
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), move |c| c.auth = cfg);
    Env { f, _d: d, ts }
}

fn client(e: &Env, token: &str, mode: ReadMode) -> RemoteStore {
    let mut cfg = ClientConfig::new(e.ts.endpoint());
    cfg.credentials = Some(e.f.creds(token));
    cfg.read_mode = mode;
    RemoteStore::connect(cfg).unwrap()
}

/// The scoped differential through `RemoteStore`, for a subset of the
/// fixed scopes (each is a server start and a full seed), in both read
/// modes: `--read linearizable` with a scoped token answers the same.
#[test]
fn scoped_reads_over_the_wire_equal_an_index_of_the_visible_repos() {
    let wanted = [
        "nothing",
        "org o1 minus o1/r2",
        "o2/r1 only (r1 shared with hidden o1/r1)",
        "org o3 granted, its only repo forbidden; plus o4",
        "any minus org o2",
    ];
    for (name, scope) in conformance::fixed_scopes() {
        if !wanted.contains(&name) {
            continue;
        }
        eprintln!("scope: {name}");
        let e = env(&policy_text(&rules_of(&scope)));
        let w2 = client(&e, &e.f.write2, ReadMode::Local);
        let d = tempfile::tempdir().unwrap();
        let b = graph_store::open_store(&d.path().join("b.redb"), rust()).unwrap();
        conformance::seed_scoped(&w2, &*b, &scope);
        for mode in [ReadMode::Local, ReadMode::Linearizable] {
            let r = client(&e, &e.f.read, mode);
            conformance::scoped_queries(&r, &*b, &scope, false);
        }
    }
}

/// A scope on a query sent through `RemoteStore` is refused, never sent
/// or dropped: the server derives the scope from the caller.
#[test]
fn a_client_side_scope_is_refused() {
    let e = env(&policy_text(&ScopeRules {
        any: true,
        ..Default::default()
    }));
    let r = client(&e, &e.f.read, ReadMode::Local);
    let mut q = Query::new("x");
    q.scope = ReadScope::nothing();
    assert!(matches!(r.search(&q), Err(StoreError::Rejected(_))));
    let mut sq = graph_store::SymbolQuery::new("x");
    sq.scope = ReadScope::nothing();
    assert!(matches!(
        r.search_symbols(&sq),
        Err(StoreError::Rejected(_))
    ));
}

/// Named-read refusals are counted as `deny` decisions (D9).
#[test]
fn a_named_read_refusal_is_counted() {
    let e = env(&policy_text(&ScopeRules {
        orgs: ["a".to_string()].into(),
        ..Default::default()
    }));
    let w2 = client(&e, &e.f.write2, ReadMode::Local);
    w2.index_bytes("b", "x", "f.txt", b"hello", None).unwrap();
    let r = client(&e, &e.f.read, ReadMode::Local);
    let deny = |e: &Env| -> u64 {
        e.ts.running()
            .unwrap()
            .auth()
            .authz_decisions()
            .into_iter()
            .filter(|(a, d, _)| *a == "read" && *d == "deny")
            .map(|(_, _, n)| n)
            .sum()
    };
    let before = deny(&e);
    assert!(matches!(
        r.describe(Some("b"), None),
        Err(StoreError::PermissionDenied { .. })
    ));
    assert_eq!(deny(&e), before + 1);
    // An aggregate read is filtered silently: no decision counted.
    assert!(r.describe(None, None).unwrap().is_empty());
    assert_eq!(deny(&e), before + 1);
}
