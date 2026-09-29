//! `--bootstrap-or-join` (ADR 0004 D10, stage E): ordinal 0 of a
//! StatefulSet that lost its volume rejoins the existing cluster (never a
//! second one); on a first deployment it bootstraps as soon as every
//! sibling answered that it has no cluster; siblings that disagree are
//! refused; `--force-bootstrap` skips the question.
//!
//! Deterministic: every wait is on a condition with a hard timeout; the
//! siblings are in-process nodes this test started.
mod support;

use graph_server::paths::BootstrapOrJoin;
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_server::{InitMode, JoinSpec};
use std::time::{Duration, Instant};
use support::*;

fn spec(siblings: Vec<String>, probe: Duration) -> BootstrapOrJoin {
    let mut join = JoinSpec::new(siblings.first().cloned().unwrap_or_default());
    join.auto_promote = true;
    join.timeout = Duration::from_secs(30);
    BootstrapOrJoin {
        ordinal: 0,
        join,
        siblings,
        probe_timeout: probe,
        force_bootstrap: false,
    }
}

#[test]
fn a_pod_0_that_lost_its_volume_rejoins_the_existing_cluster() {
    let _w = watchdog(
        "a_pod_0_that_lost_its_volume_rejoins_the_existing_cluster",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    tb.write_via_leader(|c| index_files(c, "o", "r", &[small_file(0), small_file(1)]));
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let cluster_id = tb.client(2).admin_status().unwrap().cluster_id;
    assert!(!cluster_id.is_empty());
    let (ep1, ep2, ep3) = (
        tb.node(1).endpoint(),
        tb.node(2).endpoint(),
        tb.node(3).endpoint(),
    );

    // Pod 0 (node 1) loses its volume.
    tb.node_mut(1).stop();
    let dir = tb.node(1).data_dir().to_path_buf();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // Recorded at another address: refused, nothing written.
    {
        let cfg = tb.node_mut(1).config_mut();
        cfg.init = InitMode::BootstrapOrJoin(spec(
            vec![ep2.clone(), ep3.clone()],
            Duration::from_secs(20),
        ));
        cfg.advertise = Some("127.0.0.1:9".into());
    }
    let e = tb.node_mut(1).try_restart().unwrap_err().to_string();
    assert!(e.contains("not at this node's address"), "{e}");
    assert!(e.contains("cluster remove 1"), "recovery steps: {e}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "a refused start writes nothing"
    );

    // At its own address: the old member is removed and the node joins.
    tb.node_mut(1).config_mut().advertise = Some(ep1.clone());
    tb.node_mut(1).restart();
    let st = tb.client(1).admin_status().unwrap();
    assert_eq!(
        st.cluster_id, cluster_id,
        "the same cluster, never a new one"
    );
    tb.wait_voters(&[1, 2, 3], CLUSTER_WAIT);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(summary(&tb.client(1)), summary(&tb.client(2)));
    let json = graph_server::NodeJson::read(&dir.join("node.json"))
        .unwrap()
        .unwrap();
    assert_eq!(json.cluster_id.as_deref(), Some(cluster_id.as_str()));
    assert!(!json.bootstrapped, "joined, not bootstrapped");

    // A plain restart afterwards is a restart (initialized directory).
    tb.node_mut(1).stop();
    tb.node_mut(1).restart();
    assert_eq!(tb.client(1).admin_status().unwrap().cluster_id, cluster_id);
}

#[test]
fn a_first_deployment_bootstraps_once_every_sibling_is_uninitialized() {
    let _w = watchdog(
        "a_first_deployment_bootstraps_once_every_sibling_is_uninitialized",
        TEST_LIMIT,
    );
    // Node 1 is some other, unrelated cluster; nodes 2 and 3 wait
    // uninitialized (pods 1 and 2 of a new StatefulSet).
    let mut tb = ClusterTestbed::new(1, exts());
    for id in [2, 3] {
        let cfg = tb.node_config(id, InitMode::Uninitialized);
        tb.add_node(id, cfg, exts()).unwrap();
    }
    let (ep1, ep2, ep3) = (
        tb.node(1).endpoint(),
        tb.node(2).endpoint(),
        tb.node(3).endpoint(),
    );
    let other = tb.client(1).admin_status().unwrap().cluster_id;

    // Every sibling answers "no cluster": bootstrap at once, long before
    // the probe timeout.
    let t = Instant::now();
    let cfg = tb.node_config(
        4,
        InitMode::BootstrapOrJoin(spec(vec![ep2, ep3], Duration::from_secs(120))),
    );
    tb.add_node(4, cfg, exts()).unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(60),
        "stopped asking early: {:?}",
        t.elapsed()
    );
    let new = tb.client(4).admin_status().unwrap().cluster_id;
    assert!(!new.is_empty() && new != other, "a new cluster {new}");
    let ep4 = tb.node(4).endpoint();

    // Siblings in two different clusters: refused.
    let cfg = tb.node_config(
        5,
        InitMode::BootstrapOrJoin(spec(vec![ep1.clone(), ep4], Duration::from_secs(20))),
    );
    let e = tb.add_node(5, cfg, exts()).unwrap_err().to_string();
    assert!(e.contains("disagree"), "{e}");

    // --force-bootstrap does not ask.
    let mut s = spec(vec![ep1], Duration::from_secs(20));
    s.force_bootstrap = true;
    let cfg = tb.node_config(6, InitMode::BootstrapOrJoin(s));
    tb.add_node(6, cfg, exts()).unwrap();
    let forced = tb.client(6).admin_status().unwrap().cluster_id;
    assert!(forced != other && forced != new, "{forced}");
}

