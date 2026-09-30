//! [`ClusterTestbed`]: n in-process nodes on `127.0.0.1:0` with temp data
//! directories (ADR 0004 test plan, the "injected" layer): node 1
//! bootstraps, the others start uninitialized and are added as learners
//! and promoted by [`ClusterTestbed::form`]. Every node runs on its own
//! tokio runtime, so a test thread (outside any runtime) can drive it and
//! use [`RemoteStore`]s, and [`TestNode::kill`] can drop a node's every
//! task at once. A shared [`FaultPlan`] sits in front of every node's
//! network (`partition`, `heal`, `drop_append_entries_to`).
//!
//! Every wait has a hard timeout and panics with what it waited for.
use crate::extractors::share;
use crate::paths::InitMode;
use crate::raft::network::FaultPlan;
use crate::raft::{NodeId, RaftNode, RaftSettings};
use crate::server::{start, Running, ServeConfig};
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_store::StoreError;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The default wait for cluster-level conditions (elections, catch-up).
pub const CLUSTER_WAIT: Duration = Duration::from_secs(20);

/// Raft timings for tests: fast elections, heartbeats long enough for a
/// multi-MiB `AppendEntries` plus an fsync on a slow CI disk.
pub const TEST_RAFT: RaftSettings = RaftSettings {
    heartbeat_ms: 150,
    election_min_ms: 600,
    election_max_ms: 1200,
    ..RaftSettings::cluster()
};

fn runtime(id: NodeId) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(format!("node-{id}"))
        .enable_all()
        .build()
        .expect("node runtime")
}

/// One node of a [`ClusterTestbed`].
pub struct TestNode {
    id: NodeId,
    cfg: ServeConfig,
    extractors: Vec<Arc<dyn Extractor>>,
    addr: SocketAddr,
    rt: Option<tokio::runtime::Runtime>,
    running: Option<Running>,
}

