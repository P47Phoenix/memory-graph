//! Stage C (ADR 0004 D6/D8/D9, epic story 22): membership and write
//! forwarding through the in-process `ClusterTestbed`. Every wait has a
//! hard timeout and says what it waited for; faults come from the shared
//! fault plan, never from sleeping and hoping.
mod support;
use support::*;

use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::{Extractor, NodeKind};
use graph_proto::pb;
use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT};
use graph_server::{InitMode, JoinSpec, NodeJson, RaftSettings, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::{BatchFile, IndexOptions, Store, StoreError, StoreRead, ORIGIN_DIRECTORY};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn rust_only() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// The configuration of node `id` joining through node 1.
fn join_cfg(tb: &ClusterTestbed, id: u64, auto_promote: bool) -> ServeConfig {
    let mut spec = JoinSpec::new(tb.node(1).endpoint());
    spec.auto_promote = auto_promote;
    spec.timeout = Duration::from_secs(20);
    tb.node_config(id, InitMode::Join(spec))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A raw `Write` client of `endpoint` (to see `forwarded_to_leader`).
async fn write_client(
    endpoint: &str,
) -> pb::write_client::WriteClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        graph_proto::SendVersion,
    >,
> {
    let ch = tonic::transport::Endpoint::from_shared(format!("http://{endpoint}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    pb::write_client::WriteClient::with_interceptor(ch, graph_proto::SendVersion)
}

fn file_bytes(i: usize) -> pb::FileBytes {
    let (path, bytes) = small_file(i);
    pb::FileBytes {
        path,
        bytes,
        language: None,
        origin: Some(ORIGIN_DIRECTORY.into()),
    }
}

fn node_ids_other_than(tb: &ClusterTestbed, not: &[u64]) -> Vec<u64> {
    tb.ids().into_iter().filter(|i| !not.contains(i)).collect()
}

fn cluster_id(tb: &ClusterTestbed, id: u64) -> String {
    tb.client(id).admin_status().unwrap().cluster_id
}

/// A client of node `id` with a short write deadline.
fn short_client(tb: &ClusterTestbed, id: u64, deadline: Duration) -> RemoteStore {
    let mut cfg = ClientConfig::new(tb.node(id).endpoint());
    cfg.write_deadline = deadline;
    RemoteStore::connect(cfg).unwrap()
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

/// A write sent to a follower is forwarded server-side: the answer is the
/// leader's (`forwarded_to_leader`, the leader's applied index), `Index`
/// streams through, the follower counts it, and a linearizable read on the
/// follower sees a write acknowledged by the leader.
#[test]
fn write_via_follower_is_forwarded() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    let (one, many, direct) = rt().block_on(async {
        let mut w = write_client(&tb.node(f).endpoint()).await;
        let one = w
            .index_file(pb::IndexFileRequest {
                org: "o".into(),
                repo: "r".into(),
                file: Some(file_bytes(0)),
                options: None,
            })
            .await
            .unwrap()
            .into_inner();
        let mut msgs = vec![pb::IndexRequest {
            msg: Some(pb::index_request::Msg::Header(pb::IndexHeader {
                org: "o".into(),
                repo: "r".into(),
                options: None,
            })),
        }];
        for i in 1..=10 {
            msgs.push(pb::IndexRequest {
                msg: Some(pb::index_request::Msg::File(file_bytes(i))),
            });
        }
        let many = w
            .index(tokio_stream::iter(msgs))
            .await
            .unwrap()
            .into_inner();
        let direct = write_client(&tb.node(leader).endpoint())
            .await
            .index_file(pb::IndexFileRequest {
                org: "o".into(),
                repo: "r".into(),
                file: Some(file_bytes(11)),
                options: None,
            })
            .await
            .unwrap()
            .into_inner();
        (one, many, direct)
    });
    assert!(one.forwarded_to_leader && many.forwarded_to_leader);
    assert!(!direct.forwarded_to_leader, "the leader wrote it itself");
    assert_eq!(many.results.len(), 10);
    assert!(one.applied_index < many.applied_index);
    assert!(many.applied_index < direct.applied_index);
    // The applied index is the leader's: its own next write came right
    // after, and its last log entry is that write.
    let last = tb.leader_last_log_index();
    assert_eq!(direct.applied_index, last);
    tb.wait_applied(last, CLUSTER_WAIT);
    for id in tb.ids() {
        assert_eq!(
            tb.client(id).count_nodes(NodeKind::File).unwrap(),
            12,
            "node {id}"
        );
    }
    let st = tb.client(f).admin_status().unwrap();
    assert_eq!(st.writes_forwarded_total, 2, "the follower counted both");
    assert_eq!(
        tb.client(leader)
            .admin_status()
            .unwrap()
            .writes_forwarded_total,
        0
    );
    // The client library through a follower: no endpoint switch needed.
    let c = tb.client(f);
    let (p, b) = small_file(12);
    c.index_bytes("o", "r", &p, &b, None).unwrap();
    assert_eq!(c.hello().node_id, f);
    // Linearizable on the follower: the leader's read index, then local.
    let (p, b) = small_file(13);
    tb.client(leader)
        .index_bytes("o", "r", &p, &b, None)
        .unwrap();
    let mut cfg = ClientConfig::new(tb.node(f).endpoint());
    cfg.read_mode = ReadMode::Linearizable;
    let lin = RemoteStore::connect(cfg).unwrap();
    assert_eq!(lin.count_nodes(NodeKind::File).unwrap(), 14);
}

/// `--join --auto-promote` on an empty directory: node 4 takes the cluster
/// id, joins as a learner, catches up and becomes a voter.
#[test]
fn join_auto_promote_from_empty() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let files: Vec<_> = (0..6).map(small_file).collect();
    index_files(&tb.client(tb.leader()), "o", "r", &files);
    let cfg = join_cfg(&tb, 4, true);
    tb.add_node(4, cfg, exts()).unwrap();
    tb.wait_voters(&[1, 2, 3, 4], CLUSTER_WAIT);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let leader = tb.leader();
    assert_eq!(summary(&tb.client(4)), summary(&tb.client(leader)));
    let json = NodeJson::read(&tb.data_dir(4).join("node.json"))
        .unwrap()
        .unwrap();
    assert_eq!(json.cluster_id, Some(cluster_id(&tb, 1)));
    assert!(!json.bootstrapped);
    assert_eq!(tb.client(4).admin_status().unwrap().role, "follower");
}

/// `--join` without `--auto-promote` (`--standby`): a learner that
/// replicates and serves reads, and forwards writes, but is never
/// promoted.
#[test]
fn standby_stays_learner() {
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = join_cfg(&tb, 2, false);
    tb.add_node(2, cfg, exts()).unwrap();
    let c2 = tb.client(2);
    let files: Vec<_> = (0..4).map(small_file).collect();
    // Through the learner: forwarded to the leader.
    index_files(&c2, "o", "r", &files);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(c2.count_nodes(NodeKind::File).unwrap(), 4);
    // Caught up (lag zero): an auto-promote would have happened by now.
    let st = tb.client(1).admin_status().unwrap();
    let lag = st.replication.iter().find(|p| p.node_id == 2).unwrap();
    assert_eq!(lag.lag, 0, "{lag:?}");
    let (voters, learners) = tb.membership(1);
    assert_eq!(voters.into_iter().collect::<Vec<_>>(), [1]);
    assert_eq!(learners.into_iter().collect::<Vec<_>>(), [2]);
    assert_eq!(c2.admin_status().unwrap().role, "learner");
    assert!(c2.admin_status().unwrap().writes_forwarded_total >= 1);
}

/// A joiner with another extractor version set is refused by the leader
/// (the start fails; nothing is added).
#[test]
fn join_refuses_other_extractor_hash() {
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = join_cfg(&tb, 2, true);
    let e = tb.add_node(2, cfg, rust_only()).unwrap_err();
    assert!(e.to_string().contains("extractor"), "{e}");
    assert_eq!(tb.membership(1).0.len(), 1);
    assert!(tb.membership(1).1.is_empty(), "nothing was added");
}

/// A learner restarted with another extractor version set is refused
/// promotion (the gate asks it again at promotion time).
#[test]
fn promote_refuses_other_extractor_hash() {
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = join_cfg(&tb, 2, false);
    tb.add_node(2, cfg, exts()).unwrap();
    wait_until("node 2 is a learner", || tb.membership(1).1.contains(&2));
    tb.node_mut(2).stop();
    tb.node_mut(2).set_extractors(rust_only());
    tb.node_mut(2).restart();
    let e = tb.client(1).admin_promote(2).unwrap_err();
    assert!(e.to_string().contains("extractor"), "{e}");
    assert!(tb.membership(1).1.contains(&2), "still a learner");
    // Unknown ids and voters are refused too.
    let e = tb.client(1).admin_promote(9).unwrap_err();
    assert!(e.to_string().contains("not a member"), "{e}");
    let e = tb.client(1).admin_promote(1).unwrap_err();
    assert!(e.to_string().contains("already a voter"), "{e}");
}

/// Restarting with the same `--bootstrap` / `--join --auto-promote`
/// command lines resumes from persisted state: same cluster id, same
/// members, same data; nothing re-bootstraps or re-joins.
#[test]
fn restart_with_same_bootstrap_or_join_flags_is_idempotent() {
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = join_cfg(&tb, 2, true);
    tb.add_node(2, cfg, exts()).unwrap();
    tb.wait_voters(&[1, 2], CLUSTER_WAIT);
    let files: Vec<_> = (0..5).map(small_file).collect();
    index_files(&tb.client(1), "o", "r", &files);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let id = cluster_id(&tb, 1);
    let before = summary(&tb.client(1));
    let members = member_list(&tb.client(1));
    tb.node_mut(2).stop();
    tb.node_mut(1).stop();
    // The same configurations as the first start (InitMode kept).
    assert!(matches!(
        tb.node_mut(1).config_mut().init,
        InitMode::Bootstrap { .. }
    ));
    assert!(matches!(
        tb.node_mut(2).config_mut().init,
        InitMode::Join(_)
    ));
    tb.node_mut(1).restart();
    tb.node_mut(2).restart();
    tb.wait_voters(&[1, 2], CLUSTER_WAIT);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    for n in [1, 2] {
        let c = tb.client(n);
        assert_eq!(c.admin_status().unwrap().cluster_id, id, "node {n}");
        assert_eq!(summary(&c), before, "node {n}");
        assert_eq!(member_list(&c), members, "node {n}");
    }
    let (p, b) = small_file(9);
    tb.client(2).index_bytes("o", "r", &p, &b, None).unwrap();
}

/// A data directory of cluster B restarted with `--join` to a node of
/// cluster A is refused with a typed `WrongCluster` (FAILED_PRECONDITION),
/// before anything is opened.
#[test]
fn wrong_cluster_is_refused() {
    let tb = ClusterTestbed::new(1, exts());
    let a = cluster_id(&tb, 1);
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("b");
    let mut cfg = ServeConfig::for_data_dir(
        &dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap { restore: None },
        Some(2),
    );
    cfg.raft = Some(RaftSettings::standalone());
    let b = {
        let s = TestServer::try_start_config(cfg.clone(), exts()).unwrap();
        let id = RemoteStore::connect(ClientConfig::new(s.endpoint()))
            .unwrap()
            .admin_status()
            .unwrap()
            .cluster_id;
        drop(s);
        id
    };
    assert_ne!(a, b);
    let node_json = std::fs::read(dir.join("node.json")).unwrap();
    let mut spec = JoinSpec::new(tb.node(1).endpoint());
    spec.auto_promote = true;
    cfg.init = InitMode::Join(spec);
    let e = TestServer::try_start_config(cfg, exts())
        .err()
        .expect("refused");
    match &e {
        StoreError::WrongCluster { expected, found } => {
            assert_eq!((expected, found), (&a, &b));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        graph_proto::store_error_to_status(&e).code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(std::fs::read(dir.join("node.json")).unwrap(), node_json);
    assert_eq!(tb.membership(1).0.len(), 1, "cluster A is untouched");
}

/// `--join` into a directory holding a store but no node.json (a `--db`
/// file copied in) is refused unless `--accept-snapshot-overwrite`; with
/// it, the old files are moved aside and the node holds the leader's data.
#[test]
fn join_into_non_empty_store_requires_accept_snapshot_overwrite() {
    let mut tb = ClusterTestbed::new(1, exts());
    index_files(
        &tb.client(1),
        "o",
        "r",
        &(0..3).map(small_file).collect::<Vec<_>>(),
    );
    let dir = tb.root().join("node2");
    std::fs::create_dir_all(&dir).unwrap();
    {
        let stray = graph_store::V2Store::open(dir.join("graph.redb")).unwrap();
        stray
            .index_bytes("stray", "s", "stray.rs", b"fn stray() {}", None)
            .unwrap();
    }
    let before = std::fs::read(dir.join("graph.redb")).unwrap();
    let e = tb.add_node(2, join_cfg(&tb, 2, false), exts()).unwrap_err();
    assert!(
        matches!(e, StoreError::Rejected(ref m) if m.contains("--accept-snapshot-overwrite")),
        "{e:?}"
    );
    assert_eq!(std::fs::read(dir.join("graph.redb")).unwrap(), before);
    assert!(!dir.join("node.json").exists());
    let mut cfg = join_cfg(&tb, 2, false);
    if let InitMode::Join(spec) = &mut cfg.init {
        spec.accept_snapshot_overwrite = true;
    }
    tb.add_node(2, cfg, exts()).unwrap();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let c2 = tb.client(2);
    assert_eq!(summary(&c2), summary(&tb.client(1)));
    assert!(c2.describe(Some("stray"), None).unwrap().is_empty());
    let replaced: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("replaced-")
        })
        .collect();
    assert_eq!(replaced.len(), 1, "{replaced:?}");
    assert_eq!(
        std::fs::read(replaced[0].join("graph.redb")).unwrap(),
        before
    );
}

/// Wait until the leader's `Status` shows `pred` of node `peer`'s
/// replication entry (`last_error`, `lag`).
fn wait_peer(tb: &ClusterTestbed, peer: u64, what: &str, pred: impl Fn(&pb::PeerLag) -> bool) {
    wait_until(what, || {
        tb.client(tb.leader())
            .admin_status()
            .unwrap()
            .replication
            .iter()
            .any(|p| p.node_id == peer && pred(p))
    });
}

/// `Remove` guards: the leader (transfer first), 3 voters to 2 without
/// `--force`, and below quorum (a voter down) even with `--force`; `--force`
/// with every voter up works.
#[test]
fn remove_guards() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let others = node_ids_other_than(&tb, &[leader]);
    let (a, b) = (others[0], others[1]);
    // Through a follower: forwarded to the leader, which refuses.
    let via = tb.client(a);
    let e = via.admin_remove(leader, true).unwrap_err();
    assert!(
        matches!(e, StoreError::Rejected(ref m) if m.contains("transfer leadership first")),
        "{e:?}"
    );
    let e = via.admin_remove(a, false).unwrap_err();
    assert!(e.to_string().contains("--force"), "{e}");
    let e = via.admin_remove(42, true).unwrap_err();
    assert!(e.to_string().contains("not a member"), "{e}");
    // A voter down: removing another would leave one reachable voter of
    // two, below a quorum of 2.
    tb.node_mut(b).kill();
    wait_peer(&tb, b, "the leader to see node b failing", |p| {
        !p.last_error.is_empty()
    });
    let e = tb.client(leader).admin_remove(a, true).unwrap_err();
    assert!(e.to_string().contains("below quorum"), "{e}");
    assert_eq!(tb.membership(leader).0.len(), 3);
    // Back up and caught up: --force takes 3 voters to 2.
    tb.node_mut(b).restart();
    wait_peer(&tb, b, "node b healthy again", |p| {
        p.last_error.is_empty() && p.lag == 0 && p.matched_index.is_some()
    });
    tb.client(leader).admin_remove(a, true).unwrap();
    tb.wait_voters(&[leader, b], CLUSTER_WAIT);
    let (p, bytes) = small_file(1);
    tb.client(leader)
        .index_bytes("o", "r", &p, &bytes, None)
        .unwrap();
}

/// `TransferLeader` (sent to a follower, forwarded) moves leadership to
/// the target; writes work on the new leader and through the old one.
#[test]
fn transfer_leader_moves_leadership() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    let others = node_ids_other_than(&tb, &[old]);
    let (target, via) = (others[0], others[1]);
    index_files(&tb.client(old), "o", "r", &[small_file(0)]);
    let t0 = Instant::now();
    let now = tb.client(via).admin_transfer_leader(target).unwrap();
    assert_eq!(now, target);
    eprintln!("transfer took {:?}", t0.elapsed());
    assert_eq!(tb.leader(), target);
    wait_until("the old leader to follow the target", || {
        tb.node(old).raft().unwrap().leader().id == Some(target)
    });
    for (i, via) in [(1, target), (2, old)] {
        let (p, b) = small_file(i);
        tb.client(via).index_bytes("o", "r", &p, &b, None).unwrap();
    }
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(tb.client(old).count_nodes(NodeKind::File).unwrap(), 3);
    // To itself: nothing to do. To a non-member: refused.
    assert_eq!(
        tb.client(target).admin_transfer_leader(target).unwrap(),
        target
    );
    let e = tb.client(target).admin_transfer_leader(7).unwrap_err();
    assert!(e.to_string().contains("not a member"), "{e}");
}

/// A partition isolating one follower: `LOCAL` reads on it keep working,
/// its writes fail with `NoLeader` at the client's deadline (it can reach
/// no leader), the majority keeps writing, and once healed it converges.
#[test]
fn partition_minority_serves_local_reads_refuses_writes_and_converges() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let m = node_ids_other_than(&tb, &[leader])[0];
    let majority = node_ids_other_than(&tb, &[m]);
    index_files(
        &tb.client(leader),
        "o",
        "r",
        &(0..3).map(small_file).collect::<Vec<_>>(),
    );
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    tb.partition(&[m], &majority);
    let lonely = short_client(&tb, m, Duration::from_secs(2));
    let t0 = Instant::now();
    let (p, b) = small_file(100);
    let e = lonely.index_bytes("o", "r", &p, &b, None).unwrap_err();
    assert!(matches!(e, StoreError::NoLeader { .. }), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    assert_eq!(lonely.count_nodes(NodeKind::File).unwrap(), 3, "LOCAL read");
    let mut cfg = ClientConfig::new(tb.node(m).endpoint());
    cfg.read_mode = ReadMode::Linearizable;
    cfg.retry.budget = Duration::from_millis(500);
    assert!(
        RemoteStore::connect(cfg)
            .unwrap()
            .count_nodes(NodeKind::File)
            .is_err(),
        "a linearizable read needs the leader"
    );
    index_files(
        &tb.client(leader),
        "o",
        "r",
        &(3..6).map(small_file).collect::<Vec<_>>(),
    );
    tb.heal();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let l = tb.leader();
    assert_eq!(summary(&tb.client(m)), summary(&tb.client(l)));
    assert_eq!(tb.client(m).count_nodes(NodeKind::File).unwrap(), 6);
    run_differential(&replica(&tb, m), &replica(&tb, l));
}

/// A writer indexes batches through the leader while a learner is added,
/// promoted and a voter removed: every acknowledged batch is on every
/// voter afterwards.
#[test]
fn membership_change_under_load_loses_no_acked_write() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let victim = node_ids_other_than(&tb, &[leader])[0];
    let cfg = tb.node_config(4, InitMode::Uninitialized);
    tb.add_node(4, cfg, exts()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let acked: Arc<Mutex<Vec<usize>>> = Arc::default();
    let writer = {
        let c = tb.client(leader);
        let (stop, acked) = (Arc::clone(&stop), Arc::clone(&acked));
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) && i < 400 {
                let batch = [small_file(2 * i), small_file(2 * i + 1)];
                let files: Vec<BatchFile<'_>> = batch
                    .iter()
                    .map(|(p, b)| BatchFile {
                        path: p,
                        bytes: b,
                        language: None,
                        origin: Some(ORIGIN_DIRECTORY),
                    })
                    .collect();
                match c.index_batch("o", "r", &files, IndexOptions::default()) {
                    Ok(r) if r.iter().all(|x| x.is_ok()) => acked.lock().unwrap().push(i),
                    Ok(r) => panic!("batch {i}: {r:?}"),
                    Err(e) => eprintln!("batch {i} not acknowledged: {e}"),
                }
                i += 1;
            }
        })
    };
    let acked_at_least = |n: usize| {
        wait_until(&format!("{n} acknowledged batches"), || {
            acked.lock().unwrap().len() >= n
        })
    };
    acked_at_least(3);
    let admin = tb.client(leader);
    admin
        .admin_add_learner(4, &tb.node(4).endpoint(), true)
        .unwrap();
    acked_at_least(6);
    admin.admin_promote(4).unwrap();
    acked_at_least(9);
    admin.admin_remove(victim, false).unwrap();
    acked_at_least(12);
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    let voters: Vec<u64> = node_ids_other_than(&tb, &[victim]);
    tb.wait_voters(&voters, CLUSTER_WAIT);
    // The removed node gets nothing more: stop it before waiting.
    tb.node_mut(victim).stop();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let acked = acked.lock().unwrap().clone();
    for id in voters {
        let c = tb.client(id);
        for i in &acked {
            for f in [2 * i, 2 * i + 1] {
                assert!(
                    c.file_tokens("o", "r", &small_file(f).0).unwrap().is_some(),
                    "node {id} lost acknowledged batch {i}"
                );
            }
        }
    }
    eprintln!("{} batches acknowledged", acked.len());
}

