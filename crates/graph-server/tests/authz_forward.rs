//! ADR 0013 PR 2 (story 73), D7 on a real three-node cluster: the follower
//! decides first with its own pair, the leader re-checks the forwarded
//! principal with its own token file (role and groups from there, never
//! from the follower) and its own policies; the effective permission is
//! the intersection, so a grant takes effect once it is on both nodes and
//! a revocation once it is on either. The header parser, the role ceiling
//! and the read-barrier marker are pinned against one server in
//! `tests/authz.rs`.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_server::testing::auth::{stanza, AuthFixture};
use graph_server::testing::ClusterTestbed;
use graph_store::{Store, StoreError, StoreRead};
use std::path::PathBuf;
use std::time::Duration;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// The tokens of the test: `w` administers, `ix` indexes, `r` and `lin`
/// read, `ghost` is known to the followers only.
struct Tokens {
    f: AuthFixture,
    lin: String,
    ghost: String,
}

const BASE: &str = r#"
@id("admin")
permit(principal == Token::"w", action, resource);
@id("web")
permit(principal in Group::"web", action == Action::"index", resource == Repo::"acme/web");
@id("api")
permit(principal in Group::"api", action == Action::"index", resource == Repo::"acme/api");
@id("r-index")
permit(principal == Token::"r", action == Action::"index", resource);
@id("ghost")
permit(principal == Token::"ghost", action == Action::"index", resource);
@id("lin")
permit(principal == Token::"lin", action in [Action::"read", Action::"search"], resource == Repo::"acme/web");
"#;

/// Only on the followers: `ix` may run membership changes there.
const FOLLOWER_ONLY: &str = r#"
@id("follower-admin")
permit(principal == Token::"ix", action == Action::"admin", resource == Cluster::"this");
"#;

/// Only on the leader: the follower refuses `acme/lo` before forwarding.
const LEADER_ONLY: &str = r#"
@id("leader-only")
permit(principal == Token::"ix", action == Action::"index", resource == Repo::"acme/lo");
"#;

impl Tokens {
    fn new() -> Self {
        let t = || graph_server::auth::new_credential("mgt_").to_string();
        Self {
            f: AuthFixture::new(),
            lin: t(),
            ghost: t(),
        }
    }

    /// The leader's token file: `ix` in `web` only, `r` is `read`.
    fn leader_tokens(&self) -> String {
        let f = &self.f;
        [
            stanza("w", "write", &f.write, None),
            stanza("ix", "write", &f.write2, None) + "groups = [\"web\"]\n",
            stanza("r", "read", &f.read, None),
            stanza("lin", "read", &self.lin, None),
        ]
        .join("\n")
    }

    /// A follower's: `ix` also in `api`, `r` raised to `write`, and `ghost`.
    fn follower_tokens(&self) -> String {
        let f = &self.f;
        [
            stanza("w", "write", &f.write, None),
            stanza("ix", "write", &f.write2, None) + "groups = [\"web\", \"api\"]\n",
            stanza("r", "write", &f.read, None),
            stanza("lin", "read", &self.lin, None),
            stanza("ghost", "write", &self.ghost, None),
        ]
        .join("\n")
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.f.dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        p
    }
}

fn client(tb: &ClusterTestbed, id: u64, token: &str, t: &Tokens) -> RemoteStore {
    let mut cfg = ClientConfig::new(tb.node(id).endpoint());
    cfg.write_deadline = Duration::from_secs(15);
    cfg.credentials = Some(t.f.creds(token));
    RemoteStore::connect(cfg).unwrap()
}

fn denied<T: std::fmt::Debug>(r: Result<T, StoreError>, what: &str) {
    match r {
        Err(StoreError::PermissionDenied { action, msg, .. }) => {
            assert_eq!(action, "index", "{what}");
            for leak in ["acme", "web", "api", "ghost", "leader-only"] {
                assert!(!msg.contains(leak), "{what}: {msg}");
            }
        }
        other => panic!("{what}: expected PermissionDenied, got {other:?}"),
    }
}

