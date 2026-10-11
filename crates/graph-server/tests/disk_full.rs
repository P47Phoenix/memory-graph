//! Story 67 (#115): a full disk while a node appends to its Raft log or
//! applies an entry answers the write `RESOURCE_EXHAUSTED` and does not
//! stop the node; once space is freed the node recovers by itself and a
//! retried write is applied exactly once everywhere. Driven by the
//! ClusterTestbed's disk-full failpoints (`DiskStall::fail_log_append`,
//! `DiskStall::fail_apply`), with a hard timeout on every wait.
mod support;
use support::*;

use graph_client::RemoteStore;
use graph_core::NodeKind;
use graph_server::raft::disk_stall::{DiskStall, StallSource};
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_store::{Store, StoreRead};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn stall(tb: &ClusterTestbed, id: u64) -> DiskStall {
    tb.node(id).raft().expect("running").disk_stall.clone()
}

/// The node's store slot: the same `Arc` before and after means the node
/// was neither restarted nor reopened.
fn slot_ptr(tb: &ClusterTestbed, id: u64) -> usize {
    Arc::as_ptr(&tb.node(id).running().expect("running").slot) as usize
}

fn raft_running(tb: &ClusterTestbed, id: u64) -> bool {
    tb.node(id)
        .raft()
        .expect("running")
        .raft
        .metrics()
        .borrow()
        .running_state
        .is_ok()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + CLUSTER_WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The wire status code of a `Vacuum` sent straight to node `id`.
fn raw_vacuum_code(tb: &ClusterTestbed, id: u64) -> tonic::Code {
    let endpoint = tb.node(id).endpoint();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{endpoint}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut w = graph_proto::pb::write_client::WriteClient::new(ch);
        match w.vacuum(graph_proto::pb::VacuumRequest {}).await {
            Ok(_) => tonic::Code::Ok,
            Err(s) => s.code(),
        }
    })
}

fn files(c: &RemoteStore) -> usize {
    c.count_nodes(NodeKind::File).unwrap()
}

/// Every node applied the leader's log and holds `n` files, at the same
/// marker: the retried write landed exactly once on each.
fn assert_converged(tb: &ClusterTestbed, n: usize) {
    let idx = tb.leader_last_log_index();
    tb.wait_applied(idx, CLUSTER_WAIT);
    let markers: Vec<u64> = tb
        .ids()
        .iter()
        .map(|id| tb.node(*id).applied_index())
        .collect();
    assert!(markers.iter().all(|m| *m == markers[0]), "{markers:?}");
    for id in tb.ids() {
        assert_eq!(files(&tb.client(id)), n, "node {id}");
    }
}

fn formed() -> ClusterTestbed {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    tb.write_via_leader(|c| c.index_bytes("o", "r", "a.rs", b"fn a() {}", None))
        .unwrap();
    assert_converged(&tb, 1);
    tb
}

#[test]
fn leader_log_append_disk_full_answers_resource_exhausted_and_recovers() {
    let _w = watchdog("leader log append disk full", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let slot = slot_ptr(&tb, l);
    let marker = tb.node(l).applied_index();
    // The pre-propose disk check passes (the default probe); the append's
    // commit then meets a full disk.
    let st = stall(&tb, l);
    st.fail_log_append(true);
    let c = tb.client(l);
    let e = c
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(
        graph_proto::error::is_disk_full(&e.to_string()),
        "a disk-full refusal: {e:?}"
    );
    assert!(e.to_string().contains("Raft log"), "{e}");
    assert!(st.is_stalled(StallSource::LogAppend));
    // The typed status code, straight from the wire.
    assert_eq!(raw_vacuum_code(&tb, l), tonic::Code::ResourceExhausted);
    // Unacknowledged, the marker unchanged, the node alive and serving
    // reads.
    assert_eq!(tb.node(l).applied_index(), marker);
    assert!(raft_running(&tb, l), "the node's Raft did not stop");
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
    assert_eq!(files(&c), 1, "LOCAL reads still served");
    // Space is freed: the stall clears by itself, and a retry succeeds.
    st.fail_log_append(false);
    wait_until("the append stall to clear", || st.current().is_none());
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
    assert!(raft_running(&tb, l));
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
}

#[test]
fn leader_apply_disk_full_answers_resource_exhausted_and_recovers() {
    let _w = watchdog("leader apply disk full", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let slot = slot_ptr(&tb, l);
    let marker = tb.node(l).applied_index();
    let st = stall(&tb, l);
    st.fail_apply(true);
    let c = tb.client(l);
    let e = c
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(
        graph_proto::error::is_disk_full(&e.to_string()),
        "a disk-full refusal: {e:?}"
    );
    assert!(e.to_string().contains("apply"), "{e}");
    assert!(st.is_stalled(StallSource::Apply));
    assert_eq!(raw_vacuum_code(&tb, l), tonic::Code::ResourceExhausted);
    assert_eq!(tb.node(l).applied_index(), marker, "marker unchanged");
    assert!(raft_running(&tb, l), "the node's Raft did not stop");
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
    assert_eq!(files(&c), 1, "LOCAL reads still served");
    // The Raft core goes on: this node still leads (heartbeats flow).
    assert_eq!(tb.leader(), l);
    st.fail_apply(false);
    wait_until("the apply stall to clear", || st.current().is_none());
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
    assert!(raft_running(&tb, l));
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
}

#[test]
fn follower_apply_disk_full_lags_and_catches_up() {
    let _w = watchdog("follower apply disk full", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let f = *tb.ids().iter().find(|id| **id != l).unwrap();
    let slot = slot_ptr(&tb, f);
    let marker = tb.node(f).applied_index();
    let st = stall(&tb, f);
    st.fail_apply(true);
    // The leader and the other follower commit and apply it.
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    let idx = tb.leader_last_log_index();
    wait_until("the follower's apply to stall", || {
        st.is_stalled(StallSource::Apply)
    });
    // Its log keeps up; its store does not move; it is still running.
    wait_until("the follower's log to hold the entry", || {
        tb.node(f).raft().unwrap().metrics().last_log_index >= Some(idx)
    });
    assert_eq!(tb.node(f).applied_index(), marker, "marker unchanged");
    assert!(raft_running(&tb, f), "the node's Raft did not stop");
    assert_eq!(files(&tb.client(f)), 1, "LOCAL reads still served");
    st.fail_apply(false);
    assert_converged(&tb, 2);
    assert!(st.current().is_none());
    assert_eq!(slot_ptr(&tb, f), slot, "not restarted");
}
