//! Stage E (ADR 0004 D10, epic story 24): readiness follows the lag, and
//! `Admin.Metrics` answers the Prometheus document.
//!
//! Deterministic: the learners' apply is held back by a test gate
//! (`ServeConfig::apply_gate`) while they keep receiving the leader's
//! entries and its committed index, so their applied index stays put as the
//! leader's committed index moves on; only `ready_max_lag` differs between
//! them.
mod support;

use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_server::{InitMode, JoinSpec, READY_SERVICE};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use support::*;

fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + CLUSTER_WAIT;
    while !probe() {
        assert!(
            Instant::now() < deadline,
            "timed out after {CLUSTER_WAIT:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Holds apply of every entry above the index it is set to; `None` lets
/// everything through. A held apply gives up after a minute (it never
/// hangs a test run).
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<Option<u64>>, Condvar)>);

impl Gate {
    fn hold_above(&self, index: Option<u64>) {
        *self.0 .0.lock().unwrap() = index;
        self.0 .1.notify_all();
    }

    fn as_apply_gate(&self) -> graph_server::raft::state_machine::ApplyGate {
        let g = self.clone();
        Arc::new(move |index| {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut held = g.0 .0.lock().unwrap();
            while held.is_some_and(|x| index > x) && Instant::now() < deadline {
                held =
                    g.0 .1
                        .wait_timeout(held, Duration::from_millis(100))
                        .unwrap()
                        .0;
            }
        })
    }
}

#[test]
fn a_lagging_learner_is_not_ready_until_within_max_lag() {
    let _w = watchdog(
        "a_lagging_learner_is_not_ready_until_within_max_lag",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(1, exts());
    index_files(&tb.client(1), "o", "r", &[small_file(0)]);
    let gate = Gate::default();
    for (id, max_lag) in [(2u64, 2u64), (3, 1_000_000)] {
        let mut spec = JoinSpec::new(tb.node(1).endpoint());
        spec.timeout = Duration::from_secs(20);
        let mut cfg = tb.node_config(id, InitMode::Join(spec));
        cfg.ready_max_lag = max_lag;
        cfg.apply_gate = Some(gate.as_apply_gate());
        tb.add_node(id, cfg, exts()).unwrap();
    }
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    for id in [1, 2, 3] {
        wait_until(&format!("node {id} to be ready once caught up"), || {
            tb.client(id).health(READY_SERVICE).unwrap()
        });
    }
    // Hold the learners' apply; five more entries commit on the leader.
    let before = tb.leader_last_log_index();
    gate.hold_above(Some(before));
    for i in 1..=5 {
        index_files(&tb.client(1), "o", "r", &[small_file(i)]);
    }
    let committed = tb.client(1).admin_status().unwrap().committed_index;
    assert!(committed >= before + 5, "{committed} vs {before}");
    for id in [2, 3] {
        wait_until(&format!("node {id} to hear the new commit index"), || {
            tb.node(id).raft().unwrap().obs.leader_commit() >= before + 3
        });
        assert_eq!(
            tb.node(id).applied_index(),
            before,
            "node {id} applied nothing new"
        );
    }
    wait_until(
        "node 2 (3+ behind, ready_max_lag 2) to report NOT_SERVING",
        || !tb.client(2).health(READY_SERVICE).unwrap(),
    );
    let (c1, c2, c3) = (tb.client(1), tb.client(2), tb.client(3));
    for _ in 0..10 {
        assert!(c2.health("").unwrap(), "node 2 is alive, only not ready");
        assert!(!c2.health(READY_SERVICE).unwrap(), "node 2 stays not ready");
        assert!(
            c3.health(READY_SERVICE).unwrap(),
            "node 3 (same lag, ready_max_lag 1000000) is ready"
        );
        assert!(c1.health(READY_SERVICE).unwrap(), "the leader is ready");
        std::thread::sleep(Duration::from_millis(30));
    }
    // Released, node 2 catches up and is ready again.
    gate.hold_above(None);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    wait_until("node 2 to be ready again after catching up", || {
        c2.health(READY_SERVICE).unwrap()
    });
}

#[test]
fn admin_metrics_names_every_metric() {
    let _w = watchdog("admin_metrics_names_every_metric", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(2, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let text = tb.client(leader).admin_metrics().unwrap();
    for name in graph_server::observe::METRIC_NAMES {
        assert!(
            text.contains(&format!("# TYPE {name} ")),
            "{name} missing:\n{text}"
        );
    }
    let other = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    assert!(
        text.contains(&format!("mg_raft_replication_lag{{peer=\"{other}\"}} ")),
        "the leader reports its peer's lag:\n{text}"
    );
    assert!(text.contains("mg_raft_role{role=\"leader\"} 1"), "{text}");
    assert!(
        text.contains("mg_rpc_total{rpc=\"Write/"),
        "writes are counted:\n{text}"
    );
    // The follower's document has no replication lines (leader only).
    let f = tb.client(other).admin_metrics().unwrap();
    assert!(!f.contains("mg_raft_replication_lag{"), "{f}");
    assert!(f.contains("mg_raft_role{role=\"follower\"} 1"), "{f}");
}