fn index_decisions(tb: &ClusterTestbed, id: u64) -> u64 {
    tb.node(id)
        .running()
        .unwrap()
        .auth()
        .authz_decisions()
        .into_iter()
        .filter(|(a, _, _)| *a == "index")
        .map(|(_, _, n)| n)
        .sum()
}

fn repos(s: &RemoteStore) -> Vec<String> {
    s.describe(None, None)
        .unwrap()
        .into_iter()
        .map(|r| format!("{}/{}", r.org, r.repo))
        .collect()
}

#[test]
fn forwarded_calls_are_rechecked_by_the_leader() {
    let t = Tokens::new();
    let leader_tokens = t.file("leader.toml", &t.leader_tokens());
    let follower_tokens = t.file("follower.toml", &t.follower_tokens());
    let leader_pol = t.file("leader.cedar", &(BASE.to_string() + LEADER_ONLY));
    let follower_pol = t.file("follower.cedar", &(BASE.to_string() + FOLLOWER_ONLY));
    let mut tb = ClusterTestbed::with_config(3, rust(), |id, c| {
        c.auth = t.f.config();
        let (tok, pol) = if id == 1 {
            (&leader_tokens, &leader_pol)
        } else {
            (&follower_tokens, &follower_pol)
        };
        c.auth.tokens = Some(tok.clone());
        c.auth.policies = Some(pol.clone());
    });
    tb.set_client_credentials(Some(t.f.creds(&t.f.write)));
    tb.form();
    let leader = tb.leader();
    assert_eq!(leader, 1, "node 1 bootstrapped and leads");
    let fol = 2;
    let src = b"fn a() {}";
    let ix = client(&tb, fol, &t.f.write2, &t);

    // Allowed on both nodes: applied, on every replica (whose policy files
    // differ: apply is never policy-checked).
    ix.index_bytes("acme", "web", "a.rs", src, None).unwrap();
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(10));
    for id in [1, 2, 3] {
        assert_eq!(
            repos(&client(&tb, id, &t.f.write, &t)),
            ["acme/web"],
            "node {id}"
        );
    }

    // Denied on the follower: refused before forwarding, the leader decides
    // nothing.
    let before = index_decisions(&tb, leader);
    denied(
        ix.index_bytes("acme", "lo", "a.rs", src, None),
        "leader-only grant",
    );
    assert_eq!(
        index_decisions(&tb, leader),
        before,
        "no call reached the leader"
    );

    // Allowed on the follower (its file puts `ix` in `api`), denied on the
    // leader, which takes the groups from its own file.
    denied(
        ix.index_bytes("acme", "api", "a.rs", src, None),
        "follower-only group",
    );
    assert!(
        index_decisions(&tb, leader) > before,
        "the leader decided it"
    );
    // The follower's `write` for `r` does not raise the leader's `read`.
    let r = client(&tb, fol, &t.f.read, &t);
    match r.index_bytes("acme", "r", "a.rs", src, None) {
        Err(StoreError::PermissionDenied { .. }) => {}
        other => panic!("role ceiling at the leader: {other:?}"),
    }
    // A forwarded `Index` stream is re-checked per file at the leader too.
    let files: Vec<graph_store::BatchFile<'_>> = ["s1.rs", "s2.rs"]
        .iter()
        .map(|p| graph_store::BatchFile {
            path: p,
            bytes: src,
            ..Default::default()
        })
        .collect();
    let opts = graph_store::IndexOptions::default();
    for r in ix.index_batch("acme", "web", &files, opts).unwrap() {
        r.unwrap();
    }
    denied(
        ix.index_batch("acme", "api", &files, opts).map(|_| ()),
        "a forwarded stream, follower-only group",
    );
    // A forwarded membership change the follower allows (`ix` holds
    // `admin` there) is refused by the leader, which grants it nothing.
    match ix.admin_promote(3) {
        Err(StoreError::PermissionDenied { action, .. }) => assert_eq!(action, "admin"),
        other => panic!("admin at the leader: {other:?}"),
    }
    // A token only the follower knows is denied at the leader (Q7).
    let ghost = client(&tb, fol, &t.ghost, &t);
    denied(
        ghost.index_bytes("acme", "g", "a.rs", src, None),
        "unknown at the leader",
    );
    // Nothing of that was written anywhere.
    let w1 = client(&tb, leader, &t.f.write, &t);
    assert_eq!(repos(&w1), ["acme/web"]);

    // A scoped reader (ADR 0013 D5: `lin` reads `acme/web` only) keeps
    // `--read linearizable` through a follower: the forwarded barrier is
    // marked with the read's action, not `status` (which `lin` does not
    // hold), and the read is scoped on the follower.
    let mut cfg = ClientConfig::new(tb.node(fol).endpoint());
    cfg.credentials = Some(t.f.creds(&t.lin));
    cfg.read_mode = graph_client::ReadMode::Linearizable;
    let lin = RemoteStore::connect(cfg).unwrap();
    assert_eq!(repos(&lin), ["acme/web"]);
    let hits = lin.search(&graph_store::Query::new("a")).unwrap();
    assert!(!hits.is_empty());
    assert!(hits
        .iter()
        .all(|h| h.org == "acme" && h.repo.as_deref() == Some("web")));
    let mut q = graph_store::Query::new("a");
    q.org = Some("acme".into());
    assert_eq!(lin.search(&q).unwrap(), hits);
    assert!(matches!(
        lin.admin_status(),
        Err(StoreError::PermissionDenied { .. })
    ));

    // `cluster status` shows the differing policy hashes.
    let h1 = w1.admin_status().unwrap().policy_hash;
    let h2 = client(&tb, fol, &t.f.write, &t)
        .admin_status()
        .unwrap()
        .policy_hash;
    assert_eq!(h1.len(), 64);
    assert_ne!(h1, h2, "different pairs, different hashes");

    // A revocation on either node takes effect: drop `web` on the leader
    // only; the follower still allows, the leader now refuses.
    std::fs::write(
        &leader_pol,
        BASE.replace(
            "permit(principal in Group::\"web\"",
            "forbid(principal in Group::\"web\"",
        ),
    )
    .unwrap();
    tb.node(leader).running().unwrap().auth().reload_now();
    denied(
        ix.index_bytes("acme", "web", "b.rs", src, None),
        "revoked on the leader",
    );
    assert_eq!(repos(&w1), ["acme/web"]);
    assert!(w1.file_tokens("acme", "web", "b.rs").unwrap().is_none());

    // A forwarded `TransferLeader`: the leader asks the target to campaign
    // on behalf of the same principal (the forwarded id travels on), which
    // the target re-checks with its own files.
    let w2 = client(&tb, fol, &t.f.write, &t);
    w2.admin_transfer_leader(3).unwrap();
    assert_eq!(tb.wait_leader(Duration::from_secs(10)), 3);
}

