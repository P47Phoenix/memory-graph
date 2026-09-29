//! Stage D (ADR 0004 D8, epic story 23): linearizable reads and read
//! freshness (`ReadMeta`, `stale_possible`) through the in-process
//! `ClusterTestbed`. Faults come from the shared fault plan and the test
//! hooks; every wait has a hard timeout and names what it waited for.
mod support;
use support::*;

use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::NodeKind;
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
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
}

/// The history checker (ADR 0004 test plan D): writers index into their
/// own repos through any node, readers read linearizably, each bound to
/// one node, for a fixed time. Checked:
///
/// * every read's `applied_index` is monotonic per reader;
/// * every write acknowledged before a linearizable read started is
///   visible to it (the repo has at least that many files).
///
/// `MEMORY_GRAPH_HISTORY_SECS` (default 10) and
/// `MEMORY_GRAPH_HISTORY_SEED` override the run length and the seed.
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
    let endpoints: Vec<String> = tb.ids().iter().map(|i| tb.node(*i).endpoint()).collect();
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
            order.rotate_left((rng.next() % eps.len() as u64) as usize);
            let mut cfg = ClientConfig::new(order[0].clone());
            cfg.endpoints = order;
            cfg.write_deadline = Duration::from_secs(20);
            let c = RemoteStore::connect(cfg).unwrap();
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let (p, b) = small_file(i);
                graph_store::Store::index_bytes(&c, "h", &format!("w{w}"), &p, &b, None)
                    .unwrap_or_else(|e| panic!("writer {w} file {i}: {e}"));
                i += 1;
                acked[w].store(i, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(rng.next() % 20));
            }
            i
        }));
    }
    let mut readers = Vec::new();
    for r in 0..READERS {
        let (acked, stop, failures) =
            (Arc::clone(&acked), Arc::clone(&stop), Arc::clone(&failures));
        let ep = endpoints[r % endpoints.len()].clone();
        let mut rng = Rng(seed ^ (0x100 + r as u64));
        readers.push(std::thread::spawn(move || {
            let c = reader(std::slice::from_ref(&ep), ReadMode::Linearizable, Duration::from_secs(10));
            let (mut last_applied, mut reads) = (0u64, 0usize);
            while !stop.load(Ordering::SeqCst) {
                // What was acknowledged before this read starts.
                let floor: Vec<usize> = acked.iter().map(|a| a.load(Ordering::SeqCst)).collect();
                let infos = c.describe(Some("h"), None).unwrap_or_else(|e| {
                    panic!("reader {r} on {ep}: linearizable read failed: {e}")
                });
                let meta = c.read_log().last().expect("a read meta");
                let seen: BTreeMap<String, usize> =
                    infos.into_iter().map(|i| (i.repo, i.files)).collect();
                let mut f = failures.lock().unwrap();
                if meta.applied_index < last_applied {
                    f.push(format!(
                        "reader {r} on {ep}: applied_index went back {last_applied} -> {}",
                        meta.applied_index
                    ));
                }
                if meta.stale_possible {
                    f.push(format!("reader {r}: a linearizable read was stale_possible"));
                }
                for (w, min) in floor.iter().enumerate() {
                    let got = seen.get(&format!("w{w}")).copied().unwrap_or(0);
                    if got < *min {
                        f.push(format!(
                            "reader {r} on {ep}: repo w{w} has {got} files, {min} were acked before the read"
                        ));
                    }
                }
                drop(f);
                last_applied = meta.applied_index;
                reads += 1;
                std::thread::sleep(Duration::from_millis(rng.next() % 10));
            }
            reads
        }));
    }
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::SeqCst);
    let writes: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let reads: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();
    let f = failures.lock().unwrap();
    assert!(
        f.is_empty(),
        "seed {seed:#x}: {} violations:\n{}",
        f.len(),
        f.join("\n")
    );
    assert!(
        writes > 0 && reads > READERS,
        "seed {seed:#x}: {writes} writes, {reads} reads"
    );
    eprintln!("history_checker seed {seed:#x}: {writes} acked writes, {reads} linearizable reads, no violation");
}
