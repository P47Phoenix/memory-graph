//! Stage D (ADR 0004 D8, epic story 23): linearizable reads and read
//! freshness (`ReadMeta`, `stale_possible`) through the in-process
//! `ClusterTestbed`. Faults come from the shared fault plan and the test
//! hooks; every wait has a hard timeout and names what it waited for.
mod support;
use support::*;

use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::NodeKind;
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT, TEST_RAFT};
use graph_store::{StoreError, StoreRead};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn others(tb: &ClusterTestbed, not: &[u64]) -> Vec<u64> {
    tb.ids().into_iter().filter(|i| !not.contains(i)).collect()
}

/// Wait until `probe` holds, polling; fail with `what` after `CLUSTER_WAIT`.
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

/// A client of `endpoints` in `mode` with a short read retry budget.
fn reader(endpoints: &[String], mode: ReadMode, budget: Duration) -> RemoteStore {
    let mut cfg = ClientConfig::new(endpoints[0].clone());
    cfg.endpoints = endpoints.to_vec();
    cfg.read_mode = mode;
    cfg.retry.budget = budget;
    RemoteStore::connect(cfg).unwrap()
}

fn files(c: &RemoteStore) -> Result<usize, StoreError> {
    c.count_nodes(NodeKind::File)
}

/// A read's freshness is reported: a local read on the leader and on a
/// caught-up follower is not `stale_possible`, a linearizable one never
/// is, and `applied_index` is the node's.
#[test]
fn reads_carry_read_meta() {
    let _w = watchdog("reads_carry_read_meta", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    let last = tb.leader_last_log_index();
    tb.wait_applied(last, CLUSTER_WAIT);
    let f = others(&tb, &[leader])[0];
    for (id, mode) in [
        (leader, ReadMode::Local),
        (leader, ReadMode::Linearizable),
        (f, ReadMode::Linearizable),
    ] {
        let c = reader(&[tb.node(id).endpoint()], mode, Duration::from_secs(5));
        assert_eq!(files(&c).unwrap(), 1);
        let m = c.read_log().last().expect("a read meta");
        assert!(m.applied_index >= last, "node {id} {mode:?}: {m:?}");
        assert!(!m.stale_possible, "node {id} {mode:?}: {m:?}");
        assert_eq!(c.read_log().stale_possible(), Some(false));
    }
    // A caught-up follower hears the leader's heartbeats: fresh.
    let c = reader(
        &[tb.node(f).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    wait_until("the follower's local read to be fresh", || {
        files(&c).unwrap();
        !c.read_log().last().unwrap().stale_possible
    });
    let m = c.read_log().last().unwrap();
    assert!(
        m.leader_committed_index
            .is_some_and(|lc| lc <= m.applied_index),
        "{m:?}"
    );
}

/// `n3` gets no `AppendEntries`: a write acked by the leader is not on it.
/// A linearizable read there waits for the leader's read index (the test
/// sees it parked, then heals) and includes the write; a local read does
/// not, and says `stale_possible`.
#[test]
fn linearizable_read_on_lagging_follower_sees_the_write() {
    let _w = watchdog(
        "linearizable_read_on_lagging_follower_sees_the_write",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = others(&tb, &[leader])[0];
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    // `f` gets no more appends (and must not campaign meanwhile).
    let f_raft = tb.node(f).raft().unwrap().clone();
    f_raft.raft.runtime_config().elect(false);
    tb.drop_append_entries_to(f);
    index_files(&tb.client(leader), "o", "r", &[small_file(1)]);

    // LOCAL on the laggard: the old answer, flagged stale once the
    // leader's silence outlasts an election timeout.
    let local = reader(
        &[tb.node(f).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    wait_until("a stale_possible local read on the laggard", || {
        assert_eq!(files(&local).unwrap(), 1, "the laggard is behind");
        local.read_log().last().unwrap().stale_possible
    });
    assert_eq!(local.read_log().stale_possible(), Some(true));

    // LINEARIZABLE on the laggard: parks on the read index, then (healed)
    // answers with the write.
    let ep = tb.node(f).endpoint();
    let read = std::thread::spawn(move || {
        let lin = reader(&[ep], ReadMode::Linearizable, Duration::from_secs(20));
        let n = files(&lin);
        (n, lin.read_log().last())
    });
    wait_until("the linearizable read to wait for the read index", || {
        f_raft.read_index_waits.load(Ordering::SeqCst) > 0
    });
    tb.heal();
    f_raft.raft.runtime_config().elect(true);
    let (n, meta) = read.join().unwrap();
    assert_eq!(
        n.unwrap(),
        2,
        "the linearizable read includes the acked write"
    );
    let meta = meta.unwrap();
    assert!(!meta.stale_possible, "{meta:?}");
}

/// The old leader is cut off from the majority, which elects a new leader
/// and acks a write. A linearizable read on the old leader (its client
/// connected before the partition) never answers the pre-write state: it
/// fails with `NotLeader`/`NoLeader`, or, after the heal, sees the write.
#[test]
fn stale_leader_linearizable_read_never_returns_old_data() {
    let _w = watchdog(
        "stale_leader_linearizable_read_never_returns_old_data",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    index_files(&tb.client(old), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let lin = reader(
        &[tb.node(old).endpoint()],
        ReadMode::Linearizable,
        Duration::from_millis(500),
    );
    let local = reader(
        &[tb.node(old).endpoint()],
        ReadMode::Local,
        Duration::from_millis(500),
    );
    assert_eq!(files(&lin).unwrap(), 1);
    let majority = others(&tb, &[old]);
    tb.partition(&[old], &majority);
    // A new leader on the majority side, at a higher term.
    let new = {
        let mut found = None;
        wait_until("a new leader on the majority", || {
            found = majority
                .iter()
                .copied()
                .find(|id| tb.node(*id).raft().unwrap().is_leader());
            found.is_some()
        });
        found.unwrap()
    };
    assert_ne!(new, old);
    index_files(&tb.client(new), "o", "r", &[small_file(1)]);
    // The old leader: never the old answer from a linearizable read.
    for _ in 0..5 {
        match files(&lin) {
            Ok(n) => assert_eq!(n, 2, "a stale linearizable read on the old leader"),
            Err(e) => assert!(
                matches!(
                    e,
                    StoreError::NoLeader { .. } | StoreError::NotLeader { .. }
                ),
                "{e:?}"
            ),
        }
    }
    // A local read there still answers, and says it may be stale.
    wait_until("a stale_possible local read on the old leader", || {
        assert_eq!(files(&local).unwrap(), 1);
        local.read_log().last().unwrap().stale_possible
    });
    tb.heal();
    wait_until(
        "the healed old leader to serve the write linearizably",
        || matches!(files(&lin), Ok(2)),
    );
}

/// A node cut off in a minority: a linearizable read fails with `NoLeader`
/// (bounded), never a stale answer; a local read still answers.
#[test]
fn minority_linearizable_read_fails_no_leader() {
    let _w = watchdog("minority_linearizable_read_fails_no_leader", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let m = others(&tb, &[leader])[0];
    let majority = others(&tb, &[m]);
    tb.partition(&[m], &majority);
    index_files(&tb.client(leader), "o", "r", &[small_file(1)]);
    let lin = reader(
        &[tb.node(m).endpoint()],
        ReadMode::Linearizable,
        Duration::from_millis(500),
    );
    let t0 = Instant::now();
    let e = files(&lin).unwrap_err();
    assert!(matches!(e, StoreError::NoLeader { .. }), "{e:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "NoLeader took {:?}",
        t0.elapsed()
    );
    let local = reader(
        &[tb.node(m).endpoint()],
        ReadMode::Local,
        Duration::from_millis(500),
    );
    wait_until("a stale_possible local read in the minority", || {
        assert_eq!(files(&local).unwrap(), 1);
        local.read_log().last().unwrap().stale_possible
    });
    tb.heal();
}

/// The freshness lease of the testbed's timings (ADR 0004 D8).
fn lease() -> Duration {
    graph_server::raft::node::freshness_lease(&TEST_RAFT)
}

/// Dev review 1: the old leader is cut off; once the majority has elected a
/// new leader and acknowledged a write there, a `local` read on the old
/// leader must already say `stale_possible` (no waiting): its quorum lease
/// ends before `election_timeout_min`, and no election can finish sooner.
#[test]
fn old_leader_is_stale_once_a_new_leader_commits() {
    let _w = watchdog("old_leader_is_stale_once_a_new_leader_commits", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    index_files(&tb.client(old), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let local = reader(
        &[tb.node(old).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    wait_until("a fresh local read on the leader", || {
        files(&local).unwrap();
        !local.read_log().last().unwrap().stale_possible
    });
    let majority = others(&tb, &[old]);
    tb.partition(&[old], &majority);
    let mut new = None;
    wait_until("a new leader on the majority", || {
        new = majority
            .iter()
            .copied()
            .find(|id| tb.node(*id).raft().unwrap().is_leader());
        new.is_some()
    });
    index_files(&tb.client(new.unwrap()), "o", "r", &[small_file(1)]);
    // One read, no retry loop: it must already be flagged.
    assert_eq!(files(&local).unwrap(), 1, "the old leader misses the write");
    let m = local.read_log().last().unwrap();
    assert!(
        m.stale_possible,
        "the deposed leader answered a stale read as fresh after a new leader committed: {m:?}"
    );
    tb.heal();
}

/// QA 3: after the leader dies, a follower's `local` reads turn
/// `stale_possible` within the freshness lease (bounded here, with slack
/// for the polling reads, by `election_timeout_max`).
#[test]
fn follower_is_stale_soon_after_the_leader_dies() {
    let _w = watchdog("follower_is_stale_soon_after_the_leader_dies", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let f = others(&tb, &[leader])[0];
    let local = reader(
        &[tb.node(f).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    wait_until("a fresh local read on the follower", || {
        files(&local).unwrap();
        !local.read_log().last().unwrap().stale_possible
    });
    tb.node_mut(leader).kill();
    // The leader is gone: its last AppendEntries reached `f` before now.
    let t0 = Instant::now();
    wait_until("a stale_possible local read on the follower", || {
        files(&local).unwrap();
        local.read_log().last().unwrap().stale_possible
    });
    let took = t0.elapsed();
    // The last contact was at most one heartbeat before the kill, so the
    // lease ends within lease + heartbeat of `t0`; 250 ms of slack covers
    // polling and scheduling (about 700 ms with TEST_RAFT). The old window
    // (election_timeout_max, 1200 ms) would fail this.
    let bound = lease() + Duration::from_millis(TEST_RAFT.heartbeat_ms + 250);
    assert!(
        took <= bound,
        "stale_possible took {took:?}, more than lease + heartbeat + slack {bound:?} (lease {:?})",
        lease()
    );
}

/// Dev review 3: a follower that keeps hearing from the leader (recent
/// contact) but has not applied the commit index the leader sent says
/// `stale_possible`. Its apply is held by the test's apply gate, never a
/// sleep; heartbeats keep arriving meanwhile.
#[test]
fn follower_behind_the_leader_commit_is_stale() {
    let _w = watchdog("follower_behind_the_leader_commit_is_stale", TEST_LIMIT);
    // `held`: the node whose apply is parked (0: none); `parked`: applies
    // waiting in the gate now.
    let gate = Arc::new((Mutex::new(0u64), std::sync::Condvar::new()));
    let parked = Arc::new(AtomicUsize::new(0));
    let mut tb = {
        let (gate, parked) = (Arc::clone(&gate), Arc::clone(&parked));
        ClusterTestbed::with_config(3, exts(), move |id, cfg| {
            let (gate, parked) = (Arc::clone(&gate), Arc::clone(&parked));
            cfg.testing_apply_gate = Some(Arc::new(move |_index| {
                let (held, cv) = &*gate;
                let mut h = held.lock().unwrap();
                if *h == id {
                    parked.fetch_add(1, Ordering::SeqCst);
                    while *h == id {
                        h = cv.wait(h).unwrap();
                    }
                    parked.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        })
    };
    let release = |gate: &(Mutex<u64>, std::sync::Condvar)| {
        *gate.0.lock().unwrap() = 0;
        gate.1.notify_all();
    };
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let f = others(&tb, &[leader])[0];
    *gate.0.lock().unwrap() = f;
    // Acked by the leader and the other follower; `f` appends it but its
    // apply parks.
    index_files(&tb.client(leader), "o", "r", &[small_file(1)]);
    let idx = tb
        .node(leader)
        .raft()
        .unwrap()
        .metrics()
        .last_applied
        .unwrap()
        .index;
    wait_until("f's apply to park in the gate", || {
        parked.load(Ordering::SeqCst) > 0
    });
    let contact = Arc::clone(&tb.node(f).raft().unwrap().obs);
    wait_until("f to hear the leader's commit index", || {
        contact
            .last_leader_contact()
            .is_some_and(|(_, c)| c.is_some_and(|c| c >= idx))
    });
    let local = reader(
        &[tb.node(f).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    let n = files(&local).unwrap();
    let m = local.read_log().last().unwrap();
    let (at, _) = contact.last_leader_contact().unwrap();
    release(&gate);
    assert_eq!(n, 1, "f has not applied the write");
    assert!(
        m.leader_committed_index
            .is_some_and(|c| c > m.applied_index),
        "{m:?}"
    );
    assert!(m.stale_possible, "applied < leader_commit is stale: {m:?}");
    // The contact was recent, so only the applied < commit rule flagged it
    // (reported, not asserted: a slow CI may delay one heartbeat).
    eprintln!(
        "last contact {:?} before the check (lease {:?})",
        at.elapsed(),
        lease()
    );
    wait_until("f to apply and read fresh", || {
        files(&local).unwrap() == 2 && !local.read_log().last().unwrap().stale_possible
    });
}

/// Dev review 7 and a QA gap: a linearizable snapshot handle opened on a
/// lagging follower holds the acked write, and its reads are never
/// `stale_possible` (the handle's meta is frozen at the open), even once the
/// node itself is stale; a `local` handle opened then says it is.
#[test]
fn linearizable_snapshot_on_lagging_follower_has_the_write() {
    let _w = watchdog(
        "linearizable_snapshot_on_lagging_follower_has_the_write",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = others(&tb, &[leader])[0];
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let f_raft = tb.node(f).raft().unwrap().clone();
    f_raft.raft.runtime_config().elect(false);
    tb.drop_append_entries_to(f);
    index_files(&tb.client(leader), "o", "r", &[small_file(1)]);
    let ep = tb.node(f).endpoint();
    let (tx_go, rx_go) = std::sync::mpsc::channel::<()>();
    let (tx_res, rx_res) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let lin = reader(&[ep], ReadMode::Linearizable, Duration::from_secs(20));
        let snap = graph_store::Store::snapshot(&lin).unwrap();
        let first = (snap.count_nodes(NodeKind::File), lin.read_log().last());
        tx_res.send(first).unwrap();
        // Read again once the test made the node stale.
        rx_go.recv_timeout(CLUSTER_WAIT).unwrap();
        let again = (snap.count_nodes(NodeKind::File), lin.read_log().last());
        tx_res.send(again).unwrap();
    });
    wait_until("the linearizable open to wait for the read index", || {
        f_raft.read_index_waits.load(Ordering::SeqCst) > 0
    });
    tb.heal();
    let (n, m) = rx_res.recv_timeout(CLUSTER_WAIT).unwrap();
    assert_eq!(n.unwrap(), 2, "the handle holds the acked write");
    assert!(!m.unwrap().stale_possible, "{m:?}");
    // Make `f` stale again, then read through the same handle.
    tb.drop_append_entries_to(f);
    let local = reader(
        &[tb.node(f).endpoint()],
        ReadMode::Local,
        Duration::from_secs(5),
    );
    wait_until("f to be stale", || {
        files(&local).unwrap();
        local.read_log().last().unwrap().stale_possible
    });
    tx_go.send(()).unwrap();
    let (n, m) = rx_res.recv_timeout(CLUSTER_WAIT).unwrap();
    t.join().unwrap();
    assert_eq!(n.unwrap(), 2);
    assert!(
        !m.unwrap().stale_possible,
        "a linearizable handle's reads are never stale_possible: {m:?}"
    );
    // A local handle opened on the stale node says so.
    let snap = graph_store::Store::snapshot(&local).unwrap();
    snap.count_nodes(NodeKind::File).unwrap();
    assert!(local.read_log().last().unwrap().stale_possible);
    drop(snap);
    tb.heal();
    f_raft.raft.runtime_config().elect(true);
}

/// Dev review 6: a snapshot handle is node-local, so its reads stay on the
/// node that opened it even after the connection moved to another endpoint
/// (here: a linearizable read on a partitioned node answered `NoLeader`
/// and the client rotated).
#[test]
fn snapshot_reads_stay_on_the_node_that_opened_the_handle() {
    let _w = watchdog(
        "snapshot_reads_stay_on_the_node_that_opened_the_handle",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let f = others(&tb, &[leader])[0];
    let other = others(&tb, &[f])[0];
    let eps = [tb.node(f).endpoint(), tb.node(other).endpoint()];
    let c = reader(&eps, ReadMode::Linearizable, Duration::from_secs(20));
    let snap = graph_store::Store::snapshot(&c).unwrap();
    assert_eq!(snap.count_nodes(NodeKind::File).unwrap(), 1);
    assert_eq!(c.endpoint(), eps[0]);
    // `f` loses the majority: a linearizable read there is `NoLeader`, the
    // client rotates to `other`.
    tb.partition(&[f], &others(&tb, &[f]));
    assert_eq!(files(&c).unwrap(), 1);
    assert_eq!(c.endpoint(), eps[1], "the client moved on");
    // The handle's reads still go to `f` (it exists only there).
    assert_eq!(
        snap.count_nodes(NodeKind::File).unwrap(),
        1,
        "a snapshot read followed the rotation"
    );
    drop(snap);
    tb.heal();
}

/// A QA gap: the node a client is using dies mid-session; its next read
/// moves to another endpoint of the list.
#[test]
fn client_moves_on_when_its_node_dies() {
    let _w = watchdog("client_moves_on_when_its_node_dies", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let f = others(&tb, &[leader])[0];
    let mut eps = vec![tb.node(f).endpoint()];
    eps.extend(others(&tb, &[f]).iter().map(|i| tb.node(*i).endpoint()));
    let c = reader(&eps, ReadMode::Local, Duration::from_secs(10));
    assert_eq!(files(&c).unwrap(), 1);
    assert_eq!(c.endpoint(), eps[0]);
    tb.node_mut(f).kill();
    assert_eq!(files(&c).unwrap(), 1);
    assert_ne!(c.endpoint(), eps[0], "the read moved off the dead node");
}

/// Dev review 5 / QA 4: with several endpoints, a dead one first in the
/// list costs one short connect attempt, not the retry budget (30 s here)
/// nor a full connect timeout.
#[test]
fn a_dead_first_endpoint_is_skipped_fast() {
    let _w = watchdog("a_dead_first_endpoint_is_skipped_fast", TEST_LIMIT);
    let tb = ClusterTestbed::new(1, exts());
    let live = tb.node(tb.leader()).endpoint();
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let mut cfg = ClientConfig::new(dead.clone());
    cfg.endpoints = vec![dead, live.clone()];
    cfg.retry.budget = Duration::from_secs(30);
    let t0 = Instant::now();
    let c = RemoteStore::connect(cfg).unwrap();
    let took = t0.elapsed();
    assert_eq!(c.hello().endpoint, live);
    assert!(
        took < Duration::from_millis(1500),
        "connect took {took:?} (quick pass: {:?} connect timeout, no retries)",
        graph_client::QUICK_CONNECT_TIMEOUT
    );
}

/// A tiny deterministic generator (SplitMix64): the checker's choices come
/// from a fixed seed, so a failing run names the seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The history checker (ADR 0004 test plan D): writers index into their
/// own repos through any node while readers read linearizably, and a seeded
/// fault schedule runs meanwhile: `AppendEntries` to a random node dropped
/// for a while, a leadership transfer to a random voter, a random node
/// (the leader too) partitioned from the others for a while. Each fault is
/// healed before the next. The property checked, for every read:
///
/// * **Linearizability (as observed):** every write acknowledged before a
///   linearizable read started is visible to it (its repo has at least as
///   many files as were acked before the read began).
/// * **Only safe failures:** a linearizable read either answers or fails
///   with `NoLeader`/`NotLeader`; any other error is a violation, and it is
///   never `stale_possible`.
/// * **Per-node monotonicity:** `applied_index` never goes back across the
///   reads one client made on one node. Readers may move between nodes (a
///   node that answers `NoLeader` is skipped), and different nodes may be
///   at different applied indexes, so the check is per client per node;
///   across nodes the first property is what must hold.
///
/// `MEMORY_GRAPH_HISTORY_SECS` (default 10) and `MEMORY_GRAPH_HISTORY_SEED`
/// override the run length and the seed; a failure prints the seed.
#[test]
fn history_checker() {
    let _w = watchdog("history_checker", TEST_LIMIT);
    let secs: u64 = std::env::var("MEMORY_GRAPH_HISTORY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let seed: u64 = std::env::var("MEMORY_GRAPH_HISTORY_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x05EE_DD23);
    const WRITERS: usize = 3;
    const READERS: usize = 3;
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let ids = tb.ids();
    let endpoints: Vec<String> = ids.iter().map(|i| tb.node(*i).endpoint()).collect();
    // acked[w] = files of repo w whose write returned Ok.
    let acked: Arc<Vec<AtomicUsize>> =
        Arc::new((0..WRITERS).map(|_| AtomicUsize::new(0)).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let failures: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let (acked, stop, eps) = (Arc::clone(&acked), Arc::clone(&stop), endpoints.clone());
        let mut rng = Rng(seed ^ (w as u64 + 1));
        handles.push(std::thread::spawn(move || {
            // Start on a seeded node; writes are forwarded to the leader.
            let mut order = eps.clone();
            order.rotate_left(rng.below(eps.len() as u64) as usize);
            let mut cfg = ClientConfig::new(order[0].clone());
            cfg.endpoints = order;
            cfg.write_deadline = Duration::from_secs(20);
            let c = RemoteStore::connect(cfg).unwrap();
            let (mut i, mut unacked) = (0usize, 0usize);
            while !stop.load(Ordering::SeqCst) {
                let (p, b) = small_file(i);
                match graph_store::Store::index_bytes(&c, "h", &format!("w{w}"), &p, &b, None) {
                    Ok(_) => {
                        acked[w].fetch_add(1, Ordering::SeqCst);
                    }
                    // Not acknowledged: it may or may not land; never
                    // counted in a floor.
                    Err(StoreError::NoLeader { .. } | StoreError::NotLeader { .. }) => unacked += 1,
                    Err(e) => panic!("writer {w} file {i}: {e}"),
                }
                i += 1;
                std::thread::sleep(Duration::from_millis(rng.below(20)));
            }
            (acked[w].load(Ordering::SeqCst), unacked)
        }));
    }
    let mut readers = Vec::new();
    for r in 0..READERS {
        let (acked, stop, failures) =
            (Arc::clone(&acked), Arc::clone(&stop), Arc::clone(&failures));
        let mut order = endpoints.clone();
        order.rotate_left(r % endpoints.len());
        let mut rng = Rng(seed ^ (0x100 + r as u64));
        readers.push(std::thread::spawn(move || {
            let c = reader(&order, ReadMode::Linearizable, Duration::from_secs(10));
            let mut last_applied: BTreeMap<String, u64> = BTreeMap::new();
            let (mut reads, mut refused) = (0usize, 0usize);
            while !stop.load(Ordering::SeqCst) {
                // What was acknowledged before this read starts.
                let floor: Vec<usize> = acked.iter().map(|a| a.load(Ordering::SeqCst)).collect();
                let infos = match c.describe(Some("h"), None) {
                    Ok(i) => i,
                    Err(StoreError::NoLeader { .. } | StoreError::NotLeader { .. }) => {
                        refused += 1;
                        continue;
                    }
                    Err(e) => {
                        failures.lock().unwrap().push(format!(
                            "reader {r}: a linearizable read failed with {e:?} (only NoLeader/NotLeader are allowed)"
                        ));
                        continue;
                    }
                };
                let node = c.endpoint();
                let meta = c.read_log().last().expect("a read meta");
                let seen: BTreeMap<String, usize> =
                    infos.into_iter().map(|i| (i.repo, i.files)).collect();
                let mut f = failures.lock().unwrap();
                let prev = last_applied.insert(node.clone(), meta.applied_index);
                if let Some(prev) = prev.filter(|p| meta.applied_index < *p) {
                    f.push(format!(
                        "reader {r} on {node}: applied_index went back {prev} -> {}",
                        meta.applied_index
                    ));
                }
                if meta.stale_possible {
                    f.push(format!(
                        "reader {r} on {node}: a linearizable read was stale_possible"
                    ));
                }
                for (w, min) in floor.iter().enumerate() {
                    let got = seen.get(&format!("w{w}")).copied().unwrap_or(0);
                    if got < *min {
                        f.push(format!(
                            "reader {r} on {node}: repo w{w} has {got} files, {min} were acked before the read"
                        ));
                    }
                }
                drop(f);
                reads += 1;
                std::thread::sleep(Duration::from_millis(rng.below(10)));
            }
            (reads, refused)
        }));
    }
    // The fault schedule, on this thread: seeded choices, each fault held
    // for a seeded while, healed, then a seeded pause.
    let mut rng = Rng(seed ^ 0xFA17);
    let admin = {
        let mut cfg = ClientConfig::new(endpoints[0].clone());
        cfg.endpoints = endpoints.clone();
        cfg.write_deadline = Duration::from_secs(10);
        RemoteStore::connect(cfg).unwrap()
    };
    let end = Instant::now() + Duration::from_secs(secs);
    let mut faults: Vec<String> = Vec::new();
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200 + rng.below(400)));
        let target = ids[rng.below(ids.len() as u64) as usize];
        let hold = Duration::from_millis(300 + rng.below(900));
        match rng.below(3) {
            0 => {
                faults.push(format!("drop appends to {target} for {hold:?}"));
                tb.drop_append_entries_to(target);
                std::thread::sleep(hold);
                tb.heal();
            }
            1 => {
                let r = admin.admin_transfer_leader(target);
                faults.push(format!("transfer leadership to {target}: {r:?}"));
            }
            _ => {
                faults.push(format!("partition {target} away for {hold:?}"));
                tb.partition(&[target], &others(&tb, &[target]));
                std::thread::sleep(hold);
                tb.heal();
            }
        }
    }
    tb.heal();
    stop.store(true, Ordering::SeqCst);
    let (mut writes, mut unacked) = (0, 0);
    for h in handles {
        let (a, u) = h.join().unwrap();
        writes += a;
        unacked += u;
    }
    let (mut reads, mut refused) = (0, 0);
    for h in readers {
        let (a, u) = h.join().unwrap();
        reads += a;
        refused += u;
    }
    let f = failures.lock().unwrap();
    assert!(
        f.is_empty(),
        "seed {seed:#x}: {} violations:\n{}\nfaults:\n{}",
        f.len(),
        f.join("\n"),
        faults.join("\n")
    );
    assert!(
        writes > 0 && reads > READERS,
        "seed {seed:#x}: {writes} writes, {reads} reads"
    );
    eprintln!(
        "history_checker seed {seed:#x}: {writes} acked writes ({unacked} not acked), \
         {reads} linearizable reads ({refused} refused NoLeader/NotLeader), {} faults, no violation:\n{}",
        faults.len(),
        faults.join("\n")
    );
}