/// revocation is not retroactive to committed log entries. An
/// entry admitted before a revocation is applied by a replica that catches
/// up after the revocation landed on every node.
#[test]
fn revocation_is_not_retroactive_to_log_entries() {
    let t = Tokens::new();
    let lt = t.file("leader.toml", &t.leader_tokens());
    let ft = t.file("follower.toml", &t.follower_tokens());
    let lp = t.file("leader.cedar", BASE);
    let fp = t.file("follower.cedar", BASE);
    let mut tb = ClusterTestbed::with_config(3, rust(), |id, c| {
        c.auth = t.f.config();
        let (tok, pol) = if id == 1 { (&lt, &lp) } else { (&ft, &fp) };
        c.auth.tokens = Some(tok.clone());
        c.auth.policies = Some(pol.clone());
    });
    tb.set_client_credentials(Some(t.f.creds(&t.f.write)));
    tb.form();
    assert_eq!(tb.leader(), 1);
    tb.node_mut(3).stop();
    let ix = client(&tb, 2, &t.f.write2, &t);
    ix.index_bytes("acme", "web", "a.rs", b"fn a() {}", None)
        .unwrap();
    let committed = tb.leader_last_log_index();
    let revoked = BASE.replace(
        "permit(principal in Group::\"web\"",
        "forbid(principal in Group::\"web\"",
    );
    std::fs::write(&lp, &revoked).unwrap();
    std::fs::write(&fp, &revoked).unwrap();
    for id in [1, 2] {
        tb.node(id).running().unwrap().auth().reload_now();
    }
    denied(
        ix.index_bytes("acme", "web", "b.rs", b"fn b() {}", None),
        "after revocation",
    );
    tb.node_mut(3).restart();
    tb.wait_applied(committed, Duration::from_secs(20));
    let n3 = client(&tb, 3, &t.f.write, &t);
    assert!(
        n3.file_tokens("acme", "web", "a.rs").unwrap().is_some(),
        "applied after revocation"
    );
    assert!(n3.file_tokens("acme", "web", "b.rs").unwrap().is_none());
}

