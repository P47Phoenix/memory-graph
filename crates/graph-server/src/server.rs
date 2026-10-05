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
    /// `--update-advertise`: a restarted member now listens at this address;
    /// it asks the cluster to record it, then rewrites `node.json`
    /// ([`crate::advertise`]). Refused on a first start and in `--db` mode.
    pub update_advertise: Option<String>,
    /// How long `--update-advertise` keeps asking before the start fails.
    pub update_advertise_timeout: Duration,
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
    /// `--mcp-listen` and the other `--mcp-*` flags (ADR 0005 D4): the
    /// MCP endpoint; `None` (the default): no MCP endpoint listens.
    pub mcp: Option<crate::mcp::McpConfig>,
    /// `--ready-max-lag`: `memory-graph.ready` is `SERVING` only while this
    /// node's applied index is within this many entries of the leader's
    /// committed index (default [`crate::DEFAULT_READY_MAX_LAG`]).
    pub ready_max_lag: u64,
    /// Testing only, not a supported API (hidden from the docs, like the
    /// failpoints): hold apply back (see
    /// [`crate::raft::state_machine::TestingApplyGate`]).
    #[doc(hidden)]
    pub testing_apply_gate: Option<crate::raft::state_machine::TestingApplyGate>,
    /// `--backup-url` and friends (ADR 0006): upload snapshots; `None`: off.
    pub backup: Option<crate::backup::BackupConfig>,
    /// `--restore-allow-extractor-mismatch`: a verified restore accepts a
    /// backup made by other extractors.
    pub restore_allow_extractor_mismatch: bool,
    /// The `s3://` settings of an `s3://` `--restore` (`--backup-endpoint`,
    /// `--backup-region`, `--backup-virtual-host`,
    /// `--backup-credentials-file`, `--backup-profile`,
    /// `--backup-connect-to`).
    pub restore_s3: crate::backup::S3Options,
    /// `--worker-threads`: the tokio runtime's worker threads in
    /// [`run_blocking`]; `None` keeps tokio's default (one per CPU, or
    /// `TOKIO_WORKER_THREADS`). Fewer idle workers mean fewer idle wakeups
    /// on a many-core host. [`start`] runs on the caller's runtime and
    /// ignores it.
    pub worker_threads: Option<std::num::NonZeroUsize>,
}

/// How long a restart whose `node.json` address differs from the one its
/// membership lists waits for the leader to catch it up (its log may be
/// merely behind) before refusing to start.
pub const ADVERTISE_CATCHUP: Duration = Duration::from_secs(5);

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
    /// Failpoint: `--update-advertise` fails right after the cluster
    /// committed the new address, before `node.json` is rewritten (a crash
    /// in that window).
    pub fail_before_advertise_rewrite: bool,
    /// `--update-advertise` rewrites `node.json` as soon as the leader
    /// committed the new address, without waiting for this node's own log
    /// to hold it (the behaviour before that wait existed), so a test can
    /// leave a node with a lagging membership view.
    pub advertise_rewrite_skip_wait: bool,
    /// How long a restart whose `node.json` address differs from its
    /// membership's waits for the leader to catch it up before refusing
    /// (default [`ADVERTISE_CATCHUP`]).
    pub advertise_catchup_ms: Option<u64>,
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
            update_advertise: None,
            update_advertise_timeout: crate::advertise::DEFAULT_UPDATE_ADVERTISE_TIMEOUT,
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
            mcp: None,
            ready_max_lag: crate::DEFAULT_READY_MAX_LAG,
            testing_apply_gate: None,
            backup: None,
            restore_allow_extractor_mismatch: false,
            restore_s3: crate::backup::S3Options::default(),
            worker_threads: None,
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
            .field("update_advertise", &self.update_advertise)
            .field("raft", &self.raft)
            .field("min_free_disk", &self.min_free_disk)
            .field("cache_bytes", &self.cache_bytes)
            .field("snapshot_max_age", &self.snapshot_max_age)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("sysinfo", &self.sysinfo.is_some())
            .field("testing", &self.testing)
            .field("metrics_listen", &self.metrics_listen)
            .field("mcp", &self.mcp)
            .field("ready_max_lag", &self.ready_max_lag)
            .field("backup", &self.backup)
            .field(
                "restore_allow_extractor_mismatch",
                &self.restore_allow_extractor_mismatch,
            )
            .field("restore_s3", &self.restore_s3)
            .field("worker_threads", &self.worker_threads)
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
    /// Where the MCP endpoint listens (`--mcp-listen`), as bound; `None`:
    /// off. The endpoint is `http://<mcp_addr>/mcp`.
    pub mcp_addr: Option<SocketAddr>,
    shutdown: ShutdownHandle,
    task: tokio::task::JoinHandle<Result<(), StoreError>>,
    pub slot: Arc<StoreSlot>,
    pub raft: RaftNode,
    /// Where this node keeps its files.
    pub paths: NodePaths,
    /// The cluster id as this node knows it.
    pub identity: Arc<ClusterIdentity>,
    /// The backup uploader (`--backup-url`).
    pub backup: Option<crate::backup::Backup>,
}

