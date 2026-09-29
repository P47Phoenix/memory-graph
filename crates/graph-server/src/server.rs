//! Start-up and shutdown (ADR 0004 D4/D6/D10): resolve the node's files
//! ([`NodePaths`]: `--data-dir` or `--db`) and identity (`node.json`,
//! [`InitMode`]), open the store, bind, write the LOCK sidecar, start the
//! Raft node, serve until told to stop. Shutdown
//! order: health `""` and `memory-graph.ready` go `NOT_SERVING` at once; stop
//! accepting and finish in-flight requests (bounded by the grace period);
//! shut the Raft node down (bounded by the grace period too, logged on
//! expiry); close the store; remove the sidecar last.
use crate::conn::ConnIo;
use crate::disk::{system_probe, DiskGuard, FreeSpaceProbe};
use crate::extractors::{extractors_hash, share};
use crate::lock::LockFile;
use crate::paths::{self, ClusterIdentity, InitMode, NodeJson, NodePaths};
use crate::raft::log_store::{AppendObserver, RedbLogStore};
use crate::raft::network::FaultPlan;
use crate::raft::node::NodeStart;
use crate::raft::snapshot_dir::SnapshotDir;
use crate::raft::state_machine::SmFailpoints;
use crate::raft::{RaftNode, RaftSettings};
use crate::services::admin::AdminService;
use crate::services::raft::RaftService;
use crate::services::store::StoreService;
use crate::services::write::WriteService;
use crate::services::{Ctx, ServerInfo};
use crate::slot::StoreSlot;
use crate::snapshots::SnapshotTable;
use crate::READY_SERVICE;
use graph_core::Extractor;
use graph_proto::pb::admin_server::AdminServer;
use graph_proto::pb::raft_server::RaftServer;
use graph_proto::pb::store_server::StoreServer;
use graph_proto::pb::write_server::WriteServer;
use graph_proto::CheckVersion;
use graph_store::StoreError;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tonic::service::interceptor::InterceptedService;
use tonic_health::ServingStatus;

/// Produces the `sysinfo --json` document for `Admin.SysInfo`, given the
/// store path. The CLI passes its own report; without one the server
/// answers a minimal document.
pub type SysInfoFn = Arc<dyn Fn(&Path) -> serde_json::Value + Send + Sync>;

