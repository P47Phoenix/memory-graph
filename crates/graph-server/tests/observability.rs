//! Stage E (ADR 0004 D10, epic story 24): readiness follows the lag, and
//! `Admin.Metrics` answers the Prometheus document.
//!
//! Deterministic: the learners' apply is held back by a test gate
//! (`ServeConfig::testing_apply_gate`) while they keep receiving the leader's
//! entries and its committed index, so their applied index stays put as the
//! leader's committed index moves on; only `ready_max_lag` differs between
//! them.
mod support;

use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT};
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

    fn as_testing_apply_gate(&self) -> graph_server::raft::state_machine::TestingApplyGate {
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
        cfg.testing_apply_gate = Some(gate.as_testing_apply_gate());
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
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    // A write through a follower: it forwards to the leader.
    let fwd = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    index_files(&tb.client(fwd), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let text = tb.client(leader).admin_metrics().unwrap();
    // The leader: one lag sample per peer, equal to what Admin.Status says
    // (idle and caught up: zero).
    let st = tb.client(leader).admin_status().unwrap();
    let peers: Vec<u64> = tb.ids().into_iter().filter(|i| *i != leader).collect();
    for p in &peers {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("mg_raft_replication_lag{{peer=\"{p}\"}} ")))
            .unwrap_or_else(|| panic!("no lag sample for peer {p}:\n{text}"));
        let lag: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        let status_lag = st
            .replication
            .iter()
            .find(|r| r.node_id == *p)
            .unwrap_or_else(|| panic!("status has no replication entry for {p}: {st:?}"))
            .lag;
        assert_eq!(lag, status_lag, "peer {p}");
        assert_eq!(lag, 0, "caught up");
    }
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("mg_raft_replication_lag{"))
            .count(),
        peers.len(),
        "{text}"
    );
    // The follower that forwarded counts it.
    let f = tb.client(fwd).admin_metrics().unwrap();
    let forwarded: u64 = f
        .lines()
        .find_map(|l| l.strip_prefix("mg_writes_forwarded_total "))
        .expect("mg_writes_forwarded_total sample")
        .parse()
        .unwrap();
    assert!(forwarded >= 1, "{f}");
    assert_eq!(
        tb.client(fwd)
            .admin_status()
            .unwrap()
            .writes_forwarded_total,
        forwarded
    );
    for name in graph_server::observe::METRIC_NAMES {
        assert!(
            text.contains(&format!("# TYPE {name} ")),
            "{name} missing:\n{text}"
        );
    }
    let other = fwd;
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

