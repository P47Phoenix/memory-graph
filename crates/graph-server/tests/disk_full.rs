//! Story 67 (#115): a full disk while a node appends to its Raft log or
//! applies an entry answers the write `RESOURCE_EXHAUSTED` and does not
//! stop the node; once space is freed the node recovers by itself and a
//! retried write is applied exactly once everywhere. Driven by the
//! ClusterTestbed's disk-full failpoints (`DiskStall::fail_log_append`,
//! `DiskStall::fail_apply`) and by [`FullDisk`] storage backends whose
//! writes fail inside redb (a real ENOSPC path), with a hard timeout on
//! every wait.
mod support;
use support::*;

use graph_client::RemoteStore;
use graph_core::NodeKind;
use graph_server::full_disk::FullDisk;
use graph_server::raft::disk_stall::{DiskStall, StallSource};
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_store::{Store, StoreRead};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn stall(tb: &ClusterTestbed, id: u64) -> DiskStall {
    tb.node(id).raft().expect("running").disk_stall.clone()
}

/// The node's store slot: the same `Arc` before and after means the node
/// was not restarted (its store may have been reopened in place).
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

// ---- linearizable reads during a stall ---------------------------------

/// A linearizable reader of node `id` only, with a long budget: a stall
/// must answer at once, not wait the budget out.
fn lin_reader(tb: &ClusterTestbed, id: u64) -> RemoteStore {
    let mut cfg = graph_client::ClientConfig::new(tb.node(id).endpoint());
    cfg.read_mode = graph_client::ReadMode::Linearizable;
    cfg.retry.budget = Duration::from_secs(30);
    RemoteStore::connect(cfg).unwrap()
}

fn assert_fast_disk_full_read(tb: &ClusterTestbed, id: u64) {
    let r = lin_reader(tb, id);
    let t0 = Instant::now();
    let e = r.count_nodes(NodeKind::File).unwrap_err();
    assert!(
        graph_proto::error::is_disk_full(&e.to_string()),
        "a disk-full answer: {e:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "answered at once, not at the deadline: {:?}",
        t0.elapsed()
    );
}

#[test]
fn a_linearizable_read_on_an_apply_stalled_leader_answers_at_once() {
    let _w = watchdog("linearizable read, apply stall", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let st = stall(&tb, l);
    st.fail_apply(true);
    let _ = tb
        .client(l)
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert_fast_disk_full_read(&tb, l);
    st.fail_apply(false);
    wait_until("the stall to clear", || st.current().is_none());
    assert_eq!(lin_reader(&tb, l).count_nodes(NodeKind::File).unwrap(), 2);
    assert_converged(&tb, 2);
}

#[test]
fn a_linearizable_read_on_an_append_stalled_leader_answers_at_once() {
    let _w = watchdog("linearizable read, append stall", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let st = stall(&tb, l);
    st.fail_log_append(true);
    let _ = tb
        .client(l)
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    // Before the peers' election timeout: no newer leader is known yet.
    assert_fast_disk_full_read(&tb, l);
    st.fail_log_append(false);
    wait_until("the stall to clear", || st.current().is_none());
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
}

// ---- a stall longer than the election timeout ---------------------------

/// The leader's log append stalls for longer than an election timeout: the
/// peers elect a new leader; the stalled node, whose held core still says
/// it leads, answers a write `NotLeader` naming the new leader (seen in
/// its `AppendEntries`), so a client of the old leader alone lands its
/// write there. Once space is freed the old leader steps down, its
/// unreplicated entry is truncated, and it rejoins as a follower without
/// a restart.
#[test]
fn a_long_append_stall_moves_leadership_and_the_old_leader_rejoins() {
    let _w = watchdog("long append stall", Duration::from_secs(180));
    let tb = formed();
    let old = tb.leader();
    let slot = slot_ptr(&tb, old);
    let st = stall(&tb, old);
    st.fail_log_append(true);
    let c = tb.client(old);
    let e = c
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e:?}");
    // The peers elect another leader.
    wait_until("a new leader", || tb.leader() != old);
    let new = tb.leader();
    // The stalled node's raw answer is now NotLeader (UNAVAILABLE) ...
    wait_until("the stalled node to learn of the new leader", || {
        tb.node(old).raft().unwrap().newer_leader().is_some()
    });
    assert_eq!(tb.node(old).raft().unwrap().newer_leader().unwrap().0, new);
    assert_eq!(raw_vacuum_code(&tb, old), tonic::Code::Unavailable);
    // ... and a client of the old leader alone follows it: the write lands.
    c.index_bytes("o", "r", "c.rs", b"fn c() {}", None).unwrap();
    assert_eq!(files(&tb.client(new)), 2);
    // Space is freed: the old leader steps down and catches up.
    st.fail_log_append(false);
    wait_until("the stall to clear", || st.current().is_none());
    wait_until("the old leader to follow", || {
        tb.node(old).raft().unwrap().metrics().state != openraft::ServerState::Leader
    });
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 3);
    assert!(raft_running(&tb, old));
    assert_eq!(slot_ptr(&tb, old), slot, "not restarted");
}