/// How a server is configured (the `serve` flags).
#[derive(Clone)]
pub struct ServeConfig {
    /// `--db` mode (stage A): the store file; `<db>.raft.redb`, `<db>.LOCK`
    /// and `<db>.snapshots/` go next to it. Empty in `--data-dir` mode
    /// (both set is refused).
    pub db: PathBuf,
    /// `--data-dir` mode (ADR 0004 D6): `node.json`, `graph.redb`,
    /// `raft.redb`, `snapshots/`, `LOCK` in this directory.
    pub data_dir: Option<PathBuf>,
    /// What a `--data-dir` node does at start-up (ignored in `--db` mode).
    pub init: InitMode,
    /// Where to listen; port 0 picks a free one (see [`Running::addr`]).
    pub listen: SocketAddr,
    /// This node's Raft id. `--db` mode: default 1. `--data-dir` mode:
    /// required on the first start, then read from `node.json` (a
    /// different value is refused).
    pub node_id: Option<u64>,
    /// `host:port` peers and clients reach this node at; default the bound
    /// address with a wildcard IP replaced by the host name. Stored in
    /// `node.json` (a different value on restart is refused).
    pub advertise: Option<String>,
    /// Raft timing and log retention; `None` picks
    /// [`RaftSettings::cluster`] (`--data-dir`) or
    /// [`RaftSettings::standalone`] (`--db`).
    pub raft: Option<RaftSettings>,
    /// The disk guard (`--min-free-disk`): writes and snapshot builds are
    /// refused (`RESOURCE_EXHAUSTED`) while the volume has less than this
    /// plus one snapshot copy free. 0 (the library default) turns it off;
    /// the CLI passes its own default.
    pub min_free_disk: u64,
    /// The free-space probe (tests inject a fake one); `None`: the system's.
    pub free_space_probe: Option<FreeSpaceProbe>,
    /// Test-only: the fault plan every node of a testbed consults before
    /// sending a Raft RPC.
    pub fault_plan: Option<FaultPlan>,
    /// Test-only: observe the log store's commits and flush callbacks.
    pub append_observer: Option<AppendObserver>,
    /// redb cache size; `None` keeps redb's default.
    pub cache_bytes: Option<usize>,
    /// Snapshot handle (and store snapshot) max age; default 15 minutes.
    pub snapshot_max_age: Duration,
    /// How long in-flight requests get to finish on shutdown; default 30 s.
    pub shutdown_grace: Duration,
    /// The `Admin.SysInfo` provider.
    pub sysinfo: Option<SysInfoFn>,
    /// Fault injection for tests of the client and CLI (never set by
    /// `serve`).
    pub testing: TestingHooks,
    /// Test-only: open the store and the log over these redb storage
    /// backends instead of files (the power-cut tests). Snapshots, installs
    /// and `compact` still use files and are not supported with it.
    pub storage_backend: Option<crate::powercut::BackendFactory>,
    /// Test-only: a snapshot install waits at this gate (holding the
    /// store's install state) until the test opens it.
    pub install_gate: Option<crate::slot::InstallGate>,
    /// `--metrics-listen`: serve Prometheus text at `/metrics` here (port
    /// 0 picks a free one, see [`Running::metrics_addr`]); `None`: off.
    pub metrics_listen: Option<SocketAddr>,
    /// `--ready-max-lag`: `memory-graph.ready` is `SERVING` only while this
    /// node's applied index is within this many entries of the leader's
    /// committed index (default [`crate::DEFAULT_READY_MAX_LAG`]).
    pub ready_max_lag: u64,
    /// Testing only, not a supported API (hidden from the docs, like the
    /// failpoints): hold apply back (see
    /// [`crate::raft::state_machine::TestingApplyGate`]).
    #[doc(hidden)]
    pub testing_apply_gate: Option<crate::raft::state_machine::TestingApplyGate>,
}

/// Test-only behaviour a [`ServeConfig`] can ask for, so the CLI's exit
/// codes 3/4/5 can be driven end to end against a real server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestingHooks {
    /// Act as if no leader were known: writes and linearizable reads answer
    /// `NoLeader`, `Status`/`Hello`/`Leader` report none, `memory-graph.ready`
    /// is `NOT_SERVING`.
    pub withhold_leader: bool,
    /// Answer `Hello` with this protocol version instead of the real one.
    pub hello_protocol_version: Option<u32>,
    /// Let this many write proposals through, then park every later one
    /// forever (a future that never resolves), so a run cannot finish and a
    /// test can kill the server mid-write deterministically. The first
    /// parked proposal prints `memory-graph serve: testing: writes stalled
    /// after N` on stdout. `serve` sets it only from the test-only
    /// `MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER` environment variable.
    pub stall_writes_after: Option<usize>,
    /// Failpoint: `apply` fails (an I/O error, so openraft stops the node)
    /// just before the entry at this log index, which is then in the log
    /// and committed but not applied.
    pub fail_before_apply: Option<u64>,
    /// Failpoint: the store transaction applying the entry at this log
    /// index fails just before its commit (data and marker staged, then
    /// rolled back), and openraft stops the node.
    pub fail_in_apply_txn: Option<u64>,
    /// Failpoint: a first start fails right after writing `node.json`,
    /// before the Raft node is initialized (a crash in that window).
    pub fail_after_node_json: bool,
    /// Hold every received `AppendEntries` that carries entries for this
    /// long before handing it to Raft (a slow link or disk, longer than
    /// the leader's heartbeat timeout). Heartbeats are not delayed.
    pub delay_append_entries_ms: Option<u64>,
    /// The default deadline of a request this node forwards to the leader
    /// (when the client sent none) instead of `forward::FORWARD_*_TIMEOUT`.
    pub forward_timeout_ms: Option<u64>,
    /// A `TransferLeader` holds its slot (writes refused, heartbeats on)
    /// this long before it starts, so a test can act while one runs.
    pub transfer_hold_ms: Option<u64>,
    /// Every write proposal, once counted as in flight, waits this long
    /// before it reaches Raft, so a test can start a `TransferLeader`
    /// while a write is in flight (the transfer's drain).
    pub hold_proposal_ms: Option<u64>,
}

