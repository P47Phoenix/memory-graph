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
