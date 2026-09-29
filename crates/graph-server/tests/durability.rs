//! QA 4 (ADR 0004 D7): durability under power loss, with the store and the
//! Raft log of every node on a [`PowerCutDisk`] (an instrumented redb
//! storage backend that keeps only what was synced when the power goes).
//! Every wait has a hard timeout; the crash points are chosen by counting
//! syncs, never by timing.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::{Extractor, NodeKind};
use graph_server::powercut::{CutMode, PowerCutDisk};
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_store::{open_store, BatchFile, IndexOptions, RepoInfo, Store, StoreRead};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn exts() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn file(i: usize) -> (String, Vec<u8>) {
    (
        format!("src/f{i}.rs"),
        format!("fn f{i}() -> u32 {{ {i} }}\n").into_bytes(),
    )
}

fn summary(s: &dyn StoreRead) -> (usize, usize, Vec<RepoInfo>) {
    (
        s.count_nodes(NodeKind::File).unwrap(),
        s.count_nodes(NodeKind::Token).unwrap(),
        s.describe(None, None).unwrap(),
    )
}

/// The seeds of the seeded cut modes: fixed, so a failure names its seed
/// and reproduces.
const SEEDS: [u64; 2] = [0x5EED_0001, 0x5EED_0002];

/// Every power-cut model the durability tests run under: drop everything
/// unsynced, and per seed a random subset (with torn pages) and a random
/// prefix (torn at the cut) of it.
fn modes() -> Vec<CutMode> {
    let mut m = vec![CutMode::DropUnsynced];
    for s in SEEDS {
        m.push(CutMode::Subset { seed: s });
        m.push(CutMode::Prefix { seed: s });
    }
    m
}

/// An acknowledged write survives the power going out on every node at
/// once (the strongest case of "a majority"): after the restart each node
/// has it. Under every cut mode.
#[test]
fn an_acked_write_survives_a_power_cut_of_every_node() {
    for mode in modes() {
        acked_write_survives(mode);
    }
}

fn acked_write_survives(mode: CutMode) {
    let disk = PowerCutDisk::new();
    let d2 = disk.clone();
    let mut tb = ClusterTestbed::with_config(3, exts(), move |_, c| {
        c.storage_backend = Some(d2.factory());
    });
    tb.form();
    let c = tb.client(tb.leader());
    let files: Vec<_> = (0..4).map(file).collect();
    // `Write.Index` answers with the log index of its last entry.
    let batch: Vec<BatchFile<'_>> = files
        .iter()
        .map(|(p, b)| BatchFile {
            path: p,
            bytes: b,
            language: None,
            origin: None,
        })
        .collect();
    c.index_batch("o", "r", &batch, IndexOptions::default())
        .unwrap();
    let acked = c.applied_index().load(Ordering::SeqCst);
    assert!(acked > 0);
    // Power off: unsynced bytes are gone, then the processes die.
    for id in tb.ids() {
        disk.power_cut_with(&tb.data_dir(id), mode);
    }
    for id in tb.ids() {
        tb.node_mut(id).kill();
    }
    drop(c);
    for id in tb.ids() {
        tb.node_mut(id).restart();
    }
    tb.wait_applied(acked, CLUSTER_WAIT);
    for id in tb.ids() {
        let c = tb.client(id);
        for (path, _) in &files {
            assert!(
                c.file_tokens("o", "r", path).unwrap().is_some(),
                "{mode:?}: node {id} lost acknowledged {path}"
            );
        }
    }
}

/// The power goes out in the middle of a write, at every sync of the log
/// and then of the store in turn (the disk dies at its n-th sync, which
/// never happens, then keeps what the cut mode says of the writes since
/// the sync before: nothing, a random subset with torn pages, or a random
/// prefix torn at the cut). Each time the node restarts on what is left
/// (after redb's recovery) with no torn state: the store opens, its marker
/// is not above the log, the replay finishes, the store holds either the
/// state before the write or after it (never a part), and a write that was
/// acknowledged is there.
#[test]
fn a_power_cut_mid_write_leaves_no_torn_state() {
    let od = tempfile::tempdir().unwrap();
    let states: Vec<_> = [1usize, 2]
        .iter()
        .map(|n| {
            let o = open_store(&od.path().join(format!("o{n}.redb")), exts()).unwrap();
            for f in (0..*n).map(file) {
                o.index_bytes("o", "r", &f.0, &f.1, None).unwrap();
            }
            summary(o.as_ref())
        })
        .collect();
    let mut cut_before_ack = 0;
    let mut leftovers = 0u64;
    for mode in modes() {
        for target in ["raft.redb", "graph.redb"] {
            for k in 1..=4u64 {
                let disk = PowerCutDisk::new();
                let d2 = disk.clone();
                let mut tb = ClusterTestbed::with_config(1, exts(), move |_, c| {
                    c.storage_backend = Some(d2.factory());
                    c.raft = Some(graph_server::testing::TEST_RAFT);
                });
                let dir = tb.data_dir(1);
                // The same port across the restart.
                let endpoint = tb.node(1).endpoint();
                let client = || {
                    let mut cfg = ClientConfig::new(endpoint.clone());
                    cfg.write_deadline = Duration::from_secs(1);
                    cfg.retry.budget = Duration::from_secs(1);
                    RemoteStore::connect(cfg).unwrap()
                };
                let c = client();
                let warm = file(0);
                c.index_bytes("o", "r", &warm.0, &warm.1, None).unwrap();
                disk.fail_at_sync(&dir.join(target), k);
                let f = file(1);
                let acked = c.index_bytes("o", "r", &f.0, &f.1, None).is_ok();
                if !acked {
                    cut_before_ack += 1;
                }
                drop(c);
                leftovers += disk.power_cut_with(&dir, mode);
                tb.node_mut(1).kill();
                tb.node_mut(1).restart();
                let c = client();
                let deadline = Instant::now() + CLUSTER_WAIT;
                let st = loop {
                    let st = c.admin_status().unwrap();
                    assert!(
                        st.applied_index <= st.last_log_index,
                        "{mode:?} {target} k={k}: marker {} above the log {}",
                        st.applied_index,
                        st.last_log_index
                    );
                    if st.applied_index == st.last_log_index {
                        break st;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{mode:?} {target} k={k}: replay stuck"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                let got = summary(&c);
                assert!(
                    states.contains(&got),
                    "{mode:?} {target} k={k} (acked {acked}, applied {}): a torn state {got:?}",
                    st.applied_index
                );
                if acked {
                    assert_eq!(
                        got, states[1],
                        "{mode:?} {target} k={k}: an acknowledged write was lost"
                    );
                }
            }
        }
    }
    assert!(
        cut_before_ack > 0,
        "no crash point fell before an acknowledgement: the sweep tested nothing"
    );
    assert!(
        leftovers > 0,
        "no seeded cut wrote unsynced bytes back: the torn modes tested nothing"
    );
}