/// A client retry that re-sends an `IndexChunk` already committed under
/// the previous leader applies again as `unchanged`: counts and `describe`
/// are unchanged, and the replicas still match an embedded oracle.
#[test]
fn duplicate_index_chunk_after_leader_change_is_idempotent() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    let files: Vec<_> = (0..6).map(small_file).collect();
    let batch: Vec<BatchFile<'_>> = files
        .iter()
        .map(|(p, b)| BatchFile {
            path: p,
            bytes: b,
            language: None,
            origin: Some(ORIGIN_DIRECTORY),
        })
        .collect();
    let first = tb
        .client(old)
        .index_batch("o", "r", &batch, IndexOptions::default())
        .unwrap();
    assert!(first.iter().all(|r| !r.as_ref().unwrap().unchanged));
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let before = summary(&tb.client(old));
    let applied_before = tb.client(old).admin_status().unwrap().applied_index;
    let target = node_ids_other_than(&tb, &[old])[0];
    tb.client(old).admin_transfer_leader(target).unwrap();
    // The retry, sent where the client was (the old leader, now a
    // follower): forwarded to the new leader, a new log entry.
    let again = tb
        .client(old)
        .index_batch("o", "r", &batch, IndexOptions::default())
        .unwrap();
    assert!(
        again.iter().all(|r| r.as_ref().unwrap().unchanged),
        "{again:?}"
    );
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let st = tb.client(target).admin_status().unwrap();
    assert!(st.applied_index > applied_before, "the retry was logged");
    for id in tb.ids() {
        let c = tb.client(id);
        assert_eq!(summary(&c), before, "node {id}");
        assert_eq!(c.count_nodes(NodeKind::File).unwrap(), 6, "node {id}");
    }
    let d = tempfile::tempdir().unwrap();
    let embedded = oracle(d.path());
    index_files(embedded.as_ref(), "o", "r", &files);
    assert_eq!(summary(embedded.as_ref()), before);
    run_differential(embedded.as_ref(), &replica(&tb, old));
}
