//! Stage C (ADR 0004 D6/D8/D9, epic story 22): membership and write
//! forwarding through the in-process `ClusterTestbed`. Every wait has a
//! hard timeout and says what it waited for; faults come from the shared
//! fault plan, never from sleeping and hoping.
mod support;
use support::*;

use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_core::{Extractor, NodeKind};
use graph_proto::pb;
use graph_server::raft::log_store::RedbLogStore;
use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT, TEST_RAFT};
use graph_server::{InitMode, JoinSpec, NodeJson, RaftSettings, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::{BatchFile, IndexOptions, Store, StoreError, StoreRead, ORIGIN_DIRECTORY};
use openraft::raft::VoteRequest;
use openraft::storage::RaftLogStorage;
use openraft::{CommittedLeaderId, LogId, ServerState, Vote};
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
        ..Default::default()
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
    let _w = watchdog("write_via_follower_is_forwarded", TEST_LIMIT);
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
    let _w = watchdog("join_auto_promote_from_empty", TEST_LIMIT);
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
    let _w = watchdog("standby_stays_learner", TEST_LIMIT);
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
    let _w = watchdog("join_refuses_other_extractor_hash", TEST_LIMIT);
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
    let _w = watchdog("promote_refuses_other_extractor_hash", TEST_LIMIT);
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
    // Unknown ids are refused too; promoting a voter is an idempotent OK.
    let e = tb.client(1).admin_promote(9).unwrap_err();
    assert!(e.to_string().contains("not a member"), "{e}");
    tb.client(1).admin_promote(1).unwrap();
}

/// Membership changes are idempotent under a client retry: promote twice,
/// add the same learner twice, remove twice; each call succeeds.
#[test]
fn membership_changes_are_idempotent() {
    let _w = watchdog("membership_changes_are_idempotent", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = join_cfg(&tb, 2, false);
    tb.add_node(2, cfg, exts()).unwrap();
    wait_until("node 2 is a learner", || tb.membership(1).1.contains(&2));
    let addr = tb.node(2).endpoint();
    tb.client(1).admin_add_learner(2, &addr, true).unwrap();
    assert!(tb.membership(1).1.contains(&2), "still a learner");
    let e = tb
        .client(1)
        .admin_add_learner(2, "127.0.0.1:1", true)
        .unwrap_err();
    assert!(e.to_string().contains("already a member"), "{e}");
    tb.client(1).admin_promote(2).unwrap();
    tb.client(1).admin_promote(2).unwrap();
    assert!(tb.membership(1).0.contains(&2), "a voter");
    // A voter at the same address: add-learner is a no-op, not a demotion.
    tb.client(1).admin_add_learner(2, &addr, true).unwrap();
    assert!(tb.membership(1).0.contains(&2), "still a voter");
    let first = tb.client(1).admin_remove(2, true).unwrap();
    assert!(!first.not_a_member, "{first:?}");
    // #122: the repeat is still OK, but says there was nothing to remove.
    let again = tb.client(1).admin_remove(2, true).unwrap();
    assert!(again.not_a_member, "{again:?}");
    assert!(!tb.membership(1).0.contains(&2));
    assert!(!tb.membership(1).1.contains(&2));
}

/// Restarting with the same `--bootstrap` / `--join --auto-promote`
/// command lines resumes from persisted state: same cluster id, same
/// members, same data; nothing re-bootstraps or re-joins.
#[test]
fn restart_with_same_bootstrap_or_join_flags_is_idempotent() {
    let _w = watchdog(
        "restart_with_same_bootstrap_or_join_flags_is_idempotent",
        TEST_LIMIT,
    );
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
    let _w = watchdog("wrong_cluster_is_refused", TEST_LIMIT);
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
    cfg.raft = Some(graph_server::testing::TEST_RAFT);
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
    let _w = watchdog(
        "join_into_non_empty_store_requires_accept_snapshot_overwrite",
        TEST_LIMIT,
    );
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
    let _w = watchdog("remove_guards", TEST_LIMIT);
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
    // A non-member: nothing to remove, an idempotent OK (a retried remove).
    // #122: reported, so an operator notices a mistyped id.
    let r = via.admin_remove(42, true).unwrap();
    assert!(r.not_a_member && !r.retried, "{r:?}");
    assert_eq!(tb.membership(leader).0.len(), 3);
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
    let r = tb.client(leader).admin_remove(a, true).unwrap();
    assert!(!r.not_a_member, "{r:?}");
    tb.wait_voters(&[leader, b], CLUSTER_WAIT);
    let (p, bytes) = small_file(1);
    tb.client(leader)
        .index_bytes("o", "r", &p, &bytes, None)
        .unwrap();
}

/// A raw `Admin` client of `endpoint` (no retries, no leader following).
async fn admin_client(
    endpoint: &str,
) -> pb::admin_client::AdminClient<
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
    pb::admin_client::AdminClient::with_interceptor(ch, graph_proto::SendVersion)
}

