//! Stage F (ADR 0004 Q3, epic story 25 AC 3, issue #107): `serve
//! --update-advertise`. A member restarted at a new address with the flag
//! is recorded there by the leader, replicates, and its `node.json` names
//! the new address; without the flag a different `--advertise` is refused
//! as before. Every wait has a hard timeout ([`CLUSTER_WAIT`]).
use graph_core::Extractor;
use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT};
use graph_server::{InitMode, NodeJson, ServeConfig};
use graph_store::{Store, StoreRead};
use std::time::{Duration, Instant};

fn rust_only() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// A loopback port nobody listens on right now (the node binds it next;
/// a restart retries while it is briefly taken).
fn free_addr() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

/// Node `id`'s address in the membership node `on` sees.
fn addr_in_membership(tb: &ClusterTestbed, on: u64, id: u64) -> Option<String> {
    let m = tb.node(on).raft()?.metrics();
    let mem = m.membership_config.membership();
    mem.get_node(&id).map(|n| n.addr.clone())
}

fn write(tb: &ClusterTestbed, i: usize) -> u64 {
    tb.write_via_leader(|c| {
        c.index_bytes(
            "o",
            "r",
            &format!("f{i}.rs"),
            format!("fn f{i}() {{}}\n").as_bytes(),
            Some("rust"),
        )
        .unwrap_or_else(|e| panic!("write {i}: {e}"))
    });
    tb.leader_last_log_index()
}

fn files_on(tb: &ClusterTestbed, id: u64) -> usize {
    let s = tb.node(id).running().unwrap().slot.clone();
    s.with_store(|st| st.describe(Some("o"), Some("r")))
        .unwrap()
        .first()
        .map_or(0, |r| r.files)
}

#[test]
fn a_member_moves_to_a_new_address_with_update_advertise() {
    let t0 = Instant::now();
    let mut tb = ClusterTestbed::new(3, rust_only());
    tb.form();
    let idx = write(&tb, 0);
    tb.wait_applied(idx, CLUSTER_WAIT);
    let old = tb.node(3).endpoint();
    let json_path = tb.data_dir(3).join("node.json");
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);

    tb.node_mut(3).stop();
    let new = free_addr();
    assert_ne!(new, old);

    // Without the flag: a different --advertise is refused, before
    // anything is written, and says what to use instead.
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.listen = new.parse().unwrap();
        cfg.advertise = Some(new.clone());
    }
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    assert!(
        e.contains("differs from") && e.contains("--update-advertise"),
        "{e}"
    );
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);

    // An unreachable address with the flag: the leader cannot confirm who
    // is there, the start fails at the timeout, node.json is unchanged.
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.advertise = None;
        cfg.update_advertise = Some("127.0.0.1:9".into());
        cfg.update_advertise_timeout = Duration::from_secs(3);
    }
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    assert!(e.contains("--update-advertise 127.0.0.1:9"), "{e}");
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);
    let l = tb.wait_leader(CLUSTER_WAIT);
    assert_eq!(addr_in_membership(&tb, l, 3), Some(old.clone()));

    // With the flag and the address it listens at: recorded by the leader.
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.update_advertise = Some(new.clone());
        cfg.update_advertise_timeout = CLUSTER_WAIT;
    }
    tb.node_mut(3).restart();
    assert_eq!(tb.node(3).endpoint(), new);
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, new);
    // Every member learns it (the leader committed it before the restart
    // returned; the others once they apply the entry).
    let deadline = Instant::now() + CLUSTER_WAIT;
    loop {
        let seen: Vec<Option<String>> = [1, 2, 3]
            .iter()
            .map(|&on| addr_in_membership(&tb, on, 3))
            .collect();
        if seen.iter().all(|a| a.as_deref() == Some(new.as_str())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "node 3's address in the members' memberships: {seen:?}, want {new}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Still three voters, and node 3 replicates at the new address.
    tb.wait_voters(&[1, 2, 3], CLUSTER_WAIT);
    let idx = write(&tb, 1);
    tb.wait_applied(idx, CLUSTER_WAIT);
    assert_eq!(files_on(&tb, 3), 2);
    let st = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&new))
        .unwrap()
        .admin_status()
        .unwrap();
    assert_eq!(st.advertise, new);

    // Again with the same address: nothing to do, a plain restart.
    tb.node_mut(3).stop();
    tb.node_mut(3).restart();
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, new);
    let idx = write(&tb, 2);
    tb.wait_applied(idx, CLUSTER_WAIT);
    assert_eq!(files_on(&tb, 3), 3);
    eprintln!("update-advertise test done in {:?}", t0.elapsed());
}