#[test]
fn unreachable_siblings_are_asked_until_the_probe_timeout() {
    let _w = watchdog(
        "unreachable_siblings_are_asked_until_the_probe_timeout",
        TEST_LIMIT,
    );
    // A port nothing listens on: a listener this test bound and closed.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let mut tb = ClusterTestbed::new(1, exts());
    let probe = Duration::from_secs(2);
    let t = Instant::now();
    let cfg = tb.node_config(2, InitMode::BootstrapOrJoin(spec(vec![dead], probe)));
    tb.add_node(2, cfg, exts()).unwrap();
    assert!(t.elapsed() >= probe, "asked for {:?}", t.elapsed());
    let a = tb.client(1).admin_status().unwrap().cluster_id;
    let b = tb.client(2).admin_status().unwrap().cluster_id;
    assert!(!b.is_empty() && a != b);
}

#[test]
fn other_ordinals_join_and_initialized_ordinal_0_restarts() {
    let _w = watchdog(
        "other_ordinals_join_and_initialized_ordinal_0_restarts",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(1, exts());
    let ep1 = tb.node(1).endpoint();
    let cluster_id = tb.client(1).admin_status().unwrap().cluster_id;
    let mut s = spec(vec![], Duration::from_secs(20));
    s.ordinal = 1;
    s.join.peer = ep1.clone();
    let cfg = tb.node_config(2, InitMode::BootstrapOrJoin(s));
    tb.add_node(2, cfg, exts()).unwrap();
    assert_eq!(tb.client(2).admin_status().unwrap().cluster_id, cluster_id);
    tb.wait_voters(&[1, 2], CLUSTER_WAIT);
    // Node 1 restarted as ordinal 0 with an initialized directory: a
    // restart, whatever the siblings say (here: node 2, same cluster).
    tb.node_mut(1).stop();
    tb.node_mut(1).config_mut().init =
        InitMode::BootstrapOrJoin(spec(vec![tb.node(2).endpoint()], Duration::from_secs(20)));
    tb.node_mut(1).restart();
    assert_eq!(tb.client(1).admin_status().unwrap().cluster_id, cluster_id);
    tb.wait_voters(&[1, 2], CLUSTER_WAIT);
}