/// Transfer leadership from the current leader to `target` (asked through
/// node `via`, so forwarded unless `via` leads), and check that exactly the
/// target leads afterwards and the old leader follows it.
fn transfer_and_check(tb: &ClusterTestbed, via: u64, target: u64) -> Duration {
    let old = tb.leader();
    let t0 = Instant::now();
    let now = tb.client(via).admin_transfer_leader(target).unwrap();
    let took = t0.elapsed();
    assert_eq!(now, target, "the transfer answered another leader");
    assert_eq!(
        tb.leader(),
        target,
        "the TARGET must lead, not another node"
    );
    wait_until("the old leader to follow the target", || {
        tb.node(old).raft().unwrap().leader().id == Some(target)
    });
    took
}

/// `TransferLeader` (sent to a follower, forwarded) moves leadership to
/// exactly the target, while a writer keeps writing through the old leader
/// (its writes are refused with `NoLeader` during the transfer and retried,
/// so no append renews the followers' lease: without that pause the target
/// could fall behind the leader's log). Then twice more, around the cluster.
#[test]
fn transfer_leader_moves_leadership() {
    let _w = watchdog("transfer_leader_moves_leadership", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    let others = node_ids_other_than(&tb, &[old]);
    let (target, via) = (others[0], others[1]);
    index_files(&tb.client(old), "o", "r", &[small_file(0)]);
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let c = tb.client(old);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                let (p, b) = small_file(1000 + n);
                c.index_bytes("o", "r", &p, &b, None)
                    .unwrap_or_else(|e| panic!("write {n} during the transfer: {e}"));
                n += 1;
            }
            n
        })
    };
    let transfer = {
        let c = tb.client(via);
        std::thread::spawn(move || {
            let t0 = Instant::now();
            (c.admin_transfer_leader(target), t0.elapsed())
        })
    };
    let (now, took) = transfer.join().unwrap();
    if now.is_err() {
        for id in tb.ids() {
            let m = tb.node(id).raft().unwrap().metrics();
            eprintln!("DBG node {id}: state {:?} term {} leader {:?} vote {:?} last {:?} applied {:?} repl {:?}", m.state, m.current_term, m.current_leader, m.vote, m.last_log_index, m.last_applied, m.replication);
        }
    }
    assert_eq!(now.unwrap(), target);
    eprintln!("transfer 1 (under writes) took {took:?}");
    stop.store(true, Ordering::SeqCst);
    let written = writer.join().unwrap();
    assert_eq!(
        tb.leader(),
        target,
        "the TARGET must lead, not another node"
    );
    wait_until("the old leader to follow the target", || {
        tb.node(old).raft().unwrap().leader().id == Some(target)
    });
    for (i, via) in [(1, target), (2, old)] {
        let (p, b) = small_file(i);
        tb.client(via).index_bytes("o", "r", &p, &b, None).unwrap();
    }
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(
        tb.client(old).count_nodes(NodeKind::File).unwrap(),
        3 + written
    );
    // Around the cluster: to `via` (asked through the old leader), back to
    // `old` (asked on the leader itself).
    eprintln!("transfer 2 took {:?}", transfer_and_check(&tb, old, via));
    eprintln!("transfer 3 took {:?}", transfer_and_check(&tb, via, old));
    // To itself: nothing to do. To a non-member: refused.
    assert_eq!(tb.client(old).admin_transfer_leader(old).unwrap(), old);
    let e = tb.client(old).admin_transfer_leader(7).unwrap_err();
    assert!(e.to_string().contains("not a member"), "{e}");
}