/// Dev review 3: a crash between the cluster committing the new address and
/// node.json being rewritten (failpoint). A plain restart would serve at the
/// old address while the leader replicates to the new one, so it is refused
/// and names the flag that finishes the move; that flag then succeeds.
#[test]
fn a_crash_between_commit_and_node_json_rewrite_is_refused_then_finished() {
    let mut tb = ClusterTestbed::new(3, rust_only());
    tb.form();
    let idx = write(&tb, 0);
    tb.wait_applied(idx, CLUSTER_WAIT);
    let old = tb.node(3).endpoint();
    let json_path = tb.data_dir(3).join("node.json");
    tb.node_mut(3).stop();
    let new = free_addr();
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.listen = new.parse().unwrap();
        cfg.update_advertise = Some(new.clone());
        cfg.update_advertise_timeout = CLUSTER_WAIT;
        cfg.testing.fail_before_advertise_rewrite = true;
    }
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    // The error says the cluster has it and how to finish (QA 5).
    assert!(
        e.contains("failpoint")
            && e.contains("was not rewritten")
            && e.contains(&format!("--update-advertise {new}")),
        "{e}"
    );
    // Committed in the cluster, node.json still the old address.
    let l = tb.wait_leader(CLUSTER_WAIT);
    assert_eq!(addr_in_membership(&tb, l, 3), Some(new.clone()));
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);

    // A plain restart: refused, naming the recovery flag.
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.testing.fail_before_advertise_rewrite = false;
        cfg.update_advertise = None;
        // Refused only after the catch-up wait (the leader replicates to
        // the new address, so this view never changes).
        cfg.testing.advertise_catchup_ms = Some(2_000);
    }
    let t = Instant::now();
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    assert!(
        e.contains(&format!("--update-advertise {new}")) && e.contains(&old),
        "{e}"
    );
    assert!(
        t.elapsed() >= Duration::from_secs(2),
        "refused before the wait"
    );
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);

    // The flag it names finishes the move (the cluster has it already).
    tb.node_mut(3).config_mut().update_advertise = Some(new.clone());
    tb.node_mut(3).restart();
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, new);
    let idx = write(&tb, 1);
    tb.wait_applied(idx, CLUSTER_WAIT);
    assert_eq!(files_on(&tb, 3), 2);

    // And a plain restart after that is fine again.
    tb.node_mut(3).stop();
    tb.node_mut(3).config_mut().update_advertise = None;
    tb.node_mut(3).restart();
    assert_eq!(tb.node(3).endpoint(), new);
}

/// Review of #124: node.json is rewritten only once this node's own log holds
/// the new address, so stopping right after a successful move and
/// restarting plainly is never refused.
#[test]
fn a_restart_right_after_a_move_is_never_refused() {
    let mut tb = ClusterTestbed::new(3, rust_only());
    tb.form();
    let idx = write(&tb, 0);
    tb.wait_applied(idx, CLUSTER_WAIT);
    let json_path = tb.data_dir(3).join("node.json");
    for round in 0..3 {
        tb.node_mut(3).stop();
        let new = free_addr();
        {
            let cfg = tb.node_mut(3).config_mut();
            cfg.listen = new.parse().unwrap();
            cfg.update_advertise = Some(new.clone());
            cfg.update_advertise_timeout = CLUSTER_WAIT;
            // A slow link to this node: the leader commits without it.
            cfg.testing.delay_append_entries_ms = Some(500);
            // Refusal would be immediate-ish; any refusal fails the test.
            cfg.testing.advertise_catchup_ms = Some(100);
        }
        tb.node_mut(3).restart();
        assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, new);
        assert_eq!(
            addr_in_membership(&tb, 3, 3),
            Some(new.clone()),
            "round {round}"
        );
        // Stop at once and restart plainly.
        tb.node_mut(3).stop();
        tb.node_mut(3).config_mut().update_advertise = None;
        tb.node_mut(3)
            .try_restart()
            .unwrap_or_else(|e| panic!("round {round}: plain restart refused: {e}"));
        assert_eq!(tb.node(3).endpoint(), new);
        tb.node_mut(3).config_mut().testing.delay_append_entries_ms = None;
    }
    let idx = write(&tb, 1);
    tb.wait_applied(idx, CLUSTER_WAIT);
    assert_eq!(files_on(&tb, 3), 2);
}