impl ServeConfig {
    /// `--db` mode (stage A).
    pub fn new(db: impl Into<PathBuf>, listen: SocketAddr) -> Self {
        Self {
            db: db.into(),
            data_dir: None,
            init: InitMode::Restart,
            listen,
            node_id: None,
            advertise: None,
            raft: None,
            min_free_disk: 0,
            free_space_probe: None,
            fault_plan: None,
            append_observer: None,
            cache_bytes: None,
            snapshot_max_age: Duration::from_secs(15 * 60),
            shutdown_grace: Duration::from_secs(30),
            sysinfo: None,
            testing: TestingHooks::default(),
            storage_backend: None,
            install_gate: None,
            metrics_listen: None,
            ready_max_lag: crate::DEFAULT_READY_MAX_LAG,
            testing_apply_gate: None,
        }
    }

    /// `--data-dir` mode (stage B) with `init` and `node_id`.
    pub fn for_data_dir(
        dir: impl Into<PathBuf>,
        listen: SocketAddr,
        init: InitMode,
        node_id: Option<u64>,
    ) -> Self {
        let mut c = Self::new(PathBuf::new(), listen);
        c.data_dir = Some(dir.into());
        c.init = init;
        c.node_id = node_id;
        c
    }
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("db", &self.db)
            .field("data_dir", &self.data_dir)
            .field("init", &self.init)
            .field("listen", &self.listen)
            .field("node_id", &self.node_id)
            .field("advertise", &self.advertise)
            .field("raft", &self.raft)
            .field("min_free_disk", &self.min_free_disk)
            .field("cache_bytes", &self.cache_bytes)
            .field("snapshot_max_age", &self.snapshot_max_age)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("sysinfo", &self.sysinfo.is_some())
            .field("testing", &self.testing)
            .field("metrics_listen", &self.metrics_listen)
            .field("ready_max_lag", &self.ready_max_lag)
            .finish()
    }
}

/// Asks a running server to stop; cloneable, usable from any task.
#[derive(Clone, Default)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        loop {
            if self.is_triggered() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }
}

/// A started server.
pub struct Running {
    /// The address actually bound (a port-0 config gets the real port).
    pub addr: SocketAddr,
    /// Where `/metrics` is served (`--metrics-listen`), as bound.
    pub metrics_addr: Option<SocketAddr>,
    shutdown: ShutdownHandle,
    task: tokio::task::JoinHandle<Result<(), StoreError>>,
    pub slot: Arc<StoreSlot>,
    pub raft: RaftNode,
    /// Where this node keeps its files.
    pub paths: NodePaths,
    /// The cluster id as this node knows it.
    pub identity: Arc<ClusterIdentity>,
}

impl Running {
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Ask it to stop (returns at once; [`wait`](Self::wait) for the end).
    pub fn shutdown(&self) {
        self.shutdown.trigger();
    }

    /// Wait until the server has stopped and everything is closed.
    pub async fn wait(self) -> Result<(), StoreError> {
        self.task
            .await
            .map_err(|e| StoreError::Storage(format!("server task: {e}")))?
    }
}

fn io_err(what: &str, e: std::io::Error) -> StoreError {
    StoreError::Storage(format!("{what}: {e}"))
}

/// Resolve the node's files and start plan; refuses every inconsistent
/// combination before anything is written.
fn resolve(cfg: &ServeConfig) -> Result<(NodePaths, Option<paths::StartPlan>), StoreError> {
    match &cfg.data_dir {
        Some(dir) => {
            if !cfg.db.as_os_str().is_empty() {
                return Err(StoreError::Rejected(
                    "--data-dir and --db cannot be used together".into(),
                ));
            }
            let p = NodePaths::for_data_dir(dir);
            let plan = paths::plan(&p, &cfg.init, cfg.node_id)?;
            Ok((p, Some(plan)))
        }
        None => Ok((NodePaths::for_db(&cfg.db), None)),
    }
}