/// In a 5-node cluster the target, not one of the three other followers,
/// takes over; twice.
#[test]
fn transfer_leader_moves_leadership_five_nodes() {
    let _w = watchdog("transfer_leader_moves_leadership_five_nodes", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(5, exts());
    tb.form();
    index_files(&tb.client(tb.leader()), "o", "r", &[small_file(0)]);
    let old = tb.leader();
    let others = node_ids_other_than(&tb, &[old]);
    eprintln!(
        "transfer 1 took {:?}",
        transfer_and_check(&tb, others[1], others[3])
    );
    eprintln!(
        "transfer 2 took {:?}",
        transfer_and_check(&tb, others[3], others[0])
    );
    let (p, b) = small_file(1);
    tb.client(old).index_bytes("o", "r", &p, &b, None).unwrap();
}

/// A partition isolating one follower: `LOCAL` reads on it keep working,
/// its writes fail with `NoLeader` at the client's deadline (it can reach
/// no leader), the majority keeps writing, and once healed it converges.
#[test]
fn partition_minority_serves_local_reads_refuses_writes_and_converges() {
    let _w = watchdog(
        "partition_minority_serves_local_reads_refuses_writes_and_converges",
        TEST_LIMIT,
    );
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
    let _w = watchdog(
        "membership_change_under_load_loses_no_acked_write",
        TEST_LIMIT,
    );
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
                        ..Default::default()
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
    let _w = watchdog(
        "duplicate_index_chunk_after_leader_change_is_idempotent",
        TEST_LIMIT,
    );
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
            ..Default::default()
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

/// Dev review 1: a removal needs a quorum of the OLD voter set too (joint
/// consensus). Four voters with two down: removing one of the dead leaves
/// a new set {leader, b, c} with 2 of 3 up, but the old set has only 2 of
/// 4 up (a quorum is 3), so the change could never commit: refused.
#[test]
fn remove_needs_a_quorum_of_the_old_voter_set_too() {
    let _w = watchdog("remove_needs_a_quorum_of_the_old_voter_set_too", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(4, exts());
    tb.form();
    let leader = tb.leader();
    let others = node_ids_other_than(&tb, &[leader]);
    let (c, d) = (others[1], others[2]);
    tb.node_mut(c).kill();
    tb.node_mut(d).kill();
    for dead in [c, d] {
        wait_peer(&tb, dead, "the leader to see the node failing", |p| {
            !p.last_error.is_empty()
        });
    }
    let e = tb.client(leader).admin_remove(d, true).unwrap_err();
    let m = e.to_string();
    assert!(
        m.contains("below quorum") && m.contains("vote on the change"),
        "{m}"
    );
    assert_eq!(tb.membership(leader).0.len(), 4, "nothing changed");
}

/// Dev review 4: a forward whose leader never answers ends at the bounded
/// default deadline (here shortened by the test hook) with `NoLeader`,
/// instead of holding the follower's handler forever.
#[test]
fn a_forward_to_a_hung_leader_times_out() {
    let _w = watchdog("a_forward_to_a_hung_leader_times_out", TEST_LIMIT);
    let mut tb = ClusterTestbed::with_config(3, exts(), |_, cfg| {
        // Every write proposal parks forever (only the leader proposes);
        // forwards give up after 500 ms when the client set no deadline.
        cfg.testing.stall_writes_after = Some(0);
        cfg.testing.forward_timeout_ms = Some(500);
    });
    tb.form();
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    let t0 = Instant::now();
    let st = rt().block_on(async {
        write_client(&tb.node(f).endpoint())
            .await
            .index_file(pb::IndexFileRequest {
                org: "o".into(),
                repo: "r".into(),
                file: Some(file_bytes(0)),
                options: None,
            })
            .await
            .unwrap_err()
    });
    let took = t0.elapsed();
    assert!(
        matches!(
            graph_proto::status_to_store_error(&st),
            StoreError::NoLeader { .. }
        ),
        "{st:?}"
    );
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// QA 1: an `--auto-promote` learner removed while it is still catching up
/// (its appends are slow) stays removed: its periodic re-join is refused
/// (it is marked `rejoin`), never an add.
#[test]
fn a_removed_auto_promote_learner_stays_removed() {
    let _w = watchdog("a_removed_auto_promote_learner_stays_removed", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(1, exts());
    index_files(
        &tb.client(1),
        "o",
        "r",
        &(0..20).map(small_file).collect::<Vec<_>>(),
    );
    let mut cfg = join_cfg(&tb, 2, true);
    // Slow appends: the learner lags, so it is not promoted before the
    // removal.
    cfg.testing.delay_append_entries_ms = Some(3000);
    tb.add_node(2, cfg, exts()).unwrap();
    assert!(tb.membership(1).1.contains(&2), "a learner");
    tb.client(1).admin_remove(2, false).unwrap();
    wait_until("node 2 to leave the membership", || {
        !tb.membership(1).0.contains(&2) && !tb.membership(1).1.contains(&2)
    });
    // A re-join is refused with a reason, deterministically ...
    let hash = tb.client(1).admin_status().unwrap().extractors_hash;
    let st = rt().block_on(async {
        let mut req = graph_server::join::join_request(2, &tb.node(2).endpoint(), &hash, true);
        req.rejoin = true;
        admin_client(&tb.node(1).endpoint())
            .await
            .join(req)
            .await
            .unwrap_err()
    });
    assert!(st.message().contains("was removed"), "{st:?}");
    // ... and node 2's own re-join loop (every REJOIN_INTERVAL) does not
    // bring it back: watched for three intervals.
    let until = Instant::now() + graph_server::join::REJOIN_INTERVAL * 3;
    while Instant::now() < until {
        let (v, l) = tb.membership(1);
        assert!(!v.contains(&2) && !l.contains(&2), "node 2 came back");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// QA 2: a LINEARIZABLE read on a follower that cannot apply the leader's
/// latest write never answers stale data: it fails (`NoLeader`, after the
/// read-index wait) until the follower catches up, then sees the write.
#[test]
fn follower_linearizable_read_never_misses_an_acknowledged_write() {
    let _w = watchdog(
        "follower_linearizable_read_never_misses_an_acknowledged_write",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    // `f` gets no more appends (and must not campaign meanwhile).
    tb.node(f)
        .raft()
        .unwrap()
        .raft
        .runtime_config()
        .elect(false);
    tb.drop_append_entries_to(f);
    let (p, b) = small_file(1);
    tb.client(leader)
        .index_bytes("o", "r", &p, &b, None)
        .unwrap();
    let mut cfg = ClientConfig::new(tb.node(f).endpoint());
    cfg.read_mode = ReadMode::Linearizable;
    cfg.retry.budget = Duration::from_millis(500);
    let lin = RemoteStore::connect(cfg).unwrap();
    match lin.count_nodes(NodeKind::File) {
        Ok(n) => assert_eq!(n, 2, "a stale LINEARIZABLE read"),
        Err(e) => assert!(matches!(e, StoreError::NoLeader { .. }), "{e:?}"),
    }
    assert_eq!(
        tb.client(f).count_nodes(NodeKind::File).unwrap(),
        1,
        "the follower really is behind (LOCAL)"
    );
    tb.heal();
    tb.node(f).raft().unwrap().raft.runtime_config().elect(true);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(lin.count_nodes(NodeKind::File).unwrap(), 2);
}

/// QA 5: the real forward-failure path (no fault plan in the way): the
/// leader is gone but the followers still name it (their elections are
/// held), so a follower's forward fails on the transport and the write
/// answers `NoLeader`; once they may elect again, writes work.
#[test]
fn a_forward_to_a_dead_leader_is_no_leader() {
    let _w = watchdog("a_forward_to_a_dead_leader_is_no_leader", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let fs = node_ids_other_than(&tb, &[leader]);
    for f in &fs {
        tb.node(*f)
            .raft()
            .unwrap()
            .raft
            .runtime_config()
            .elect(false);
    }
    tb.node_mut(leader).kill();
    let f = fs[0];
    assert_eq!(
        tb.node(f).raft().unwrap().leader().id,
        Some(leader),
        "the follower still names the dead leader"
    );
    let st = rt().block_on(async {
        write_client(&tb.node(f).endpoint())
            .await
            .index_file(pb::IndexFileRequest {
                org: "o".into(),
                repo: "r".into(),
                file: Some(file_bytes(0)),
                options: None,
            })
            .await
            .unwrap_err()
    });
    assert!(
        matches!(
            graph_proto::status_to_store_error(&st),
            StoreError::NoLeader { .. }
        ),
        "{st:?}"
    );
    assert_eq!(
        tb.client(f).admin_status().unwrap().writes_forwarded_total,
        0
    );
    for f in &fs {
        tb.node(*f)
            .raft()
            .unwrap()
            .raft
            .runtime_config()
            .elect(true);
    }
    let (p, b) = small_file(1);
    tb.client(f).index_bytes("o", "r", &p, &b, None).unwrap();
}

/// Dev review 6 / QA 5: `Prune` and `Vacuum` forwarded twice (what a
/// client retry after an ambiguous forward failure does) are idempotent:
/// the second changes nothing.
#[test]
fn prune_and_vacuum_sent_twice_are_idempotent() {
    let _w = watchdog("prune_and_vacuum_sent_twice_are_idempotent", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    let files: Vec<_> = (0..4).map(small_file).collect();
    index_files(&tb.client(leader), "o", "r", &files);
    let keep: Vec<String> = files[..2].iter().map(|(p, _)| p.clone()).collect();
    let (p1, p2, v1, v2) = rt().block_on(async {
        let mut w = write_client(&tb.node(f).endpoint()).await;
        let prune = pb::PruneRequest {
            org: "o".into(),
            repo: "r".into(),
            keep: keep.clone(),
            dry_run: false,
        };
        let p1 = w.prune(prune.clone()).await.unwrap().into_inner();
        let p2 = w.prune(prune).await.unwrap().into_inner();
        let v1 = w.vacuum(pb::VacuumRequest {}).await.unwrap().into_inner();
        let v2 = w.vacuum(pb::VacuumRequest {}).await.unwrap().into_inner();
        (p1, p2, v1, v2)
    });
    assert!(p1.forwarded_to_leader && p2.forwarded_to_leader);
    assert_eq!(p1.removed.len(), 2, "{p1:?}");
    assert!(
        p2.removed.is_empty(),
        "the second prune removed {:?}",
        p2.removed
    );
    assert!(v1.forwarded_to_leader && v2.forwarded_to_leader);
    let (s1, s2) = (v1.stats.unwrap(), v2.stats.unwrap());
    assert_eq!(
        s2.terms_removed, 0,
        "the second vacuum found more: {s1:?} then {s2:?}"
    );
    assert_eq!(s2.terms_kept, s1.terms_kept, "{s1:?} then {s2:?}");
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let before = summary(&tb.client(leader));
    for id in tb.ids() {
        assert_eq!(summary(&tb.client(id)), before, "node {id}");
        assert_eq!(tb.client(id).count_nodes(NodeKind::File).unwrap(), 2);
    }
}

/// QA 7: the loop guard: a request already forwarded once (it carries
/// `mg-forwarded-by`) is handled locally on a follower, which answers
/// `NotLeader` naming the leader and forwards nothing.
#[test]
fn a_forwarded_request_is_never_forwarded_again() {
    let _w = watchdog("a_forwarded_request_is_never_forwarded_again", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    let st = rt().block_on(async {
        let mut w = write_client(&tb.node(f).endpoint()).await;
        let mut req = tonic::Request::new(pb::IndexFileRequest {
            org: "o".into(),
            repo: "r".into(),
            file: Some(file_bytes(0)),
            options: None,
        });
        req.metadata_mut().insert(
            graph_server::forward::FORWARDED_BY_HEADER,
            "9".parse().unwrap(),
        );
        w.index_file(req).await.unwrap_err()
    });
    match graph_proto::status_to_store_error(&st) {
        StoreError::NotLeader { leader_id, .. } => assert_eq!(leader_id, Some(leader)),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        tb.client(f).admin_status().unwrap().writes_forwarded_total,
        0
    );
    assert_eq!(tb.client(leader).count_nodes(NodeKind::File).unwrap(), 0);
}

/// QA 9, first half: `Join` refuses a joiner whose own request names
/// other extractors before probing it (the address is never asked: it
/// does not even answer).
#[test]
fn join_request_with_other_extractors_is_refused_without_a_probe() {
    let _w = watchdog(
        "join_request_with_other_extractors_is_refused_without_a_probe",
        TEST_LIMIT,
    );
    let tb = ClusterTestbed::new(1, exts());
    wait_until("node 1 to know it leads", || {
        tb.node(1).raft().unwrap().leader().id == Some(1)
    });
    let st = rt().block_on(async {
        admin_client(&tb.node(1).endpoint())
            .await
            .join(graph_server::join::join_request(
                2,
                "127.0.0.1:1",
                "other-extractors",
                false,
            ))
            .await
            .unwrap_err()
    });
    assert!(st.message().contains("extractor"), "{st:?}");
    assert!(tb.membership(1).1.is_empty());
}

/// QA 9, second half: a joiner that claims the cluster's extractors but
/// runs others is caught by the probe (`Status` asked of the node itself).
#[test]
fn join_probe_catches_a_node_running_other_extractors() {
    let _w = watchdog(
        "join_probe_catches_a_node_running_other_extractors",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(1, exts());
    let cfg = tb.node_config(2, InitMode::Uninitialized);
    tb.add_node(2, cfg, rust_only()).unwrap();
    let hash = tb.client(1).admin_status().unwrap().extractors_hash;
    wait_until("node 1 to know it leads", || {
        tb.node(1).raft().unwrap().leader().id == Some(1)
    });
    let st = rt().block_on(async {
        admin_client(&tb.node(1).endpoint())
            .await
            .join(graph_server::join::join_request(
                2,
                &tb.node(2).endpoint(),
                &hash,
                false,
            ))
            .await
            .unwrap_err()
    });
    assert!(
        st.message().contains("runs extractor version set"),
        "{st:?}"
    );
    assert!(tb.membership(1).1.is_empty(), "nothing was added");
}

/// Dev review 10: a join that gets no answer says how to clean up a
/// learner the leader may have added meanwhile.
#[test]
fn a_join_that_times_out_names_the_cleanup() {
    let _w = watchdog("a_join_that_times_out_names_the_cleanup", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(1, exts());
    let mut spec = JoinSpec::new("127.0.0.1:1".to_string());
    spec.timeout = Duration::from_secs(1);
    let cfg = tb.node_config(2, InitMode::Join(spec));
    let e = tb.add_node(2, cfg, exts()).unwrap_err();
    assert!(e.to_string().contains("cluster remove 2"), "{e}");
}

/// QA 4: a transfer to a target that cannot take over (it is down) leaves
/// the cluster undisturbed: the leader keeps leading in the same term (its
/// heartbeats never stopped, so no follower's lease ran out and nobody
/// campaigned), the transfer is refused with the reason, and writes resume.
/// While it runs, a second transfer is refused (`already in progress`) and
/// so is a membership change (`NoLeader`).
#[test]
fn a_failed_transfer_disturbs_nobody() {
    let _w = watchdog("a_failed_transfer_disturbs_nobody", TEST_LIMIT);
    // The transfer holds its slot 3 s before starting: a deterministic
    // window for the concurrent requests below.
    let mut tb = ClusterTestbed::with_config(3, exts(), |_, cfg| {
        cfg.testing.transfer_hold_ms = Some(3000);
    });
    tb.form();
    let leader = tb.leader();
    let target = node_ids_other_than(&tb, &[leader])[0];
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let term = tb.node(leader).raft().unwrap().metrics().current_term;
    tb.node_mut(target).kill();
    let via = node_ids_other_than(&tb, &[leader, target])[0];
    let (old, transfer) = (leader, {
        let c = tb.client(leader);
        std::thread::spawn(move || c.admin_transfer_leader(target))
    });
    // While the transfer holds its slot (the test hook, 3 s), a second
    // transfer and a membership
    // change sent straight to the old leader are refused.
    let flag = Arc::clone(&tb.node(old).raft().unwrap().transferring);
    wait_until("the old leader to start the transfer", || {
        flag.load(Ordering::SeqCst)
    });
    let (second, change) = rt().block_on(async {
        let mut a = admin_client(&tb.node(old).endpoint()).await;
        let second = a
            .transfer_leader(pb::TransferLeaderRequest { to_node_id: via })
            .await
            .unwrap_err();
        let change = a
            .remove(pb::RemoveRequest {
                node_id: via,
                force: true,
            })
            .await
            .unwrap_err();
        (second, change)
    });
    assert!(
        second.message().contains("already in progress"),
        "{second:?}"
    );
    assert!(
        matches!(
            graph_proto::status_to_store_error(&change),
            StoreError::NoLeader { .. }
        ),
        "{change:?}"
    );
    let e = transfer.join().unwrap().unwrap_err();
    assert!(e.to_string().contains("keeps leadership"), "{e}");
    assert_eq!(tb.leader(), leader);
    for id in node_ids_other_than(&tb, &[target]) {
        let m = tb.node(id).raft().unwrap().metrics();
        assert_eq!(m.current_term, term, "node {id}: an election happened");
        assert_eq!(m.current_leader, Some(leader), "node {id}");
    }
    let (p, b) = small_file(1);
    tb.client(leader)
        .index_bytes("o", "r", &p, &b, None)
        .unwrap();
}

/// Dev review (stage C): a transfer waits for a write that passed its check
/// before the transfer's flag went up (the in-flight drain). The test hook
/// holds every write proposal 4 s after it was counted in flight, well past
/// the time a transfer takes without the drain (a lease, about 1.2 s): so
/// the write must finish before the transfer answers, the target must win,
/// and the write must be on the target.
#[test]
fn a_transfer_waits_for_a_write_in_flight() {
    let _w = watchdog("a_transfer_waits_for_a_write_in_flight", TEST_LIMIT);
    const HOLD: Duration = Duration::from_secs(4);
    let mut tb = ClusterTestbed::with_config(3, exts(), |_, cfg| {
        cfg.testing.hold_proposal_ms = Some(HOLD.as_millis() as u64);
    });
    tb.form();
    let old = tb.leader();
    let target = node_ids_other_than(&tb, &[old])[0];
    let in_flight = Arc::clone(&tb.node(old).raft().unwrap().in_flight);
    let writer = {
        let c = tb.client(old);
        std::thread::spawn(move || {
            let (p, b) = small_file(0);
            let r = c.index_bytes("o", "r", &p, &b, None);
            (r, Instant::now())
        })
    };
    wait_until("the write to be in flight on the leader", || {
        in_flight.load(Ordering::SeqCst) > 0
    });
    let t0 = Instant::now();
    let now = tb.client(old).admin_transfer_leader(target);
    let transfer_done = Instant::now();
    let (written, write_done) = writer.join().unwrap();
    written.unwrap_or_else(|e| panic!("the write in flight: {e}"));
    assert_eq!(now.unwrap(), target, "the transfer answered another leader");
    eprintln!("transfer took {:?}", transfer_done - t0);
    assert!(
        write_done <= transfer_done,
        "the transfer answered {:?} before the write in flight finished",
        write_done - transfer_done
    );
    assert_eq!(tb.leader(), target, "the TARGET must lead");
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(
        tb.client(target).count_nodes(NodeKind::File).unwrap(),
        1,
        "the write is on the target"
    );
}

/// #165: `Store.ExtractorGaps` asked of a follower is forwarded to the
/// leader, whose registry parses every write: a follower rebuilt without
/// the Rust extractor answers the leader's (empty) gaps, while the same
/// request marked as already forwarded is answered from the follower's own
/// store and names the gap.
#[test]
fn extractor_gaps_via_follower_are_the_leaders() {
    let _w = watchdog("extractor_gaps_via_follower_are_the_leaders", TEST_LIMIT);
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let index = tb.write_via_leader(|c| {
        let (path, bytes) = small_file(0);
        c.index_bytes("o", "r", &path, &bytes, None).unwrap();
        c.applied_index().load(Ordering::SeqCst)
    });
    tb.wait_applied(index, CLUSTER_WAIT);
    let leader = tb.leader();
    let f = node_ids_other_than(&tb, &[leader])[0];
    tb.node_mut(f).stop();
    tb.node_mut(f).set_extractors(vec![]);
    tb.node_mut(f).restart();
    let leader = tb.wait_leader(CLUSTER_WAIT);
    assert_ne!(leader, f, "the rebuilt node must stay a follower");
    wait_until("the rebuilt follower caught up", || {
        tb.node(f).applied_index() >= index
    });
    assert_eq!(
        tb.client(f).extractor_gaps(Some("o"), Some("r")).unwrap(),
        vec![]
    );
    let local = rt().block_on(async {
        let ch =
            tonic::transport::Endpoint::from_shared(format!("http://{}", tb.node(f).endpoint()))
                .unwrap()
                .connect()
                .await
                .unwrap();
        let mut s = pb::store_client::StoreClient::with_interceptor(ch, graph_proto::SendVersion);
        let mut req = tonic::Request::new(pb::ExtractorGapsRequest {
            org: Some("o".into()),
            repo: Some("r".into()),
        });
        req.metadata_mut().insert(
            graph_server::forward::FORWARDED_BY_HEADER,
            "9".parse().unwrap(),
        );
        s.extractor_gaps(req).await.unwrap().into_inner().gaps
    });
    assert_eq!(local.len(), 1, "{local:?}");
    assert_eq!(local[0].language, "rust");
}

/// Issue #205: a node that leads alone suspends openraft's ticks, so an
/// idle sole voter publishes no metrics (nothing wakes the readiness and
/// snapshot-policy loops); a join resumes them, so the learner keeps
/// hearing heartbeats while idle; removing it suspends them again, and the
/// node still serves writes throughout.
#[test]
fn sole_voter_suspends_ticks_until_the_membership_grows() {
    let _w = watchdog(
        "sole_voter_suspends_ticks_until_the_membership_grows",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(1, exts());
    let suspended = |tb: &ClusterTestbed| tb.node(1).raft().unwrap().ticks.suspended();
    // Idle with ticks off: the metrics stay still for many tick periods.
    let idle_metrics_still = |tb: &ClusterTestbed| {
        let mut rx = tb.node(1).raft().unwrap().raft.metrics();
        // Past any trailing update of the last write or change.
        std::thread::sleep(Duration::from_millis(300));
        rx.borrow_and_update();
        std::thread::sleep(Duration::from_millis(1500));
        !rx.has_changed().unwrap()
    };
    wait_until("node 1, leading alone, suspends its ticks", || {
        suspended(&tb)
    });
    assert!(idle_metrics_still(&tb), "an idle sole voter still ticks");

    let cfg = join_cfg(&tb, 2, false);
    tb.add_node(2, cfg, exts()).unwrap();
    assert!(!suspended(&tb), "ticks stay suspended with a learner");
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    // Idle, the learner still hears from the leader every heartbeat.
    let learner = tb.node(2).raft().unwrap();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        let heard = learner.obs.since_heard_from_leader().unwrap();
        assert!(
            heard < Duration::from_millis(1000),
            "the idle learner has not heard from the leader for {heard:?}"
        );
    }

    tb.client(1).admin_remove(2, true).unwrap();
    wait_until("node 1, alone again, suspends its ticks", || suspended(&tb));
    let files: Vec<_> = (0..3).map(small_file).collect();
    index_files(&tb.client(1), "o", "r", &files);
    assert_eq!(tb.client(1).count_nodes(NodeKind::File).unwrap(), 3);
    assert!(
        idle_metrics_still(&tb),
        "an idle sole voter still ticks after a write"
    );
}

/// Issue #211: a sole leader with its ticks suspended that loses
/// leadership with no membership change (another node's vote request at a
/// higher term, with a longer log) resumes its ticks, campaigns at its next
/// tick and leads again. Kept suspended, it would stay a follower with no
/// election timer, forever.
#[test]
fn a_suspended_sole_leader_deposed_by_a_vote_resumes_ticks_and_leads_again() {
    let _w = watchdog(
        "a_suspended_sole_leader_deposed_by_a_vote_resumes_ticks_and_leads_again",
        TEST_LIMIT,
    );
    let tb = ClusterTestbed::new(1, exts());
    let node = tb.node(1).raft().unwrap().clone();
    wait_until("node 1, leading alone, suspends its ticks", || {
        node.ticks.suspended()
    });
    let m = node.metrics();
    let term = m.current_term;
    let last = m.last_log_index.unwrap_or(0);
    // Node 2 (not a member: openraft does not ask) campaigns at the next
    // term with a longer log. The leader refuses it while its own vote's
    // lease (election_timeout_max) lasts, then grants it.
    let req = VoteRequest::new(
        Vote::new(term + 1, 2),
        Some(LogId::new(CommittedLeaderId::new(term + 1, 2), last + 1)),
    );
    let deadline = Instant::now() + CLUSTER_WAIT;
    rt().block_on(async {
        loop {
            let r = node.raft.vote(req.clone()).await.expect("vote request");
            if r.vote_granted {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "node 1 never granted the vote: {r:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    wait_until("node 1 campaigns and leads again at a later term", || {
        let m = node.metrics();
        m.state == ServerState::Leader && m.current_leader == Some(1) && m.current_term > term + 1
    });
    wait_until("node 1, leading alone again, suspends its ticks", || {
        node.ticks.suspended()
    });
    let files: Vec<_> = (0..2).map(small_file).collect();
    index_files(&tb.client(1), "o", "r", &files);
    assert_eq!(tb.client(1).count_nodes(NodeKind::File).unwrap(), 2);
}

/// Issue #211: a sole voter restarted after a crash mid-campaign (its own
/// vote for a newer term on disk, not committed) starts as a follower and
/// campaigns at once, not at its first tick: the restart returns leading.
/// The timings put that first tick 7.5 s away.
#[test]
fn a_sole_voter_restarted_mid_campaign_leads_at_once() {
    let _w = watchdog(
        "a_sole_voter_restarted_mid_campaign_leads_at_once",
        TEST_LIMIT,
    );
    let slow = RaftSettings {
        heartbeat_ms: 5000,
        election_min_ms: 20_000,
        election_max_ms: 40_000,
        ..TEST_RAFT
    };
    slow.validate(true).unwrap();
    let mut tb = ClusterTestbed::with_config(1, exts(), move |_, c| c.raft = Some(slow));
    let term = tb.node(1).raft().unwrap().metrics().current_term;
    tb.node_mut(1).stop();
    {
        // The stopped node's file can be held for a moment yet (Windows:
        // `DatabaseAlreadyOpen`).
        let path = tb.data_dir(1).join("raft.redb");
        let deadline = Instant::now() + CLUSTER_WAIT;
        let mut log = loop {
            match RedbLogStore::open(&path) {
                Ok(log) => break log,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "opening the stopped node's log: {e}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        let campaign = Vote::new(term + 1, 1);
        assert!(!campaign.is_committed());
        rt().block_on(log.save_vote(&campaign)).unwrap();
    }
    let started = Instant::now();
    tb.node_mut(1).try_restart().unwrap();
    let took = started.elapsed();
    let m = tb.node(1).raft().unwrap().metrics();
    assert_eq!(m.state, ServerState::Leader);
    assert_eq!(m.current_leader, Some(1));
    assert!(
        m.current_term > term + 1,
        "term {} after a campaign at {}",
        m.current_term,
        term + 1
    );
    assert!(
        took < Duration::from_secs(3),
        "the restart took {took:?} to lead (its first tick is 7.5 s away)"
    );
    index_files(&tb.client(1), "o", "r", &[small_file(0)]);
}