/// Review of #124: a node whose membership view merely lags (node.json has
/// the new address, its own log not yet) is not refused: it serves at
/// node.json's address, the leader catches it up, and the check passes.
#[test]
fn a_lagging_membership_view_is_caught_up_not_refused() {
    let mut tb = ClusterTestbed::new(3, rust_only());
    tb.form();
    let idx = write(&tb, 0);
    tb.wait_applied(idx, CLUSTER_WAIT);
    let old = tb.node(3).endpoint();
    let json_path = tb.data_dir(3).join("node.json");
    tb.node_mut(3).stop();
    let new = free_addr();
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.listen = new.parse().unwrap();
        cfg.update_advertise = Some(new.clone());
        cfg.update_advertise_timeout = CLUSTER_WAIT;
        // The old behaviour (no wait for the own log) plus a slow link:
        // node.json is rewritten while this node's view still has `old`.
        cfg.testing.advertise_rewrite_skip_wait = true;
        cfg.testing.delay_append_entries_ms = Some(5_000);
    }
    tb.node_mut(3).restart();
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, new);
    assert_eq!(
        addr_in_membership(&tb, 3, 3),
        Some(old.clone()),
        "the view should lag behind node.json here"
    );
    tb.node_mut(3).stop();
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.update_advertise = None;
        cfg.testing.advertise_rewrite_skip_wait = false;
        cfg.testing.delay_append_entries_ms = None;
        cfg.testing.advertise_catchup_ms = Some(CLUSTER_WAIT.as_millis() as u64);
    }
    tb.node_mut(3)
        .try_restart()
        .unwrap_or_else(|e| panic!("a lagging view was refused: {e}"));
    assert_eq!(addr_in_membership(&tb, 3, 3), Some(new.clone()));
    let idx = write(&tb, 1);
    tb.wait_applied(idx, CLUSTER_WAIT);
    assert_eq!(files_on(&tb, 3), 2);
}

/// QA 3 and 4: the leader refuses a new address that is another member's
/// recorded one (even while that member is down), and one where another
/// node answers. node.json and the membership stay as they were.
#[test]
fn update_advertise_refuses_another_members_or_another_nodes_address() {
    let mut tb = ClusterTestbed::new(3, rust_only());
    tb.form();
    // A learner, then down: its address stays recorded.
    let mut spec = graph_server::JoinSpec::new(tb.node(1).endpoint());
    spec.auto_promote = false;
    spec.timeout = CLUSTER_WAIT;
    let cfg = tb.node_config(4, InitMode::Join(spec));
    tb.add_node(4, cfg, rust_only()).unwrap();
    let four = tb.node(4).endpoint();
    let l = tb.wait_leader(CLUSTER_WAIT);
    let deadline = Instant::now() + CLUSTER_WAIT;
    while addr_in_membership(&tb, l, 4).as_deref() != Some(four.as_str()) {
        assert!(Instant::now() < deadline, "node 4 never recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    tb.node_mut(4).stop();

    let old = tb.node(3).endpoint();
    let json_path = tb.data_dir(3).join("node.json");
    tb.node_mut(3).stop();
    // Node 3 listens at node 4's recorded address and asks for it.
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.listen = four.parse().unwrap();
        cfg.update_advertise = Some(four.clone());
        cfg.update_advertise_timeout = Duration::from_secs(3);
    }
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    assert!(e.contains("recorded address of member 4"), "{e}");
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);

    // Another server (node 2 of its own) answers at the new address.
    let d = tempfile::tempdir().unwrap();
    let mut other_cfg = ServeConfig::new(d.path().join("g.redb"), "127.0.0.1:0".parse().unwrap());
    other_cfg.node_id = Some(2);
    let other = TestServer::try_start_config(other_cfg, rust_only()).unwrap();
    {
        let cfg = tb.node_mut(3).config_mut();
        cfg.listen = free_addr().parse().unwrap();
        cfg.update_advertise = Some(other.endpoint());
    }
    let e = tb.node_mut(3).try_restart().unwrap_err().to_string();
    assert!(e.contains("is node 2, not node 3"), "{e}");
    assert_eq!(NodeJson::read(&json_path).unwrap().unwrap().advertise, old);
    let l = tb.wait_leader(CLUSTER_WAIT);
    assert_eq!(addr_in_membership(&tb, l, 3), Some(old));
}

#[test]
fn update_advertise_is_refused_where_there_is_no_member_to_move() {
    let d = tempfile::tempdir().unwrap();
    // A --db server.
    let mut cfg = ServeConfig::new(d.path().join("g.redb"), "127.0.0.1:0".parse().unwrap());
    cfg.update_advertise = Some("127.0.0.1:1".into());
    let e = TestServer::try_start_config(cfg, rust_only())
        .err()
        .expect("refused")
        .to_string();
    assert!(e.contains("needs --data-dir"), "{e}");
    assert!(!d.path().join("g.redb").exists(), "nothing written");
    // A new data directory.
    let dir = d.path().join("new");
    let mut cfg = ServeConfig::for_data_dir(
        &dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap { restore: None },
        Some(1),
    );
    cfg.update_advertise = Some("127.0.0.1:1".into());
    let e = TestServer::try_start_config(cfg, rust_only())
        .err()
        .expect("refused")
        .to_string();
    assert!(e.contains("this data directory is new"), "{e}");
    assert!(!dir.join("node.json").exists(), "nothing written");
}