impl Running {
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// The worker threads of the runtime the caller is on (what
    /// `--worker-threads`, `TOKIO_WORKER_THREADS` or the CPU count gave to
    /// [`run_blocking`]'s runtime, where `on_ready` runs); 0 outside one.
    pub fn worker_threads(&self) -> usize {
        tokio::runtime::Handle::try_current()
            .map(|h| h.metrics().num_workers())
            .unwrap_or(0)
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
    if cfg.testing != TestingHooks::default() || cfg.testing_apply_gate.is_some() {
        eprintln!("memory-graph serve: WARNING: test-only fault injection hooks are active");
        tracing::warn!(
            hooks = ?cfg.testing,
            apply_gate = cfg.testing_apply_gate.is_some(),
            "test-only fault injection hooks are active"
        );
    }
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
    // A bad --backup-url (or s3:// endpoint, region or credentials) is
    // refused before anything is written. Opening a sink sends nothing.
    if let Some(b) = &cfg.backup {
        if b.sink.is_none() {
            drop(
                b.open_sink()
                    .map_err(|e| StoreError::Rejected(format!("--backup-url {e}")))?,
            );
        }
    }
    // `--update-advertise` moves a member: refused before anything is
    // opened or written for a node that is not one.
    if cfg.update_advertise.is_some() {
        match &plan {
            None => {
                return Err(StoreError::Rejected(
                    "--update-advertise needs --data-dir (a --db server is a cluster of one; \
                     its address is --advertise)"
                        .into(),
                ))
            }
            Some(p) if p.existing.is_none() => {
                return Err(StoreError::Rejected(
                    "--update-advertise changes the address of a node that already belongs \
                     to a cluster; this data directory is new (use --advertise)"
                        .into(),
                ))
            }
            Some(_) => {}
        }
    }
    if let Some(r) = &cfg.raft {
        r.validate(paths.data_dir.is_some())
            .map_err(|e| StoreError::Rejected(format!("Raft settings: {e}")))?;
    }
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
    // `--mcp-listen` (ADR 0005 D4): loopback unless allowed, bound before
    // anything is written like the other listeners.
    let mcp_listener = match &cfg.mcp {
        Some(m) => {
            crate::mcp::check_bind(m)?;
            let l = TcpListener::bind(m.listen)
                .await
                .map_err(|e| io_err(&format!("cannot listen on {} (--mcp-listen)", m.listen), e))?;
            let bound = l.local_addr().map_err(|e| io_err("MCP local address", e))?;
            crate::mcp::warn_at_start(m, bound);
            Some((l, bound))
        }
        None => None,
    };
    let mcp_addr = mcp_listener.as_ref().map(|(_, a)| *a);
    if let Some(dir) = &paths.data_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| io_err(&format!("creating `{}`", dir.display()), e))?;
    }
    if let Some(snap) = plan.as_ref().and_then(|p| p.restore.as_deref()) {
        let checks = crate::backup::restore::RestoreChecks {
            extractors_hash: hash.clone(),
            allow_extractor_mismatch: cfg.restore_allow_extractor_mismatch,
            min_free_disk: cfg.min_free_disk,
            probe: Some(cfg.free_space_probe.clone().unwrap_or_else(system_probe)),
        };
        let s = snap.to_string_lossy().into_owned();
        if crate::backup::is_url(&s) {
            // Blocking (the s3:// client refuses to run on this runtime).
            let store = paths.store.clone();
            let s3 = cfg.restore_s3.clone();
            crate::backup::off_runtime("--restore", move || {
                crate::backup::restore::restore_from_url(&s, &store, &checks, &s3)
            })
            .await
            .map_err(StoreError::Storage)??;
        } else {
            crate::backup::restore::restore_from_path(snap, &paths.store, &checks)?;
        }
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
    // `--update-advertise` on a restart whose address differs: the new
    // address, and node.json as it is until the cluster recorded it.
    let mut moved: Option<(PathBuf, NodeJson)> = None;
    let (identity, advertise) = match (&plan, &paths.node_json) {
        (Some(plan), Some(json_path)) => {
            let (json, advertise) = match &plan.existing {
                Some(found) => {
                    if let Some(a) = &cfg.advertise {
                        if a != &found.advertise {
                            return Err(StoreError::Rejected(format!(
                                "--advertise {a} differs from {} recorded in `{}`; to move \
                                 this node to a new address, restart it with \
                                 --update-advertise {a} (instead of --advertise)",
                                found.advertise,
                                json_path.display()
                            )));
                        }
                    }
                    let advertise = match &cfg.update_advertise {
                        Some(a) if a != &found.advertise => {
                            if found.cluster_id.is_none() {
                                return Err(StoreError::Rejected(format!(
                                    "--update-advertise {a}: this node never joined its \
                                     cluster (no cluster id in `{}`), so no member knows its \
                                     address; start it with --join again",
                                    json_path.display()
                                )));
                            }
                            tracing::warn!(
                                from = %found.advertise,
                                to = %a,
                                "--update-advertise: moving this node to a new address"
                            );
                            let mut json = found.clone();
                            json.advertise = a.clone();
                            moved = Some((json_path.clone(), json));
                            a.clone()
                        }
                        Some(a) => {
                            tracing::info!(
                                addr = %a,
                                "--update-advertise: already the recorded address; nothing to do"
                            );
                            found.advertise.clone()
                        }
                        None => found.advertise.clone(),
                    };
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
                    (found.clone(), advertise)
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
    // A restart that keeps node.json's address (no move asked for): the
    // membership this node loaded must list it there too. They differ after
    // a `--update-advertise` that stopped between the cluster committing the
    // new address and node.json being rewritten; serving at the old address
    // would then leave the leader replicating to the new one. Refused, with
    // the flag that finishes the move (ADR 0004 Q3, docs/deploy/data-dir.md).
    // Checked once serving (below): a log that is merely behind gets a
    // bounded wait to catch up from the leader first.
    let check_address = moved.is_none() && plan.as_ref().is_some_and(|p| p.existing.is_some());
    raft.withhold_leader = cfg.testing.withhold_leader;
    raft.hold_proposal = cfg.testing.hold_proposal_ms.map(Duration::from_millis);
    // Backups (ADR 0006): each new snapshot build hands the uploader a job
    // and returns; the uploader decides by the leadership at that moment.
    let backup = match &cfg.backup {
        Some(b) => {
            let backup = crate::backup::Backup::start(b.clone(), Arc::clone(&identity))
                .map_err(|e| StoreError::Rejected(format!("--backup-url {e}")))?;
            // The metrics channel, not the Raft handle: the hook lives in
            // the snapshot directory, which the Raft node owns.
            let rx = raft.raft.metrics();
            let withhold = raft.withhold_leader;
            backup.set_is_leader(Box::new(move || {
                let m = rx.borrow();
                !withhold
                    && m.state == openraft::ServerState::Leader
                    && m.current_leader == Some(node_id)
            }));
            let hook = backup.clone();
            snapshots.set_on_built(Some(Arc::new(move |side, path| {
                hook.snapshot_built(side, path)
            })));
            Some(backup)
        }
        None => None,
    };
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
            mcp_addr,
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
        backup: backup.clone(),
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
        // A follower re-checks at least this often: the time since a
        // leader last reached it grows without any event.
        let recheck = (silence / 4).min(Duration::from_secs(1));
        tokio::spawn(async move {
            let mut last = None;
            loop {
                if shutdown.is_triggered() {
                    return;
                }
                let (ready, leads) = {
                    let m = rx.borrow_and_update();
                    let leads = m.state == openraft::ServerState::Leader
                        && m.current_leader == Some(raft_for_health.node_id);
                    (raft_ready(&raft_for_health, &m, max_lag, silence), leads)
                };
                // Only a change is reported: tonic-health notifies every
                // watcher on each update, even an unchanged one (issue
                // #205: an idle server must not wake for nothing).
                if last != Some(ready) {
                    tracing::info!(ready, max_lag, "readiness changed");
                    last = Some(ready);
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
                }
                if shutdown.is_triggered() {
                    // Raced the shutdown watcher: never leave SERVING behind.
                    mark_not_serving(&reporter).await;
                    return;
                }
                // Applied index and leadership come with the Raft
                // metrics; the leader's committed index with its
                // heartbeats (noted by the Raft service). A leader is
                // ready for as long as it leads, so only a follower needs
                // the timer (the `withhold_leader` test hook's leader is
                // not ready for as long as it leads: no timer either).
                tokio::select! {
                    r = rx.changed() => if r.is_err() { return },
                    _ = raft_for_health.obs.leader_commit_changed() => {}
                    _ = shutdown.wait() => {}
                    // The leader's silence, and a backstop for a change
                    // noted between the check and the wait
                    // (`Notify::notify_waiters` keeps no permit).
                    _ = tokio::time::sleep(recheck), if !leads => {}
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

    if let (Some((l, _)), Some(m)) = (mcp_listener, cfg.mcp.clone()) {
        tokio::spawn(crate::mcp::serve(l, m, Arc::clone(&ctx), shutdown.clone()));
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
        mcp = ?mcp_addr,
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
        let backup = backup.clone();
        let snapshots = Arc::clone(&snapshots);
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
            // Backups stop taking jobs (an upload in progress is left to
            // finish or fail on its own thread; nothing waits for it).
            snapshots.set_on_built(None);
            if let Some(b) = &backup {
                b.stop();
            }
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
    if check_address && !listed_at(&raft, node_id, &ctx.info.advertise) {
        // Serving at node.json's address: if the leader lists this node
        // there, it replicates here and the view catches up; if it lists
        // another address, it never does and the start is refused.
        let wait = cfg
            .testing
            .advertise_catchup_ms
            .map_or(ADVERTISE_CATCHUP, Duration::from_millis);
        let advertise = ctx.info.advertise.clone();
        let want = advertise.clone();
        let _ = raft
            .raft
            .wait(Some(wait))
            .metrics(
                move |m| {
                    m.membership_config
                        .membership()
                        .get_node(&node_id)
                        .is_none_or(|n| n.addr == want)
                },
                "node.json's address in this node's membership",
            )
            .await;
        let listed = raft
            .metrics()
            .membership_config
            .membership()
            .get_node(&node_id)
            .map(|n| n.addr.clone());
        if let Some(listed) = listed.filter(|l| *l != advertise) {
            shutdown.trigger();
            let _ = task.await;
            return Err(StoreError::Rejected(format!(
                "this node's address in node.json is {advertise}, but the cluster's membership \
                 records node {node_id} at {listed} (an --update-advertise that stopped after \
                 the cluster committed it); restart with --update-advertise {listed}, listening \
                 where {listed} reaches, to finish the move"
            )));
        }
        tracing::info!(%advertise, "membership caught up with node.json's address");
    }
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
    // `--update-advertise`: serving at the new address now (the leader asks
    // it who it is), so ask the cluster to record it: through this node
    // first (it forwards to the leader it knows, or leads itself), then the
    // other members at the addresses its membership lists. node.json
    // follows only once the change is committed; a refusal or the timeout
    // stops the server again and fails the start, node.json unchanged.
    if let Some((json_path, json)) = moved {
        let mut endpoints = vec![addr.to_string()];
        endpoints.extend(
            raft.metrics()
                .membership_config
                .membership()
                .nodes()
                .filter(|(id, _)| **id != node_id)
                .map(|(_, n)| n.addr.clone()),
        );
        let done = crate::advertise::update_advertise(
            &endpoints,
            node_id,
            &json.advertise,
            cfg.update_advertise_timeout,
        )
        .await;
        // Committed, but node.json not rewritten: say so, and how to finish.
        let not_rewritten = |e: StoreError| {
            StoreError::Storage(format!(
                "the cluster recorded node {node_id} at {new}, but `{}` was not rewritten: {e}; \
                 fix that, then restart with --update-advertise {new} to finish the move (a \
                 plain restart is refused until then)",
                json_path.display(),
                new = json.advertise,
            ))
        };
        // The leader's commit does not need this node in its majority, so
        // this node's own log may not hold the new address yet. node.json
        // follows only once it does (bounded wait): otherwise a stop in that
        // window leaves node.json ahead of the local membership, and the
        // restart check would name the old address.
        let in_own_log = |raft: &RaftNode| {
            let want = json.advertise.clone();
            let r = raft.raft.clone();
            async move {
                r.wait(Some(Duration::from_secs(30)))
                    .metrics(
                        move |m| {
                            m.membership_config
                                .membership()
                                .get_node(&node_id)
                                .is_some_and(|n| n.addr == want)
                        },
                        "new address in this node's membership",
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| {
                        StoreError::Storage(format!(
                            "this node's own log did not receive the new address: {e}"
                        ))
                    })
            }
        };
        let done = match done {
            Ok(_) if cfg.testing.fail_before_advertise_rewrite => {
                // Deterministic: the crash comes once this node's own log
                // holds the committed address (bounded wait).
                let _ = in_own_log(&raft).await;
                Err(not_rewritten(StoreError::Storage(
                    "testing: failpoint after the new address was committed, before node.json \
                     was rewritten"
                        .into(),
                )))
            }
            Ok(_) if cfg.testing.advertise_rewrite_skip_wait => {
                json.write(&json_path).map_err(not_rewritten)
            }
            Ok(_) => match in_own_log(&raft).await {
                Ok(()) => json.write(&json_path).map_err(not_rewritten),
                Err(e) => Err(not_rewritten(e)),
            },
            Err(e) => Err(e),
        };
        if let Err(e) = done {
            shutdown.trigger();
            let _ = task.await;
            return Err(e);
        }
        tracing::info!(
            advertise = %json.advertise,
            node_json = %json_path.display(),
            "--update-advertise: node.json records the new address"
        );
    }
    Ok(Running {
        addr,
        metrics_addr,
        mcp_addr,
        shutdown,
        task,
        slot,
        raft,
        paths,
        identity,
        backup,
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
    let rt = build_runtime(cfg.worker_threads).map_err(|e| io_err("tokio runtime", e))?;
    rt.block_on(async move {
        // The handlers go in before anything starts (#224): a supervisor
        // may send SIGTERM as soon as it reads the start line, or while a
        // long store open or log replay runs, and a signal that arrives
        // before its handler kills the process with the default action (no
        // graceful stop, LOCK sidecar left). One received during `start` is
        // kept, and stops the server gracefully right after it started.
        let signals = Signals::install().map_err(|e| io_err("signal handlers", e))?;
        let running = start(cfg, share(extractors)).await?;
        on_ready(&running);
        let handle = running.shutdown_handle();
        tokio::spawn(async move {
            signals.wait().await;
            tracing::info!("signal received; shutting down");
            handle.trigger();
        });
        running.wait().await
    })
}

/// The multi-thread runtime [`run_blocking`] serves on: `worker_threads`
/// workers, or tokio's default (one per CPU, or `TOKIO_WORKER_THREADS`).
fn build_runtime(
    worker_threads: Option<std::num::NonZeroUsize>,
) -> std::io::Result<tokio::runtime::Runtime> {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    if let Some(n) = worker_threads {
        b.worker_threads(n.get());
    }
    b.enable_all().build()
}

/// The stop signals `serve` handles, registered when built (not when first
/// awaited): Ctrl-C and SIGTERM on Unix; on Windows Ctrl-C, and Ctrl-Break
/// (what a supervisor sends a console process it started in its own process
/// group, e.g. Python's `send_signal(CTRL_BREAK_EVENT)` in
/// `scripts/cluster_soak.py`).
struct Signals {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
}

impl Signals {
    /// Register the handlers now; from here on these signals are delivered
    /// to [`wait`](Self::wait), never to the default action.
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                int: signal(SignalKind::interrupt())?,
                term: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
                ctrl_break: tokio::signal::windows::ctrl_break().ok(),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {})
        }
    }

    /// Resolve on the first of the signals.
    async fn wait(mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.int.recv() => {}
                _ = self.term.recv() => {}
            }
        }
        #[cfg(windows)]
        {
            match &mut self.ctrl_break {
                Some(brk) => {
                    tokio::select! {
                        _ = self.ctrl_c.recv() => {}
                        _ = brk.recv() => {}
                    }
                }
                None => {
                    self.ctrl_c.recv().await;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = &mut self;
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Whether this node's membership view lists node `id` at `addr` (or not at
/// all: nothing to compare against).
fn listed_at(raft: &RaftNode, id: u64, addr: &str) -> bool {
    raft.metrics()
        .membership_config
        .membership()
        .get_node(&id)
        .is_none_or(|n| n.addr == addr)
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

    /// `--worker-threads N` builds a runtime with exactly N workers; none
    /// keeps tokio's default (at least one).
    #[test]
    fn worker_threads_sets_the_runtime_pool() {
        let rt = build_runtime(std::num::NonZeroUsize::new(2)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 2);
        let rt = build_runtime(std::num::NonZeroUsize::new(1)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 1);
        let rt = build_runtime(None).unwrap();
        assert!(rt.metrics().num_workers() >= 1);
        // The default is unchanged: one per CPU the process may use (when
        // tokio's own TOKIO_WORKER_THREADS does not say otherwise).
        if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
            assert_eq!(
                rt.metrics().num_workers(),
                std::thread::available_parallelism().unwrap().get()
            );
        }
        assert_eq!(
            ServeConfig::new("g.redb", ([127, 0, 0, 1], 0).into()).worker_threads,
            None
        );
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