/// reloads on the leader racing forwarded writes. Every write
/// either lands on all replicas or is refused with PermissionDenied(index);
/// after the final revoking reload returns, nothing is admitted.
#[test]
fn reload_racing_forwarded_writes() {
    let t = Tokens::new();
    let lt = t.file("leader.toml", &t.leader_tokens());
    let ft = t.file("follower.toml", &t.follower_tokens());
    let lp = t.file("leader.cedar", BASE);
    let fp = t.file("follower.cedar", BASE);
    let mut tb = ClusterTestbed::with_config(3, rust(), |id, c| {
        c.auth = t.f.config();
        let (tok, pol) = if id == 1 { (&lt, &lp) } else { (&ft, &fp) };
        c.auth.tokens = Some(tok.clone());
        c.auth.policies = Some(pol.clone());
    });
    tb.set_client_credentials(Some(t.f.creds(&t.f.write)));
    tb.form();
    assert_eq!(tb.leader(), 1);
    let revoked = BASE.replace(
        "permit(principal in Group::\"web\"",
        "forbid(principal in Group::\"web\"",
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ok = std::thread::scope(|s| {
        let st = std::sync::Arc::clone(&stop);
        let ix = client(&tb, 2, &t.f.write2, &t);
        let h = s.spawn(move || {
            let mut ok = Vec::new();
            let mut i = 0;
            while !st.load(std::sync::atomic::Ordering::SeqCst) {
                let p = format!("f{i}.rs");
                match ix.index_bytes("acme", "web", &p, b"fn a() {}", None) {
                    Ok(_) => ok.push(p),
                    Err(StoreError::PermissionDenied { action, .. }) => assert_eq!(action, "index"),
                    Err(e) => panic!("unexpected: {e:?}"),
                }
                i += 1;
            }
            ok
        });
        for k in 0..10 {
            std::fs::write(&lp, if k % 2 == 0 { revoked.as_str() } else { BASE }).unwrap();
            tb.node(1).running().unwrap().auth().reload_now();
            std::thread::sleep(Duration::from_millis(15));
        }
        std::fs::write(&lp, &revoked).unwrap();
        tb.node(1).running().unwrap().auth().reload_now();
        let ix2 = client(&tb, 2, &t.f.write2, &t);
        denied(
            ix2.index_bytes("acme", "web", "after.rs", b"x", None),
            "after final revoke",
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        h.join().unwrap()
    });
    eprintln!("{} writes admitted during the race", ok.len());
    tb.wait_applied(tb.leader_last_log_index(), Duration::from_secs(20));
    for id in [1, 2, 3] {
        let c = client(&tb, id, &t.f.write, &t);
        for p in &ok {
            assert!(
                c.file_tokens("acme", "web", p).unwrap().is_some(),
                "node {id} {p}"
            );
        }
        assert!(c.file_tokens("acme", "web", "after.rs").unwrap().is_none());
    }
}