/// Start a server on the current tokio runtime.
pub async fn start(
    mut cfg: ServeConfig,
    extractors: Vec<Arc<dyn Extractor>>,
) -> Result<Running, StoreError> {
    // `--bootstrap-or-join` becomes a bootstrap or a join first (ordinal
    // 0 on an empty directory asks its siblings; a lost volume must not
    // create a second cluster).
    if let (InitMode::BootstrapOrJoin(spec), Some(dir)) = (&cfg.init, &cfg.data_dir) {
        cfg.init = crate::join::resolve_bootstrap_or_join(
            spec,
            dir,
            cfg.node_id,
            cfg.advertise.as_deref(),
        )
        .await?;
    }
    let hash = extractors_hash(&extractors);
    let (paths, plan) = resolve(&cfg)?;
    let node_id = match &plan {
        Some(p) => p.node_id,
        None => cfg.node_id.unwrap_or(1),
    };
    // `--join` on a directory that already belongs to a cluster is a
    // restart, but not into another cluster: refused before anything is
    // opened (ADR 0004 D6).
    if let (InitMode::Join(spec), Some(p)) = (&cfg.init, &plan) {
        if let Some(mine) = p.existing.as_ref().and_then(|j| j.cluster_id.as_deref()) {
            crate::join::check_peer_cluster(&spec.peer, mine).await?;
        }
    }
    // Bind before anything is written: a port in use must not leave a store
    // behind (the next start would find a store and no node.json). A store
    // held by another server is still refused below, by redb's lock.
    let listener = TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| io_err(&format!("cannot listen on {}", cfg.listen), e))?;
    let addr = listener
        .local_addr()
        .map_err(|e| io_err("local address", e))?;
    let metrics_listener = match cfg.metrics_listen {
        Some(m) => Some(
            TcpListener::bind(m)
                .await
                .map_err(|e| io_err(&format!("cannot listen on {m} (--metrics-listen)"), e))?,
        ),
        None => None,
    };
    let metrics_addr = match &metrics_listener {
        Some(l) => Some(
            l.local_addr()
                .map_err(|e| io_err("metrics local address", e))?,
        ),
        None => None,
    };
    if let Some(dir) = &paths.data_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| io_err(&format!("creating `{}`", dir.display()), e))?;
    }
    if let Some(snap) = plan.as_ref().and_then(|p| p.restore.as_deref()) {
        paths::restore_into(snap, &paths.store)?;
    }
    if plan.as_ref().is_some_and(|p| p.overwrite) {
        let to = paths::move_aside(&paths)?;
        tracing::warn!(
            data_dir = %paths.data_dir.as_deref().unwrap_or(Path::new("")).display(),
            to = %to.display(),
            "WARNING: --accept-snapshot-overwrite: the store and Raft log found in the data \
             directory (no node.json) were moved aside; this node joins empty and catches up \
             from the leader"
        );
    }
    let store_existed = cfg.storage_backend.is_some() || paths.store.exists();
    let log_probe = match &cfg.storage_backend {
        Some(_) => None,
        None => Some(RedbLogStore::probe(&paths.log)?),
    };
    // A lost store is refused before opening one would create it (and
    // make the next start look consistent).
    if let (Some(probe), Some(p)) = (log_probe, &plan) {
        if p.existing.is_some() && !store_existed {
            paths::check_log_and_store(
                paths.data_dir.as_deref().unwrap_or(Path::new("")),
                probe,
                false,
                0,
            )?;
        }
    }
    // redb's exclusive lock is the ownership check (`Locked` for a second
    // server on the same file); everything else follows.
    let slot = StoreSlot::open_with(
        &paths.store,
        extractors,
        cfg.cache_bytes,
        cfg.snapshot_max_age,
        cfg.storage_backend.clone(),
    )?;
    slot.set_install_gate(cfg.install_gate.clone());
    if let Some(at) = cfg.testing.fail_in_apply_txn {
        slot.set_marked_commit_hook(Some(Arc::new(move |m: &graph_store::RaftMarker| {
            if m.index == at {
                tracing::warn!(index = at, "testing: failpoint in the apply transaction");
                Err(StoreError::Storage(format!(
                    "testing: failpoint in the transaction applying entry {at}"
                )))
            } else {
                Ok(())
            }
        })));
    }
    let mut initialize = plan.is_none();
    let (identity, advertise) = match (&plan, &paths.node_json) {
        (Some(plan), Some(json_path)) => {
            let (json, advertise) = match &plan.existing {
                Some(found) => {
                    if let Some(a) = &cfg.advertise {
                        if a != &found.advertise {
                            return Err(StoreError::Rejected(format!(
                                "--advertise {a} differs from {} recorded in `{}` \
                                 (changing it needs --update-advertise, a later stage)",
                                found.advertise,
                                json_path.display()
                            )));
                        }
                    }
                    // The log and the store must agree before the node
                    // takes part in any election (QA: a lost raft.redb
                    // would let it vote twice in a term).
                    if let Some(probe) = log_probe {
                        let marker = slot.with_store(|s| s.raft_marker())?.map_or(0, |m| m.index);
                        paths::check_log_and_store(
                            paths.data_dir.as_deref().unwrap_or(Path::new("")),
                            probe,
                            store_existed,
                            marker,
                        )?;
                    }
                    initialize = found.bootstrapped;
                    (found.clone(), found.advertise.clone())
                }
                None => {
                    let advertise = cfg
                        .advertise
                        .clone()
                        .unwrap_or_else(|| paths::default_advertise(addr));
                    let cluster_id = plan.bootstrap.then(paths::mint_cluster_id);
                    let json = NodeJson {
                        node_id,
                        cluster_id: cluster_id.clone(),
                        advertise: advertise.clone(),
                        binary_version: crate::SERVER_VERSION.into(),
                        protocol_version: graph_proto::PROTOCOL_VERSION,
                        store_format_version: graph_store::SCHEMA_VERSION,
                        extractors_hash: hash.clone(),
                        created: paths::now_secs(),
                        bootstrapped: plan.bootstrap,
                    };
                    json.write(json_path)?;
                    if cfg.testing.fail_after_node_json {
                        return Err(StoreError::Storage(
                            "testing: failpoint after writing node.json, before the Raft \
                             node is initialized"
                                .into(),
                        ));
                    }
                    initialize = plan.bootstrap;
                    if let Some(id) = &cluster_id {
                        tracing::warn!(
                            cluster_id = %id,
                            node_id,
                            data_dir = %paths.data_dir.as_deref().unwrap_or(Path::new("")).display(),
                            "WARNING: --bootstrap created a NEW cluster; nodes of any other \
                             cluster will refuse it"
                        );
                    }
                    (json, advertise)
                }
            };
            (
                Arc::new(ClusterIdentity::for_node(json_path, json)),
                advertise,
            )
        }
        _ => (
            Arc::new(ClusterIdentity::fixed("standalone")),
            cfg.advertise.clone().unwrap_or_else(|| addr.to_string()),
        ),
    };
    let lock = LockFile::create_at(&paths.lock, &addr.to_string())
        .map_err(|e| io_err("LOCK sidecar", e))?;
    let snapshots = Arc::new(SnapshotDir::open(&paths.snapshots_dir, &hash)?);
    let guard_dir = match &paths.data_dir {
        Some(d) => d.clone(),
        None => paths
            .store
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
    };
    let disk = DiskGuard::new(
        cfg.min_free_disk,
        cfg.free_space_probe.clone().unwrap_or_else(system_probe),
        &guard_dir,
        &paths.store,
        &paths.log,
    );
    let settings = cfg.raft.unwrap_or(if paths.data_dir.is_some() {
        RaftSettings::cluster()
    } else {
        RaftSettings::standalone()
    });
    let obs = crate::observe::Observability::new();
    let mut raft = RaftNode::start(NodeStart {
        node_id,
        advertise: advertise.clone(),
        slot: Arc::clone(&slot),
        log_path: paths.log.clone(),
        snapshots: Arc::clone(&snapshots),
        identity: Arc::clone(&identity),
        // `--db` mode initializes a one-member cluster on first start
        // (stage A); `--data-dir` only a node that minted its cluster
        // (`bootstrapped` in node.json: `--bootstrap` on an empty dir),
        // also on a later start if a crash came before the initialization
        // (guarded by `is_initialized`, so a no-op once it happened).
        initialize,
        storage_backend: cfg.storage_backend.clone(),
        extractors_hash: hash.clone(),
        settings,
        disk,
        faults: cfg.fault_plan.clone(),
        failpoints: SmFailpoints {
            fail_before_apply: cfg.testing.fail_before_apply,
        },
        append_observer: cfg.append_observer.clone(),
        obs: Arc::clone(&obs),
        testing_apply_gate: cfg.testing_apply_gate.clone(),
    })
    .await?;
    raft.withhold_leader = cfg.testing.withhold_leader;
    raft.hold_proposal = cfg.testing.hold_proposal_ms.map(Duration::from_millis);
    let shutdown = ShutdownHandle::new();
    let ctx = Arc::new(Ctx {
        slot: Arc::clone(&slot),
        raft: raft.clone(),
        info: ServerInfo {
            node_id,
            identity: Arc::clone(&identity),
            extractors_hash: hash,
            db_path: paths.store.display().to_string(),
            listen_addr: addr.to_string(),
            started: Instant::now(),
            hello_protocol_version: cfg
                .testing
                .hello_protocol_version
                .unwrap_or(graph_proto::PROTOCOL_VERSION),
            data_dir: paths
                .data_dir
                .as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
            advertise,
        },
        shutdown: shutdown.clone(),
        sysinfo: cfg.sysinfo.clone(),
        stall_writes_after: cfg.testing.stall_writes_after,
        transfer_hold: cfg.testing.transfer_hold_ms.map(Duration::from_millis),
        writes_proposed: std::sync::atomic::AtomicUsize::new(0),
        fwd: crate::forward::Forwarder::new(node_id, cfg.fault_plan.clone())
            .with_default_timeout(cfg.testing.forward_timeout_ms.map(Duration::from_millis)),
        auto_promoting: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        last_elect: std::sync::Mutex::new(None),
    });

    // Health (D10): "" is SERVING once the store is open (now);
    // `memory-graph.ready` follows the known leader.
    let (reporter, health) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", ServingStatus::Serving)
        .await;
    {
        // Shutdown step 1: the moment shutdown starts, both health names
        // go NOT_SERVING (load balancers stop routing here during the
        // drain), before anything else is torn down.
        let reporter = reporter.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            shutdown.wait().await;
            mark_not_serving(&reporter).await;
        });
    }
    {
        let reporter = reporter.clone();
        let shutdown = shutdown.clone();
        let mut rx = raft.raft.metrics();
        let raft_for_health = raft.clone();
        let max_lag = cfg.ready_max_lag;
        let silence = crate::observe::leader_silence_limit(settings.election_max_ms);
        // Re-checked at least this often: the time since a leader last
        // reached this node grows without any event.
        let recheck = (silence / 4).min(Duration::from_secs(1));
        tokio::spawn(async move {
            let mut last = None;
            loop {
                if shutdown.is_triggered() {
                    return;
                }
                let ready = raft_ready(&raft_for_health, &rx.borrow_and_update(), max_lag, silence);
                if last != Some(ready) {
                    tracing::info!(ready, max_lag, "readiness changed");
                    last = Some(ready);
                }
                reporter
                    .set_service_status(
                        READY_SERVICE,
                        if ready {
                            ServingStatus::Serving
                        } else {
                            ServingStatus::NotServing
                        },
                    )
                    .await;
                if shutdown.is_triggered() {
                    // Raced the shutdown watcher: never leave SERVING behind.
                    mark_not_serving(&reporter).await;
                    return;
                }
                // Applied index and leadership come with the Raft
                // metrics; the leader's committed index with its
                // heartbeats (noted by the Raft service).
                tokio::select! {
                    r = rx.changed() => if r.is_err() { return },
                    _ = raft_for_health.obs.leader_commit_changed() => {}
                    // The leader's silence, and a backstop for a change
                    // noted between the check and the wait
                    // (`Notify::notify_waiters` keeps no permit).
                    _ = tokio::time::sleep(recheck) => {}
                }
            }
        });
    }
    tokio::spawn(SnapshotTable::reaper(Arc::downgrade(&slot)));

    if let Some(l) = metrics_listener {
        tokio::spawn(crate::observe::serve_metrics(
            l,
            Arc::clone(&ctx),
            shutdown.clone(),
        ));
    }

    let no_limit = usize::MAX;
    let router = tonic::transport::Server::builder()
        .layer(crate::observe::RpcLayer::new(Arc::clone(&obs)))
        .add_service(health)
        .add_service(InterceptedService::new(
            StoreServer::new(StoreService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
            CheckVersion,
        ))
        .add_service(InterceptedService::new(
            WriteServer::new(WriteService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
            CheckVersion,
        ))
        .add_service(InterceptedService::new(
            AdminServer::new(AdminService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
            CheckVersion,
        ))
        .add_service(InterceptedService::new(
            RaftServer::new(RaftService {
                raft: raft.raft.clone(),
                identity: Arc::clone(&identity),
                snapshots: Arc::clone(&snapshots),
                disk: raft.disk.clone(),
                delay_append: cfg
                    .testing
                    .delay_append_entries_ms
                    .map(Duration::from_millis),
                obs: Arc::clone(&obs),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
            CheckVersion,
        ));
    let incoming = {
        let weak = Arc::downgrade(&slot);
        TcpListenerStream::new(listener).map(move |r| r.map(|t| ConnIo::for_slot(t, weak.clone())))
    };
    let signal = {
        let s = shutdown.clone();
        async move { s.wait().await }
    };
    let mut serve = tokio::spawn(router.serve_with_incoming_shutdown(incoming, signal));
    tracing::info!(
        %addr,
        metrics = ?metrics_addr,
        store = %paths.store.display(),
        node_id,
        "serving"
    );

    let task = {
        let shutdown = shutdown.clone();
        let slot = Arc::clone(&slot);
        let raft = raft.clone();
        let reporter = reporter.clone();
        let grace = cfg.shutdown_grace;
        tokio::spawn(async move {
            let result = tokio::select! {
                r = &mut serve => match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(StoreError::Storage(format!("server: {e}"))),
                    Err(e) => Err(StoreError::Storage(format!("server task: {e}"))),
                },
                _ = async { shutdown.wait().await; tokio::time::sleep(grace).await } => {
                    tracing::warn!(?grace, "in-flight requests did not finish within the grace period; aborting");
                    serve.abort();
                    Ok(())
                }
            };
            // Health already went NOT_SERVING when shutdown began (above);
            // re-assert it in case serving ended on its own.
            mark_not_serving(&reporter).await;
            if tokio::time::timeout(grace, raft.shutdown()).await.is_err() {
                tracing::warn!(
                    ?grace,
                    "raft node did not shut down within the grace period; continuing"
                );
            }
            slot.close();
            // Last: the sidecar names this process as the holder until the
            // store is closed.
            drop(lock);
            tracing::info!("stopped");
            result
        })
    };
    // `--join`: serving now (the leader asks this node who it is before
    // adding it), so ask to be added; a refusal or the timeout stops the
    // server again and fails the start.
    if let InitMode::Join(spec) = &cfg.init {
        let req = crate::join::join_request(
            node_id,
            &ctx.info.advertise,
            &ctx.info.extractors_hash,
            spec.auto_promote,
        );
        if identity.get().is_none() {
            if let Err(e) = crate::join::join(spec, req.clone(), &identity).await {
                shutdown.trigger();
                let _ = task.await;
                return Err(e);
            }
        }
        if spec.auto_promote {
            crate::join::spawn_rejoin(raft.clone(), spec.peer.clone(), req, shutdown.clone());
        }
    }
    Ok(Running {
        addr,
        metrics_addr,
        shutdown,
        task,
        slot,
        raft,
        paths,
        identity,
    })
}

/// Whether `memory-graph.ready` is `SERVING`: a leader is known (and the
/// test hook does not hide it), a leader reached this node within `silence`
/// (unless it is the leader), and it applied the leader's committed index
/// to within `max_lag` entries ([`crate::observe::is_ready`]).
fn raft_ready(
    raft: &RaftNode,
    m: &openraft::RaftMetrics<crate::raft::NodeId, openraft::impls::BasicNode>,
    max_lag: u64,
    silence: Duration,
) -> bool {
    crate::observe::is_ready(
        crate::observe::Readiness {
            leader_known: !raft.withhold_leader && m.current_leader.is_some(),
            is_leader: m.current_leader == Some(raft.node_id)
                && m.state == openraft::ServerState::Leader,
            applied: m.last_applied.as_ref().map_or(0, |l| l.index),
            leader_commit: raft.obs.leader_commit(),
            since_heard: raft.obs.since_heard_from_leader(),
        },
        max_lag,
        silence,
    )
}

/// Shutdown step 1: report `""` and [`READY_SERVICE`] as `NOT_SERVING`.
pub(crate) async fn mark_not_serving(reporter: &tonic_health::server::HealthReporter) {
    reporter
        .set_service_status("", ServingStatus::NotServing)
        .await;
    reporter
        .set_service_status(READY_SERVICE, ServingStatus::NotServing)
        .await;
}

/// The blocking entry point (`memory-graph serve`): builds a multi-thread
/// runtime, serves until Ctrl-C / SIGTERM (or `Admin.Shutdown`), then
/// shuts down gracefully.
pub fn run_blocking(
    cfg: ServeConfig,
    extractors: Vec<Box<dyn Extractor>>,
) -> Result<(), StoreError> {
    run_blocking_with(cfg, extractors, |_| {})
}

/// [`run_blocking`], calling `on_ready` with the started server (its bound
/// address, and `/metrics` address if any) once the
/// store is open, the LOCK sidecar written and the listener accepting (the
/// CLI prints it, so `--listen 127.0.0.1:0` is usable by scripts and tests).
pub fn run_blocking_with(
    cfg: ServeConfig,
    extractors: Vec<Box<dyn Extractor>>,
    on_ready: impl FnOnce(&Running) + Send + 'static,
) -> Result<(), StoreError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| io_err("tokio runtime", e))?;
    rt.block_on(async move {
        let running = start(cfg, share(extractors)).await?;
        on_ready(&running);
        let handle = running.shutdown_handle();
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!("signal received; shutting down");
            handle.trigger();
        });
        running.wait().await
    })
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic_health::pb::health_server::Health;
    use tonic_health::pb::{health_check_response::ServingStatus as Pb, HealthCheckRequest};
    use tonic_health::server::{HealthReporter, HealthService};

    async fn status(svc: &HealthService, name: &str) -> i32 {
        svc.check(tonic::Request::new(HealthCheckRequest {
            service: name.into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .status
    }

    /// Shutdown step 1 reports both health names NOT_SERVING.
    #[tokio::test]
    async fn mark_not_serving_flips_both_health_names() {
        let reporter = HealthReporter::new();
        let svc = HealthService::from_health_reporter(reporter.clone());
        reporter
            .set_service_status(READY_SERVICE, ServingStatus::Serving)
            .await;
        assert_eq!(status(&svc, "").await, Pb::Serving as i32);
        assert_eq!(status(&svc, READY_SERVICE).await, Pb::Serving as i32);
        mark_not_serving(&reporter).await;
        assert_eq!(status(&svc, "").await, Pb::NotServing as i32);
        assert_eq!(status(&svc, READY_SERVICE).await, Pb::NotServing as i32);
    }
}