#[test]
fn an_idle_caught_up_learner_stays_ready_and_a_partitioned_one_does_not() {
    let _w = watchdog(
        "an_idle_caught_up_learner_stays_ready_and_a_partitioned_one_does_not",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    index_files(&tb.client(tb.leader()), "o", "r", &[small_file(0)]);
    let spec = JoinSpec::new(tb.node(1).endpoint());
    let cfg = tb.node_config(4, InitMode::Join(spec));
    tb.add_node(4, cfg, exts()).unwrap();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let (_, learners) = tb.membership(tb.leader());
    assert!(learners.contains(&4), "node 4 is a learner");
    for id in [1, 2, 3, 4] {
        wait_until(&format!("node {id} to be ready"), || {
            tb.client(id).health(READY_SERVICE).unwrap()
        });
    }
    // Idle (no writes) for twice the silence limit: the leader's
    // heartbeats reach the learner too, so every node stays ready.
    let limit = graph_server::observe::leader_silence_limit(
        graph_server::testing::TEST_RAFT.election_max_ms,
    );
    let clients: Vec<_> = [1, 2, 3, 4].map(|id| (id, tb.client(id))).into();
    let until = Instant::now() + limit * 2;
    while Instant::now() < until {
        for (id, c) in &clients {
            assert!(
                c.health(READY_SERVICE).unwrap(),
                "node {id} went NOT_SERVING while idle and caught up"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The learner is cut off from the leader: it keeps its last known
    // leader (a learner never campaigns) but is not ready once the leader
    // has been silent past the limit.
    tb.partition(&[4], &[1, 2, 3]);
    let t = Instant::now();
    wait_until("the partitioned learner to report NOT_SERVING", || {
        !clients[3].1.health(READY_SERVICE).unwrap()
    });
    assert!(
        t.elapsed() + Duration::from_millis(500) >= limit,
        "not before the limit: {:?} < {limit:?}",
        t.elapsed()
    );
    assert!(clients[3].1.health("").unwrap(), "alive, only not ready");
    assert!(
        tb.node(4)
            .raft()
            .unwrap()
            .metrics()
            .current_leader
            .is_some(),
        "it still names a leader"
    );
    for (id, c) in &clients[..3] {
        assert!(c.health(READY_SERVICE).unwrap(), "node {id} stays ready");
    }
    tb.heal();
    wait_until("the learner to be ready again once healed", || {
        clients[3].1.health(READY_SERVICE).unwrap()
    });
}

#[test]
fn a_survivor_without_quorum_is_live_but_not_ready() {
    let _w = watchdog(
        "a_survivor_without_quorum_is_live_but_not_ready",
        TEST_LIMIT,
    );
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.leader();
    index_files(&tb.client(leader), "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    // Stop two of three, the leader among them.
    let survivor = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    for id in tb.ids() {
        if id != survivor {
            tb.node_mut(id).stop();
        }
    }
    let c = tb.client(survivor);
    wait_until(
        "the survivor to report NOT_SERVING on memory-graph.ready",
        || !c.health(READY_SERVICE).unwrap(),
    );
    for _ in 0..10 {
        assert!(c.health("").unwrap(), "the survivor is live");
        assert!(!c.health(READY_SERVICE).unwrap(), "and not ready");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Issue #211: `memory-graph.ready` is reported on a change only. A
/// `grpc.health.v1` Watch on an idle `--db` server gets SERVING and then
/// nothing more, even after a write moves the Raft metrics the readiness
/// loop wakes on (tonic-health tells every watcher about each update,
/// changed or not, so a re-report would reach it).
#[test]
fn a_ready_watch_on_an_idle_server_hears_serving_once() {
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;
    use tonic_health::pb::HealthCheckRequest;
    let _w = watchdog(
        "a_ready_watch_on_an_idle_server_hears_serving_once",
        TEST_LIMIT,
    );
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(&dir.path().join("g.redb"), exts());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = server.endpoint();
    let mut stream = rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{endpoint}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut h = HealthClient::new(ch);
        let deadline = Instant::now() + CLUSTER_WAIT;
        // The name is registered by the readiness loop's first report.
        let mut s = loop {
            match h
                .watch(HealthCheckRequest {
                    service: READY_SERVICE.into(),
                })
                .await
            {
                Ok(r) => break r.into_inner(),
                Err(e) => {
                    assert!(Instant::now() < deadline, "no readiness to watch: {e}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        loop {
            let m = tokio::time::timeout(CLUSTER_WAIT, s.message())
                .await
                .expect("a readiness status")
                .expect("the watch stream")
                .expect("an open stream");
            if m.status == ServingStatus::Serving as i32 {
                break s;
            }
        }
    });
    // The readiness loop wakes on the write's metrics; readiness stays.
    let c = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(endpoint)).unwrap();
    index_files(&c, "o", "r", &[small_file(0)]);
    let more =
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(3), stream.message()).await });
    assert!(
        more.is_err(),
        "a readiness update with nothing changed: {more:?}"
    );
}

fn scrape(addr: std::net::SocketAddr) -> std::io::Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

#[test]
fn idle_connections_do_not_block_a_metrics_scrape() {
    use std::io::Read;
    let _w = watchdog("idle_connections_do_not_block_a_metrics_scrape", TEST_LIMIT);
    let tb = ClusterTestbed::with_config(1, exts(), |_, cfg| {
        cfg.metrics_listen = Some("127.0.0.1:0".parse().unwrap());
    });
    let addr = tb.node(1).running().unwrap().metrics_addr.unwrap();
    assert!(scrape(addr).unwrap().starts_with("HTTP/1.1 200 OK"));
    // Far more idle connections than the cap: they send nothing.
    let cap = graph_server::observe::MAX_CONNECTIONS;
    let idle: Vec<std::net::TcpStream> = (0..cap * 3)
        .map(|_| std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap())
        .collect();
    // Those over the cap are closed at once (EOF), not parked.
    let mut closed = 0;
    for s in &idle[cap..] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut b = [0u8; 1];
        if matches!((&*s).read(&mut b), Ok(0) | Err(_)) {
            closed += 1;
        }
    }
    assert!(
        closed >= cap,
        "{closed} of {} over the cap were closed",
        cap * 2
    );
    // A scrape succeeds, at the latest once the idle ones time out.
    let deadline = Instant::now() + graph_server::observe::READ_TIMEOUT * 4;
    loop {
        match scrape(addr) {
            Ok(r) if r.starts_with("HTTP/1.1 200 OK") => break,
            r => assert!(
                Instant::now() < deadline,
                "no scrape succeeded within {:?}: {r:?}",
                graph_server::observe::READ_TIMEOUT * 4
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(idle);
}