impl TestNode {
    fn launch(&mut self) -> Result<(), StoreError> {
        let rt = runtime(self.id);
        let r = rt.block_on(start(self.cfg.clone(), self.extractors.clone()));
        match r {
            Ok(running) => {
                self.addr = running.addr;
                self.cfg.listen = running.addr;
                self.running = Some(running);
                self.rt = Some(rt);
                Ok(())
            }
            Err(e) => {
                rt.shutdown_timeout(Duration::from_secs(5));
                Err(e)
            }
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// `host:port` of this node (the same across restarts).
    pub fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn data_dir(&self) -> &Path {
        self.cfg
            .data_dir
            .as_deref()
            .expect("testbed nodes use data dirs")
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn running(&self) -> Option<&Running> {
        self.running.as_ref()
    }

    /// The backup uploader (`None` while stopped or without one).
    pub fn backup(&self) -> Option<&crate::backup::Backup> {
        self.running.as_ref().and_then(|r| r.backup.as_ref())
    }

    /// The Raft node (`None` while stopped).
    pub fn raft(&self) -> Option<&RaftNode> {
        self.running.as_ref().map(|r| &r.raft)
    }

    /// The configuration the next [`restart`](Self::restart) uses (set a
    /// failpoint, clear it).
    pub fn config_mut(&mut self) -> &mut ServeConfig {
        &mut self.cfg
    }

    /// The store's `RAFT_SM` marker index (0: none); panics when stopped.
    pub fn applied_index(&self) -> u64 {
        let r = self.running.as_ref().expect("node is running");
        r.slot
            .with_store(|s| s.raft_marker())
            .expect("marker readable")
            .map_or(0, |m| m.index)
    }

    /// Graceful stop: the server drains, the Raft node shuts down, the
    /// store closes, the LOCK sidecar goes.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.shutdown();
            let rt = self.rt.take().expect("a running node has a runtime");
            if let Err(e) = rt.block_on(r.wait()) {
                eprintln!("node {} stop: {e}", self.id);
            }
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }

    /// An abrupt stop, but not a crash: no graceful shutdown (no drain, no
    /// Raft shutdown), and the node's runtime is torn down, which drops
    /// every async task at its next await point; blocking work already
    /// running (an apply, a log append on the blocking pool) finishes, and
    /// redb then closes the store and the log cleanly. What a crash loses
    /// on top of that (writes not yet synced) is what the power-cut tests
    /// cover ([`crate::powercut::PowerCutDisk`], `tests/durability.rs`),
    /// and `tests/process_kill.rs` kills a real server process.
    pub fn kill(&mut self) {
        if let Some(r) = self.running.take() {
            drop(r);
            if let Some(rt) = self.rt.take() {
                rt.shutdown_timeout(Duration::from_secs(5));
            }
        }
    }

    /// Start again on the same data directory and port. Retries only the
    /// errors a just-stopped node can cause (the port or the files still
    /// held for a moment), within [`CLUSTER_WAIT`].
    pub fn restart(&mut self) {
        if let Err(e) = self.try_restart() {
            panic!("node {} restart: {e}", self.id);
        }
    }

    /// [`restart`](Self::restart), returning a start-up refusal instead
    /// of panicking.
    pub fn try_restart(&mut self) -> Result<(), StoreError> {
        assert!(self.running.is_none(), "node {} is running", self.id);
        let deadline = Instant::now() + CLUSTER_WAIT;
        loop {
            match self.launch() {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let transient = matches!(e, StoreError::Locked(_))
                        || e.to_string().contains("cannot listen");
                    if !transient || Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// The extractors the next [`restart`](Self::restart) uses (a node
    /// rebuilt with another extractor version set).
    pub fn set_extractors(&mut self, extractors: Vec<Box<dyn Extractor>>) {
        self.extractors = share(extractors);
    }

    /// Wait until this node's Raft stopped with a fatal error (a failpoint
    /// fired).
    pub fn wait_fatal(&self, timeout: Duration) {
        let raft = self.raft().expect("node is running").raft.clone();
        let rt = self.rt.as_ref().expect("runtime");
        rt.block_on(async move {
            let mut rx = raft.metrics();
            let waited = tokio::time::timeout(timeout, async {
                loop {
                    if rx.borrow().running_state.is_err() {
                        return;
                    }
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            })
            .await;
            assert!(
                waited.is_ok(),
                "node's raft did not stop within {timeout:?}"
            );
        });
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.stop();
    }
}

/// n nodes of one cluster, in process.
pub struct ClusterTestbed {
    nodes: BTreeMap<NodeId, TestNode>,
    plan: FaultPlan,
    // Last: the directories outlive the nodes.
    _root: tempfile::TempDir,
}

impl ClusterTestbed {
    /// Start `n` nodes (node 1 bootstraps, the rest wait uninitialized)
    /// with [`TEST_RAFT`] timings. Call [`form`](Self::form) to make them
    /// one cluster.
    pub fn new(n: usize, extractors: Vec<Box<dyn Extractor>>) -> Self {
        Self::with_config(n, extractors, |_, _| {})
    }

    /// [`new`](Self::new) with every node backing its snapshots up per
    /// `backup` (ADR 0006).
    pub fn with_backup(
        n: usize,
        extractors: Vec<Box<dyn Extractor>>,
        backup: crate::backup::BackupConfig,
    ) -> Self {
        Self::with_config(n, extractors, |_, c| c.backup = Some(backup.clone()))
    }

    /// Wait until no running node has a backup queued or uploading.
    pub fn wait_backups_idle(&self, timeout: Duration) {
        for n in self.live() {
            if let Some(b) = n.backup() {
                assert!(
                    b.wait_idle(timeout),
                    "node {}: backups still busy after {timeout:?}",
                    n.id
                );
            }
        }
    }

    /// [`new`](Self::new) with a hook to adjust each node's configuration
    /// before its first start (snapshot knobs, failpoints, disk probe).
    pub fn with_config(
        n: usize,
        extractors: Vec<Box<dyn Extractor>>,
        tweak: impl Fn(NodeId, &mut ServeConfig),
    ) -> Self {
        assert!(n >= 1);
        let root = tempfile::tempdir().expect("testbed temp dir");
        let extractors = share(extractors);
        let plan = FaultPlan::new();
        let mut nodes = BTreeMap::new();
        for i in 1..=n as NodeId {
            let init = if i == 1 {
                InitMode::Bootstrap { restore: None }
            } else {
                InitMode::Uninitialized
            };
            let mut cfg = ServeConfig::for_data_dir(
                root.path().join(format!("node{i}")),
                "127.0.0.1:0".parse().unwrap(),
                init,
                Some(i),
            );
            cfg.shutdown_grace = Duration::from_secs(5);
            cfg.raft = Some(TEST_RAFT);
            cfg.fault_plan = Some(plan.clone());
            tweak(i, &mut cfg);
            let mut node = TestNode {
                id: i,
                cfg,
                extractors: extractors.clone(),
                addr: "127.0.0.1:0".parse().unwrap(),
                rt: None,
                running: None,
            };
            node.launch()
                .unwrap_or_else(|e| panic!("node {i} starts: {e}"));
            nodes.insert(i, node);
        }
        Self {
            nodes,
            plan,
            _root: root,
        }
    }

    pub fn ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    /// Where the nodes' data directories live (`node<id>` under it).
    pub fn root(&self) -> &Path {
        self._root.path()
    }

    /// The configuration a new node `id` gets: a data directory
    /// `node<id>` under [`root`](Self::root), `init`, [`TEST_RAFT`] and
    /// the shared fault plan.
    pub fn node_config(&self, id: NodeId, init: InitMode) -> ServeConfig {
        let mut cfg = ServeConfig::for_data_dir(
            self.root().join(format!("node{id}")),
            "127.0.0.1:0".parse().unwrap(),
            init,
            Some(id),
        );
        cfg.shutdown_grace = Duration::from_secs(5);
        cfg.raft = Some(TEST_RAFT);
        cfg.fault_plan = Some(self.plan.clone());
        cfg
    }

    /// Start one more node with `cfg` (see [`node_config`](Self::node_config))
    /// and these extractors; on success it is part of the testbed. A
    /// start-up refusal (a `--join` the leader refused, `WrongCluster`) is
    /// returned and the node is not added.
    pub fn add_node(
        &mut self,
        id: NodeId,
        cfg: ServeConfig,
        extractors: Vec<Box<dyn Extractor>>,
    ) -> Result<(), StoreError> {
        assert!(!self.nodes.contains_key(&id), "node {id} exists");
        let mut node = TestNode {
            id,
            cfg,
            extractors: share(extractors),
            addr: "127.0.0.1:0".parse().unwrap(),
            rt: None,
            running: None,
        };
        node.launch()?;
        self.nodes.insert(id, node);
        Ok(())
    }

    /// The `(voters, learners)` node `id` sees in its membership.
    pub fn membership(&self, id: NodeId) -> (BTreeSet<NodeId>, BTreeSet<NodeId>) {
        let m = self.node(id).raft().expect("node is running").metrics();
        let mem = m.membership_config.membership();
        let voters: BTreeSet<NodeId> = mem.voter_ids().collect();
        let learners = mem
            .nodes()
            .map(|(id, _)| *id)
            .filter(|id| !voters.contains(id))
            .collect();
        (voters, learners)
    }

    /// Wait until the current leader sees exactly `voters` as its voters.
    pub fn wait_voters(&self, voters: &[NodeId], timeout: Duration) {
        let want: BTreeSet<NodeId> = voters.iter().copied().collect();
        let deadline = Instant::now() + timeout;
        loop {
            let l = self.wait_leader(timeout);
            let (v, _) = self.membership(l);
            if v == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the voters are {v:?}, not {want:?}, after {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn node(&self, id: NodeId) -> &TestNode {
        self.nodes.get(&id).expect("no such node")
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut TestNode {
        self.nodes.get_mut(&id).expect("no such node")
    }

    /// The shared fault plan.
    pub fn faults(&self) -> &FaultPlan {
        &self.plan
    }

    pub fn partition(&self, a: &[NodeId], b: &[NodeId]) {
        self.plan.partition(a, b);
    }

    pub fn heal(&self) {
        self.plan.heal();
    }

    pub fn drop_append_entries_to(&self, id: NodeId) {
        self.plan.drop_append_entries_to(id);
    }

    fn live(&self) -> Vec<&TestNode> {
        self.nodes.values().filter(|n| n.is_running()).collect()
    }

    /// Add every other node as a learner (waiting until it caught up) and
    /// promote each to voter, through node 1; then wait until every node
    /// sees all of them as voters.
    pub fn form(&mut self) {
        let leader = self.leader();
        let c = self.client(leader);
        let others: Vec<(NodeId, String)> = self
            .nodes
            .values()
            .filter(|n| n.id != leader)
            .map(|n| (n.id, n.endpoint()))
            .collect();
        for (id, addr) in &others {
            c.admin_add_learner(*id, addr, true)
                .unwrap_or_else(|e| panic!("add learner {id}: {e}"));
        }
        for (id, _) in &others {
            c.admin_promote(*id)
                .unwrap_or_else(|e| panic!("promote {id}: {e}"));
        }
        let all: BTreeSet<NodeId> = self.nodes.keys().copied().collect();
        for n in self.live() {
            let raft = n.raft().unwrap().raft.clone();
            let all = all.clone();
            n.rt.as_ref().unwrap().block_on(async move {
                raft.wait(Some(CLUSTER_WAIT))
                    .metrics(
                        move |m| {
                            m.membership_config
                                .membership()
                                .voter_ids()
                                .collect::<BTreeSet<_>>()
                                == all
                        },
                        "every node is a voter",
                    )
                    .await
                    .unwrap_or_else(|e| panic!("forming the cluster: {e}"));
            });
        }
    }

    /// The current leader among the running nodes: one that is in the
    /// `Leader` state (the highest term wins, should a deposed one not
    /// know yet). Waits up to [`CLUSTER_WAIT`].
    pub fn leader(&self) -> NodeId {
        self.wait_leader(CLUSTER_WAIT)
    }

    pub fn wait_leader(&self, timeout: Duration) -> NodeId {
        let deadline = Instant::now() + timeout;
        loop {
            let best = self
                .live()
                .into_iter()
                .filter_map(|n| {
                    let m = n.raft()?.metrics();
                    (m.state == openraft::ServerState::Leader && m.current_leader == Some(n.id))
                        .then_some((m.current_term, n.id))
                })
                .max();
            if let Some((_, id)) = best {
                return id;
            }
            assert!(
                Instant::now() < deadline,
                "no leader among the running nodes within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until every running node has applied (store marker) at least
    /// `index`.
    pub fn wait_applied(&self, index: u64, timeout: Duration) {
        for n in self.live() {
            let raft = n.raft().unwrap().raft.clone();
            let id = n.id;
            n.rt.as_ref().unwrap().block_on(async move {
                raft.wait(Some(timeout))
                    .applied_index_at_least(Some(index), "applied")
                    .await
                    .unwrap_or_else(|e| panic!("node {id} did not apply {index}: {e}"));
            });
            assert!(
                n.applied_index() >= index,
                "node {id}: openraft applied {index} but the store marker is {}",
                n.applied_index()
            );
        }
    }

    /// The leader's last log index (what `wait_applied` should reach after
    /// a write through it).
    pub fn leader_last_log_index(&self) -> u64 {
        let l = self.leader();
        self.node(l)
            .raft()
            .unwrap()
            .metrics()
            .last_log_index
            .unwrap_or(0)
    }

    /// A client of node `id` only (short write deadline). Build it outside
    /// any runtime (the test thread).
    pub fn client(&self, id: NodeId) -> RemoteStore {
        let mut cfg = ClientConfig::new(self.node(id).endpoint());
        cfg.write_deadline = Duration::from_secs(15);
        RemoteStore::connect(cfg).unwrap_or_else(|e| panic!("connect to node {id}: {e}"))
    }

    /// Run `f` with a client of the current leader.
    pub fn write_via_leader<T>(&self, f: impl FnOnce(&RemoteStore) -> T) -> T {
        let c = self.client(self.leader());
        f(&c)
    }

    /// The data directory of `id` (also while stopped).
    pub fn data_dir(&self, id: NodeId) -> PathBuf {
        self.node(id).data_dir().to_path_buf()
    }
}