// ---- concurrent writes behind a stalled entry ---------------------------

#[test]
fn concurrent_writes_behind_a_stalled_apply_all_refused_then_land_once() {
    let _w = watchdog("concurrent writes behind a stall", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let st = stall(&tb, l);
    st.fail_apply(true);
    let ep = tb.node(l).endpoint();
    let writers: Vec<_> = (0..4)
        .map(|i| {
            let ep = ep.clone();
            std::thread::spawn(move || {
                let mut cfg = graph_client::ClientConfig::new(ep);
                cfg.write_deadline = Duration::from_secs(15);
                let c = RemoteStore::connect(cfg).unwrap();
                c.index_bytes("o", "r", &format!("w{i}.rs"), b"fn w() {}", None)
                    .unwrap_err()
                    .to_string()
            })
        })
        .collect();
    for w in writers {
        let e = w.join().unwrap();
        assert!(graph_proto::error::is_disk_full(&e), "{e}");
    }
    let marker = tb.node(l).applied_index();
    assert_eq!(files(&tb.client(l)), 1, "nothing applied on the leader");
    st.fail_apply(false);
    wait_until("the stall to clear", || st.current().is_none());
    assert!(tb.node(l).applied_index() >= marker);
    // Every client retries: each file lands exactly once.
    for i in 0..4 {
        tb.write_via_leader(|c| c.index_bytes("o", "r", &format!("w{i}.rs"), b"fn w() {}", None))
            .unwrap();
    }
    assert_converged(&tb, 5);
}

// ---- a follower's log append stalls ------------------------------------

#[test]
fn a_follower_append_stall_leaves_the_leader_serving_and_catches_up() {
    let _w = watchdog("follower append stall", Duration::from_secs(180));
    let tb = formed();
    let l = tb.leader();
    let f = *tb.ids().iter().find(|id| **id != l).unwrap();
    let slot = slot_ptr(&tb, f);
    let st = stall(&tb, f);
    st.fail_log_append(true);
    for i in 0..3 {
        tb.write_via_leader(|c| c.index_bytes("o", "r", &format!("f{i}.rs"), b"fn f() {}", None))
            .unwrap();
    }
    wait_until("the follower's append to stall", || {
        st.is_stalled(StallSource::LogAppend)
    });
    assert_eq!(tb.leader(), l, "the leader keeps leading");
    assert_eq!(files(&tb.client(l)), 4);
    assert!(raft_running(&tb, f));
    st.fail_log_append(false);
    assert_converged(&tb, 4);
    assert_eq!(slot_ptr(&tb, f), slot, "not restarted");
}

// ---- real redb errors: a backend whose disk fills ----------------------

/// A testbed whose nodes keep their store and log on [`FullDisk`] files:
/// the full-disk error reaches redb itself and poisons the open database
/// (`PreviousIo`), as a real ENOSPC does.
fn formed_on_full_disks() -> (ClusterTestbed, Vec<FullDisk>) {
    let disks: Vec<FullDisk> = (0..3).map(|_| FullDisk::new()).collect();
    let d2 = disks.clone();
    let mut tb = ClusterTestbed::with_config(3, exts(), move |id, c| {
        c.storage_backend = Some(d2[(id - 1) as usize].factory());
    });
    tb.form();
    tb.write_via_leader(|c| c.index_bytes("o", "r", "a.rs", b"fn a() {}", None))
        .unwrap();
    assert_converged(&tb, 1);
    (tb, disks)
}

#[test]
fn redb_append_disk_full_reopens_the_log_and_recovers() {
    let _w = watchdog("redb append disk full", Duration::from_secs(180));
    let (tb, disks) = formed_on_full_disks();
    let l = tb.leader();
    let slot = slot_ptr(&tb, l);
    let marker = tb.node(l).applied_index();
    let disk = &disks[(l - 1) as usize];
    disk.set_log_full(true);
    let e = tb
        .client(l)
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e:?}");
    let st = stall(&tb, l);
    assert!(st.is_stalled(StallSource::LogAppend));
    assert_eq!(tb.node(l).applied_index(), marker);
    assert_eq!(files(&tb.client(l)), 1, "LOCAL reads still served");
    // Space comes back: the retry reopens raft.redb (clearing PreviousIo)
    // and the append goes through, without a restart.
    disk.set_log_full(false);
    wait_until("the stall to clear", || st.current().is_none());
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
    assert!(raft_running(&tb, l));
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
}

#[test]
fn redb_apply_disk_full_reopens_the_store_and_recovers() {
    let _w = watchdog("redb apply disk full", Duration::from_secs(180));
    let (tb, disks) = formed_on_full_disks();
    let l = tb.leader();
    let slot = slot_ptr(&tb, l);
    let marker = tb.node(l).applied_index();
    let disk = &disks[(l - 1) as usize];
    disk.set_store_full(true);
    let e = tb
        .client(l)
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e:?}");
    let st = stall(&tb, l);
    assert!(st.is_stalled(StallSource::Apply));
    assert_eq!(tb.node(l).applied_index(), marker, "marker unchanged");
    assert!(raft_running(&tb, l));
    disk.set_store_full(false);
    wait_until("the stall to clear", || st.current().is_none());
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
    assert!(raft_running(&tb, l));
    assert_eq!(slot_ptr(&tb, l), slot, "not restarted");
}

/// Every node's disk fills (store and log): writes are refused, elections
/// stall on their vote writes too, and nothing exits. Once space is back
/// everywhere the cluster recovers by itself, elects a leader and a
/// retried write lands once on every node, with no restart.
#[test]
fn the_whole_cluster_full_recovers_without_restarts() {
    let _w = watchdog("whole cluster full", Duration::from_secs(240));
    let (tb, disks) = formed_on_full_disks();
    let slots: Vec<usize> = tb.ids().iter().map(|id| slot_ptr(&tb, *id)).collect();
    let l = tb.leader();
    for d in &disks {
        d.set_store_full(true);
        d.set_log_full(true);
    }
    let e = tb
        .client(l)
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e:?}");
    // Let elections run into the full disks for a while.
    std::thread::sleep(Duration::from_secs(3));
    for id in tb.ids() {
        let state = tb
            .node(id)
            .raft()
            .unwrap()
            .raft
            .metrics()
            .borrow()
            .running_state
            .clone();
        assert!(state.is_ok(), "node {id} still running: {state:?}");
        assert_eq!(files(&tb.client(id)), 1, "node {id}: LOCAL reads");
    }
    for d in &disks {
        d.set_store_full(false);
        d.set_log_full(false);
    }
    for id in tb.ids() {
        let st = stall(&tb, id);
        wait_until("every stall to clear", || st.current().is_none());
    }
    tb.write_via_leader(|c| c.index_bytes("o", "r", "b.rs", b"fn b() {}", None))
        .unwrap();
    assert_converged(&tb, 2);
    for (i, id) in tb.ids().iter().enumerate() {
        assert!(raft_running(&tb, *id));
        assert_eq!(slot_ptr(&tb, *id), slots[i], "node {id} not restarted");
    }
}
