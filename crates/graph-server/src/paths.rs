//! Where a node keeps its files and who it is (ADR 0004 D6).
//!
//! Two modes resolve to one [`NodePaths`], so the rest of the server has a
//! single code path:
//!
//! * `--data-dir <dir>` (stage B):
//!   ```text
//!   <dir>/node.json          identity (NodeJson)
//!   <dir>/graph.redb         the store (state machine)
//!   <dir>/raft.redb          the Raft log and hard state
//!   <dir>/snapshots/snap-<term>-<index>.redb (+ .meta)
//!   <dir>/LOCK               the holder sidecar
//!   ```
//! * `--db <file>` (stage A, unchanged): the log at `<db>.raft.redb`, the
//!   sidecar at `<db>.LOCK`, snapshots in `<db>.snapshots/`, and no
//!   `node.json` (a standalone one-member cluster, `cluster_id`
//!   `standalone`).
//!
//! [`InitMode`] says what to do with a data directory at start-up:
//! bootstrap a new cluster (idempotent on restart), restart from persisted
//! state, start as an uninitialized member that a leader adds, or join a
//! cluster through a peer (`--join`, idempotent on restart).
use graph_store::StoreError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file layout of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePaths {
    /// The data directory (`None` in `--db` mode).
    pub data_dir: Option<PathBuf>,
    /// The store file (`graph.redb` or the `--db` file).
    pub store: PathBuf,
    /// The Raft log (`raft.redb` or `<db>.raft.redb`).
    pub log: PathBuf,
    /// Where snapshots live (`snapshots/` or `<db>.snapshots/`).
    pub snapshots_dir: PathBuf,
    /// `node.json` (`None` in `--db` mode).
    pub node_json: Option<PathBuf>,
    /// The holder sidecar (`LOCK` or `<db>.LOCK`).
    pub lock: PathBuf,
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

impl NodePaths {
    /// Stage A's `--db <file>` layout.
    pub fn for_db(db: &Path) -> Self {
        Self {
            data_dir: None,
            store: db.to_path_buf(),
            log: with_suffix(db, ".raft.redb"),
            snapshots_dir: with_suffix(db, ".snapshots"),
            node_json: None,
            lock: crate::lock::lock_path(db),
        }
    }

    /// The `--data-dir <dir>` layout (ADR 0004 D6).
    pub fn for_data_dir(dir: &Path) -> Self {
        Self {
            data_dir: Some(dir.to_path_buf()),
            store: dir.join("graph.redb"),
            log: dir.join("raft.redb"),
            snapshots_dir: dir.join("snapshots"),
            node_json: Some(dir.join("node.json")),
            lock: dir.join("LOCK"),
        }
    }

    /// Whether the node has no persisted state at all: no `node.json`, no
    /// store, no log (a data directory may exist and hold unrelated files
    /// such as a `lost+found`).
    pub fn is_empty(&self) -> bool {
        !self.node_json.as_ref().is_some_and(|p| p.exists())
            && !self.store.exists()
            && !self.log.exists()
    }
}

/// What a `--data-dir` node does at start-up (ADR 0004 D6). Ignored in
/// `--db` mode, which always behaves as stage A did (a one-member cluster
/// initialized on first start).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InitMode {
    /// `--bootstrap`: on an empty directory mint a cluster id, write
    /// `node.json` and initialize a one-member cluster (this node, voter);
    /// on a directory that already has a `node.json`, an idempotent
    /// restart. With `restore`, seed the store from that snapshot file
    /// first (empty directory only; the Raft state in it is stripped so the
    /// new cluster's log starts at 0).
    Bootstrap { restore: Option<PathBuf> },
    /// Neither flag: restart from persisted state; an empty directory is
    /// refused (pass `--bootstrap`, or `--join`).
    #[default]
    Restart,
    /// Start empty and wait to be added by a leader (`AddLearner`); the
    /// cluster id is adopted from the first leader that contacts this
    /// node. On a directory that already has a `node.json`, a restart.
    /// (Library and test use; the CLI's `--join` is [`InitMode::Join`].)
    Uninitialized,
    /// `--join <peer>` (ADR 0004 D6/D9): on an empty directory start as
    /// [`InitMode::Uninitialized`], then ask `peer` (any member; it
    /// forwards to the leader) to add this node as a learner
    /// (`Admin.Join`), retrying until a leader answers or the timeout
    /// passes. On a directory that already has a `node.json`, a restart
    /// (refused with `WrongCluster` when the peer belongs to another
    /// cluster).
    Join(JoinSpec),
    /// `--bootstrap-or-join` (a StatefulSet, ADR 0004 D10): ordinal 0
    /// bootstraps and the others join, except that an ordinal 0 whose data
    /// directory is not initialized first asks its siblings whether a
    /// cluster already exists (pod 0 lost its volume) and joins it rather
    /// than creating a second one. Resolved into [`InitMode::Bootstrap`] or
    /// [`InitMode::Join`] by [`crate::join::resolve_bootstrap_or_join`]
    /// before anything is opened.
    BootstrapOrJoin(BootstrapOrJoin),
}

/// The `--bootstrap-or-join` settings ([`InitMode::BootstrapOrJoin`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOrJoin {
    /// This pod's ordinal (the host name's `-<n>` suffix).
    pub ordinal: u64,
    /// How to join: the peer is the named `--bootstrap-or-join` address
    /// (ordinal 0's; for ordinal 0 itself, the sibling that answered).
    pub join: JoinSpec,
    /// `host:port` of the other members to ask for an existing cluster
    /// (ordinal 0 on an uninitialized data directory only).
    pub siblings: Vec<String>,
    /// How long ordinal 0 asks before it bootstraps (it stops early once
    /// every sibling answered that it has no cluster yet).
    pub probe_timeout: std::time::Duration,
    /// `--force-bootstrap`: ordinal 0 bootstraps without asking.
    pub force_bootstrap: bool,
}

/// The `--join` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSpec {
    /// `host:port` of any member.
    pub peer: String,
    /// `--auto-promote`: the leader promotes this node to voter once its
    /// replication lag is zero. Without it (`--standby`, or neither flag)
    /// the node stays a learner until `cluster promote`.
    pub auto_promote: bool,
    /// `--accept-snapshot-overwrite`: a directory holding a store (or a
    /// Raft log) but no `node.json` is joined anyway; what it held is moved
    /// aside into `replaced-<secs>/` and the node catches up from the
    /// leader.
    pub accept_snapshot_overwrite: bool,
    /// `--join-timeout`: how long the first join keeps retrying (no leader
    /// yet, the peer unreachable) before the start fails.
    pub timeout: std::time::Duration,
}

impl JoinSpec {
    /// Join `peer` with the defaults: no auto-promote, a 2 minute timeout.
    pub fn new(peer: impl Into<String>) -> Self {
        Self {
            peer: peer.into(),
            auto_promote: false,
            accept_snapshot_overwrite: false,
            timeout: std::time::Duration::from_secs(120),
        }
    }
}

/// `node.json`: the node's identity, written once (and again only when the
/// cluster id is first learned by an uninitialized member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeJson {
    pub node_id: u64,
    /// Random 128-bit hex minted by `--bootstrap`; `None` for an
    /// uninitialized member until a leader contacts it.
    pub cluster_id: Option<String>,
    /// `host:port` peers and clients use to reach this node.
    pub advertise: String,
    pub binary_version: String,
    pub protocol_version: u32,
    pub store_format_version: u64,
    pub extractors_hash: String,
    /// Seconds since the Unix epoch at creation.
    pub created: u64,
    /// This node minted the cluster (`--bootstrap` on an empty directory)
    /// and must initialize the one-member Raft cluster if its log is not
    /// initialized yet: set before the first initialization, so a crash
    /// between writing `node.json` and initializing is finished by the next
    /// start instead of leaving a node that never elects. Absent in files
    /// written before this field existed (`false`: they were initialized).
    #[serde(default)]
    pub bootstrapped: bool,
}

/// Write `bytes` to `path` durably: a temp file beside it, fsynced, renamed
/// over `path`, and the directory fsynced (where the platform can), so a
/// crash leaves either the old or the new file, never a torn or lost one.
pub fn durable_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    crate::raft::snapshot_dir::replace_file(&tmp, path)?;
    sync_parent(path);
    Ok(())
}

/// fsync the directory holding `path` (Unix; Windows has no directory
/// handle to sync, and NTFS journals the rename's metadata itself).
pub fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl NodeJson {
    pub fn read(path: &Path) -> Result<Option<NodeJson>, StoreError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
                StoreError::Corrupt(format!(
                    "`{}` is not a valid node.json: {e}",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Storage(format!(
                "reading `{}`: {e}",
                path.display()
            ))),
        }
    }

    /// Write via [`durable_write`], so a crash never leaves a torn or lost
    /// `node.json`.
    pub fn write(&self, path: &Path) -> Result<(), StoreError> {
        let text = serde_json::to_string_pretty(self).expect("NodeJson serializes");
        durable_write(path, text.as_bytes())
            .map_err(|e| StoreError::Storage(format!("writing `{}`: {e}", path.display())))
    }
}

/// The cluster id a running node knows, shared by the services and the
/// Raft network: fixed for `--db` mode and a bootstrapped node, learned
/// from the first leader for an uninitialized member (and then written to
/// its `node.json`). Every Raft RPC carries the sender's id in the
/// `mg-cluster-id` header; a node refuses an RPC from another cluster.
pub struct ClusterIdentity {
    id: std::sync::RwLock<Option<String>>,
    node_json: Option<(PathBuf, std::sync::Mutex<NodeJson>)>,
}

/// The Raft RPC header naming the sender's cluster.
pub const CLUSTER_ID_HEADER: &str = "mg-cluster-id";

/// The Raft RPC header naming the sender's extractor version set hash: a
/// replica must extract identically (ADR 0004 D5), so a node refuses Raft
/// traffic from a node built with other extractors.
pub const EXTRACTORS_HASH_HEADER: &str = "mg-extractors-hash";

impl ClusterIdentity {
    /// A fixed id with nothing to persist (`--db` mode: `standalone`).
    pub fn fixed(id: &str) -> Self {
        Self {
            id: std::sync::RwLock::new(Some(id.to_string())),
            node_json: None,
        }
    }

    /// The identity recorded in `node.json` at `path`.
    pub fn for_node(path: &Path, json: NodeJson) -> Self {
        Self {
            id: std::sync::RwLock::new(json.cluster_id.clone()),
            node_json: Some((path.to_path_buf(), std::sync::Mutex::new(json))),
        }
    }

    pub fn get(&self) -> Option<String> {
        self.id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Accept a Raft RPC from a sender in cluster `remote` (its
    /// `mg-cluster-id` header):
    ///
    /// * this node has an id: the header is required and must match;
    /// * it has none (an uninitialized member): with `adopt` (a leader's
    ///   `AppendEntries` or `InstallSnapshot`, which make it a member) the
    ///   sender's id is adopted and persisted to `node.json`; without (a
    ///   `Vote`, which a candidate of any cluster may send and which never
    ///   makes this node a member) nothing is adopted.
    pub fn check_or_adopt(&self, remote: Option<&str>, adopt: bool) -> Result<(), String> {
        let remote = remote.filter(|r| !r.is_empty());
        // Fast path: a member checks under the read lock only.
        {
            let g = self
                .id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mine) = g.as_deref() {
                return match remote {
                    Some(r) if r == mine => Ok(()),
                    Some(r) => Err(format!(
                        "wrong cluster: this node belongs to cluster {mine}, the sender to {r}"
                    )),
                    None => Err(format!(
                        "wrong cluster: this node belongs to cluster {mine} and the sender \
                         named none (no `{CLUSTER_ID_HEADER}` header)"
                    )),
                };
            }
        }
        let Some(remote) = remote else {
            return Ok(());
        };
        if !adopt {
            return Ok(());
        }
        let mut g = self
            .id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match g.as_deref() {
            Some(mine) if mine == remote => Ok(()),
            Some(mine) => Err(format!(
                "wrong cluster: this node belongs to cluster {mine}, the sender to {remote}"
            )),
            None => {
                if let Some((path, json)) = &self.node_json {
                    let mut j = json
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    j.cluster_id = Some(remote.to_string());
                    j.write(path).map_err(|e| e.to_string())?;
                }
                tracing::info!(cluster_id = remote, "joined cluster");
                *g = Some(remote.to_string());
                Ok(())
            }
        }
    }
}

/// A fresh cluster id: 128 random bits, lowercase hex.
pub fn mint_cluster_id() -> String {
    let v: u128 = rand::random();
    format!("{v:032x}")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The machine's host name, for a default `--advertise` when listening on
/// a wildcard address (`HOSTNAME`/`COMPUTERNAME`, then `/etc/hostname`,
/// then `localhost`).
pub fn hostname() -> String {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(var) {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    "localhost".into()
}

/// The ordinal of a StatefulSet pod's host name: the number after the last
/// `-` (`memory-graph-2` -> 2, `mg-0-1` -> 1). Refused (with the reason):
/// no `-<digits>` suffix, a leading zero (`mg-007`: Kubernetes never names
/// a pod so, and `mg-07` / `mg-7` must not both be node 8), or an ordinal
/// too large for a node id.
pub fn hostname_ordinal(host: &str) -> Result<u64, String> {
    // A fully qualified name counts by its first label.
    let first = host.split('.').next().unwrap_or(host);
    let not_a_pod = || {
        format!(
            "host name `{host}` does not end in `-<ordinal>` (a StatefulSet pod name such as \
             memory-graph-0)"
        )
    };
    let (_, n) = first.rsplit_once('-').ok_or_else(not_a_pod)?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(not_a_pod());
    }
    if n.len() > 1 && n.starts_with('0') {
        return Err(format!(
            "host name `{host}`: the ordinal `{n}` has a leading zero (a StatefulSet pod \
             ordinal never does)"
        ));
    }
    n.parse::<u64>()
        .ok()
        .filter(|o| *o < u64::MAX)
        .ok_or_else(|| {
            format!(
                "host name `{host}`: the ordinal `{n}` is too large for a node id (ordinal + 1)"
            )
        })
}

/// The other pods of a StatefulSet of `replicas` pods, from ordinal 0's
/// address: `memory-graph-0.memory-graph.ns.svc:7000` with 3 replicas is
/// `memory-graph-1.memory-graph.ns.svc:7000`, `memory-graph-2...` (what
/// `--bootstrap-or-join` asks when `--peers` is not given).
pub fn sibling_addrs(peer: &str, replicas: u64) -> Result<Vec<String>, String> {
    let (host, port) = peer
        .rsplit_once(':')
        .ok_or_else(|| format!("`{peer}` is not host:port"))?;
    let (first, rest) = match host.split_once('.') {
        Some((f, r)) => (f, format!(".{r}")),
        None => (host, String::new()),
    };
    let set = first
        .strip_suffix("-0")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "`{peer}` is not ordinal 0 of a StatefulSet (`<name>-0...`); pass the other \
                 members with --peers"
            )
        })?;
    Ok((1..replicas)
        .map(|i| format!("{set}-{i}{rest}:{port}"))
        .collect())
}

/// `serve --node-id-from-hostname`: the node id of a StatefulSet pod, its
/// ordinal plus one (node ids start at 1).
pub fn node_id_from_hostname(host: &str) -> Result<u64, StoreError> {
    hostname_ordinal(host)
        .map(|n| n + 1)
        .map_err(|e| StoreError::Rejected(format!("--node-id-from-hostname: {e}")))
}

/// The default advertised address for a bound `addr`: itself, with a
/// wildcard IP (`0.0.0.0`, `[::]`) replaced by the host name.
pub fn default_advertise(addr: std::net::SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("{}:{}", hostname(), addr.port())
    } else {
        addr.to_string()
    }
}

/// What start-up decided about a data directory, before anything is
/// opened: the identity found (if any) and whether this start must
/// initialize the Raft cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPlan {
    pub node_id: u64,
    /// `node.json` as found on disk (`None`: this start creates it).
    pub existing: Option<NodeJson>,
    /// Initialize a one-member cluster (`--bootstrap` on an empty dir).
    pub bootstrap: bool,
    /// Seed the store from this snapshot file first.
    pub restore: Option<PathBuf>,
    /// `--join --accept-snapshot-overwrite` on a directory that holds a
    /// store or a log but no `node.json`: move them aside first.
    pub overwrite: bool,
}

/// Decide what to do with `paths` for `init` and the requested node id,
/// refusing every inconsistent combination before any file is written.
pub fn plan(
    paths: &NodePaths,
    init: &InitMode,
    node_id: Option<u64>,
) -> Result<StartPlan, StoreError> {
    let dir = paths
        .data_dir
        .as_deref()
        .expect("plan is for --data-dir mode");
    let json_path = paths.node_json.as_deref().expect("data-dir has node.json");
    let existing = NodeJson::read(json_path)?;
    if let Some(found) = existing {
        if let Some(asked) = node_id {
            if asked != found.node_id {
                return Err(StoreError::Rejected(format!(
                    "wrong node: `{}` belongs to node {} but --node-id {asked} was given \
                     (a data directory keeps its node id for life)",
                    dir.display(),
                    found.node_id
                )));
            }
        }
        if let InitMode::Bootstrap {
            restore: Some(snap),
        } = init
        {
            return Err(StoreError::Rejected(format!(
                "--restore `{}` needs an empty data directory, and `{}` is already \
                 initialized (node {})",
                snap.display(),
                dir.display(),
                found.node_id
            )));
        }
        // Bootstrap and Uninitialized are idempotent on an initialized
        // directory (Compose/Kubernetes restart with the same command
        // line): a restart from persisted state.
        return Ok(StartPlan {
            node_id: found.node_id,
            existing: Some(found),
            bootstrap: false,
            restore: None,
            overwrite: false,
        });
    }
    let mut overwrite = false;
    if !paths.is_empty() {
        let failed = failed_first_start(paths);
        match init {
            InitMode::Restart if failed => {
                return Err(StoreError::Rejected(format!(
                    "`{}` holds only what a failed first start leaves (a blank Raft log, an \
                     empty store, no node.json); retry that first start with its flags \
                     (--bootstrap, or --join <peer>) rather than a plain restart",
                    dir.display()
                )))
            }
            _ if failed => {}
            InitMode::Join(j) if j.accept_snapshot_overwrite => overwrite = true,
            InitMode::Join(_) => {
                return Err(StoreError::Rejected(format!(
                    "`{}` holds a store or a Raft log but no node.json (a store copied in, \
                     say); --join would replace it with the cluster's data. Pass \
                     --accept-snapshot-overwrite to join anyway (what it holds is moved \
                     aside into replaced-<time>/), or clear the directory",
                    dir.display()
                )))
            }
            _ => {
                return Err(StoreError::Rejected(format!(
                    "`{}` holds a store or a Raft log but no node.json; it was not created by \
                     `serve --data-dir` (or node.json was lost). Refusing to guess its identity",
                    dir.display()
                )))
            }
        }
    }
    let restore =
        match init {
            InitMode::BootstrapOrJoin(_) => return Err(StoreError::Rejected(
                "internal: --bootstrap-or-join must be resolved (join::resolve_bootstrap_or_join) \
                 before planning the start"
                    .into(),
            )),
            InitMode::Restart => {
                return Err(StoreError::Rejected(format!(
                    "`{}` is not initialized: pass --bootstrap to create a new cluster, or \
                 --join <peer> to join one",
                    dir.display()
                )))
            }
            InitMode::Bootstrap { restore } => restore.clone(),
            InitMode::Uninitialized | InitMode::Join(_) => None,
        };
    let node_id = node_id.ok_or_else(|| {
        StoreError::Rejected(format!(
            "--node-id is required on the first start of `{}`",
            dir.display()
        ))
    })?;
    if node_id == 0 {
        return Err(StoreError::Rejected("--node-id must be at least 1".into()));
    }
    Ok(StartPlan {
        node_id,
        existing: None,
        bootstrap: matches!(init, InitMode::Bootstrap { .. }),
        restore,
        overwrite,
    })
}

/// `--join --accept-snapshot-overwrite`: move the store, the Raft log and
/// the snapshots of a directory without `node.json` into
/// `<dir>/replaced-<secs>/` (nothing is deleted), so the node starts empty
/// and catches up from the leader. Returns where they went.
pub fn move_aside(paths: &NodePaths) -> Result<PathBuf, StoreError> {
    let dir = paths
        .data_dir
        .as_deref()
        .expect("move_aside is for --data-dir mode");
    let mut to = dir.join(format!("replaced-{}", now_secs()));
    let mut n = 1;
    while to.exists() {
        to = dir.join(format!("replaced-{}-{n}", now_secs()));
        n += 1;
    }
    std::fs::create_dir_all(&to)
        .map_err(|e| StoreError::Storage(format!("creating `{}`: {e}", to.display())))?;
    for p in [&paths.store, &paths.log, &paths.snapshots_dir] {
        if p.exists() {
            let name = p.file_name().expect("a file name");
            std::fs::rename(p, to.join(name)).map_err(|e| {
                StoreError::Storage(format!(
                    "moving `{}` into `{}`: {e}",
                    p.display(),
                    to.display()
                ))
            })?;
        }
    }
    sync_parent(&to);
    Ok(to)
}

/// Whether a data directory without `node.json` holds only what a first
/// start that failed before writing `node.json` leaves behind: a log with
/// no Raft state at all, and a store (if any) with no Raft marker and no
/// data. Such a directory is started as the empty one it effectively is
/// (a `--bootstrap` or uninitialized first start retried after, say, the
/// port was in use); anything else is refused, because its identity cannot
/// be known.
///
/// Only an explicit first-start mode (`--bootstrap`, or an uninitialized
/// start waiting for a join) takes this path; a plain restart is refused
/// with an error saying to retry with those flags. That is safe because a
/// blank log holds no vote, no entries and no purge point, and the store
/// no marker and no data: nothing that a node with an identity could have
/// promised a cluster is lost by treating the directory as new, and the
/// explicit flag is the operator saying this is a first start.
pub fn failed_first_start(paths: &NodePaths) -> bool {
    use graph_store::StoreRead;
    let log_blank = matches!(
        crate::raft::log_store::RedbLogStore::probe(&paths.log),
        Ok(p) if p.is_blank()
    );
    if !log_blank {
        return false;
    }
    if !paths.store.exists() {
        return true;
    }
    match graph_store::V2Store::open(&paths.store) {
        Ok(s) => {
            matches!(s.raft_marker(), Ok(None))
                && matches!(s.count_nodes(graph_core::NodeKind::Org), Ok(0))
        }
        Err(_) => false,
    }
}

/// The start-up consistency check of an initialized data directory (one
/// with a `node.json`) between its Raft log and its store (ADR 0004 D6).
/// The log holds the node's vote and term; losing it while keeping the
/// store would let the node vote a second time in a term it already voted
/// in, and losing the store while keeping a log that was purged (or whose
/// entries the old store had applied) would silently diverge. Refused:
///
/// * a log with no Raft state (missing or blank) beside a store whose
///   marker says entries were applied;
/// * a store that did not exist at start beside a log that has state.
pub fn check_log_and_store(
    dir: &Path,
    log: crate::raft::log_store::LogProbe,
    store_existed: bool,
    marker_index: u64,
) -> Result<(), StoreError> {
    if log.is_blank() && marker_index > 0 {
        return Err(StoreError::Rejected(format!(
            "`{}`: the Raft log (raft.redb) is {} but the store has applied entries up to \
             index {marker_index}: this node's vote and log were lost, and starting would let \
             it vote twice in one term. Refusing to start. Restore raft.redb from the same \
             backup as the store, or clear the data directory and add the node again",
            dir.display(),
            if log.exists { "empty" } else { "missing" },
        )));
    }
    if !store_existed && !log.is_blank() {
        return Err(StoreError::Rejected(format!(
            "`{}`: the store (graph.redb) is missing but the Raft log holds state: the store \
             was lost. Refusing to start. Restore graph.redb from the same backup as the log, \
             or clear the data directory and add the node again",
            dir.display()
        )));
    }
    Ok(())
}

/// `--restore`: copy `snapshot` into place as the store, check it is a
/// store in the current format, and strip its Raft state so the new
/// cluster's log starts at 0.
pub fn restore_into(snapshot: &Path, store: &Path) -> Result<(), StoreError> {
    match graph_store::detect_format(snapshot) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(StoreError::Rejected(format!(
                "--restore `{}` is not a memory-graph store file",
                snapshot.display()
            )))
        }
        Err(e) => return Err(e),
    }
    let tmp = restore_tmp(store);
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(snapshot, &tmp)
        .map_err(|e| StoreError::Storage(format!("copying `{}`: {e}", snapshot.display())))?;
    place_restore(&tmp, store)
}

/// `<store>.restore.tmp`: the one file a restore writes before the store
/// is in place (a copy of the snapshot, or a verified backup download).
pub fn restore_tmp(store: &Path) -> PathBuf {
    with_suffix(store, ".restore.tmp")
}

/// Put a staged restore (`tmp`, normally [`restore_tmp`]) in place as the
/// store: check it is a store, strip its Raft state, rename it. `tmp` is
/// removed on any failure.
pub fn place_restore(tmp: &Path, store: &Path) -> Result<(), StoreError> {
    let checked = match graph_store::detect_format(tmp) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(StoreError::Rejected(format!(
            "`{}` is not a memory-graph store file",
            tmp.display()
        ))),
        Err(e) => Err(e),
    };
    let stripped =
        checked.and_then(|()| graph_store::V2Store::open(tmp).and_then(|s| s.clear_raft_state()));
    if let Err(e) = stripped {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    std::fs::rename(tmp, store).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        StoreError::Storage(format!("placing `{}`: {e}", store.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(node_id: u64) -> NodeJson {
        NodeJson {
            node_id,
            cluster_id: Some("c".into()),
            advertise: "h:1".into(),
            binary_version: "0".into(),
            protocol_version: 1,
            store_format_version: 1,
            extractors_hash: "x".into(),
            created: 0,
            bootstrapped: false,
        }
    }

    #[test]
    fn layouts() {
        let p = NodePaths::for_data_dir(Path::new("/d"));
        assert_eq!(p.store, Path::new("/d/graph.redb"));
        assert_eq!(p.log, Path::new("/d/raft.redb"));
        assert_eq!(p.snapshots_dir, Path::new("/d/snapshots"));
        assert_eq!(p.node_json.as_deref(), Some(Path::new("/d/node.json")));
        assert_eq!(p.lock, Path::new("/d/LOCK"));
        let p = NodePaths::for_db(Path::new("/x/g.redb"));
        assert_eq!(p.log, Path::new("/x/g.redb.raft.redb"));
        assert_eq!(p.lock, Path::new("/x/g.redb.LOCK"));
        assert_eq!(p.snapshots_dir, Path::new("/x/g.redb.snapshots"));
        assert!(p.node_json.is_none());
    }

    #[test]
    fn plans() {
        let d = tempfile::tempdir().unwrap();
        let p = NodePaths::for_data_dir(d.path());
        let boot = InitMode::Bootstrap { restore: None };
        assert!(plan(&p, &InitMode::Restart, Some(1)).is_err());
        assert!(plan(&p, &boot, None).is_err(), "node id required");
        let pl = plan(&p, &boot, Some(3)).unwrap();
        assert!(pl.bootstrap && pl.existing.is_none() && pl.node_id == 3);
        let pl = plan(&p, &InitMode::Uninitialized, Some(2)).unwrap();
        assert!(!pl.bootstrap);
        json(3).write(p.node_json.as_ref().unwrap()).unwrap();
        for init in [boot.clone(), InitMode::Restart, InitMode::Uninitialized] {
            let pl = plan(&p, &init, None).unwrap();
            assert!(!pl.bootstrap && pl.node_id == 3, "{init:?}");
        }
        let e = plan(&p, &InitMode::Restart, Some(4))
            .unwrap_err()
            .to_string();
        assert!(e.contains("node 3") && e.contains("--node-id 4"), "{e}");
        let restore = InitMode::Bootstrap {
            restore: Some("s.redb".into()),
        };
        assert!(plan(&p, &restore, None).is_err());
        let d2 = tempfile::tempdir().unwrap();
        let p2 = NodePaths::for_data_dir(d2.path());
        std::fs::write(&p2.store, b"x").unwrap();
        assert!(
            plan(&p2, &boot, Some(1)).is_err(),
            "a stray store is refused"
        );
    }

    /// Dev review 1: a first start that failed before writing node.json
    /// (the port was in use, say) leaves at most an empty store and a
    /// blank log; the retry with the same flags goes ahead, a start
    /// without flags is still refused, and a store with data or a log with
    /// Raft state is never adopted.
    #[test]
    fn a_failed_first_start_can_be_retried() {
        use crate::raft::log_store::RedbLogStore;
        let d = tempfile::tempdir().unwrap();
        let p = NodePaths::for_data_dir(d.path());
        let boot = InitMode::Bootstrap { restore: None };
        drop(graph_store::V2Store::open(&p.store).unwrap());
        drop(RedbLogStore::open(&p.log).unwrap());
        assert!(failed_first_start(&p));
        assert!(plan(&p, &boot, Some(1)).unwrap().bootstrap);
        assert!(
            !plan(&p, &InitMode::Uninitialized, Some(2))
                .unwrap()
                .bootstrap
        );
        let e = plan(&p, &InitMode::Restart, Some(1)).unwrap_err();
        assert!(
            matches!(e, StoreError::Rejected(ref m) if m.contains("failed first start")
                && m.contains("--bootstrap")),
            "{e:?}"
        );
        // A store with data is somebody's store.
        let d2 = tempfile::tempdir().unwrap();
        let p2 = NodePaths::for_data_dir(d2.path());
        graph_store::Store::index_bytes(
            &graph_store::V2Store::open(&p2.store).unwrap(),
            "o",
            "r",
            "a.rs",
            b"fn a() {}",
            None,
        )
        .unwrap();
        assert!(!failed_first_start(&p2));
        assert!(plan(&p2, &boot, Some(1)).is_err());
    }

    /// Stage C: `--join` plans like an uninitialized start on an empty
    /// directory, restarts on an initialized one, and takes a directory
    /// holding a store without node.json only with
    /// `--accept-snapshot-overwrite` (which moves it aside).
    #[test]
    fn join_plans_and_the_overwrite_flag() {
        let d = tempfile::tempdir().unwrap();
        let p = NodePaths::for_data_dir(d.path());
        let join = InitMode::Join(JoinSpec::new("h:1"));
        let pl = plan(&p, &join, Some(2)).unwrap();
        assert!(!pl.bootstrap && !pl.overwrite && pl.existing.is_none());
        graph_store::Store::index_bytes(
            &graph_store::V2Store::open(&p.store).unwrap(),
            "o",
            "r",
            "a.rs",
            b"fn a() {}",
            None,
        )
        .unwrap();
        let e = plan(&p, &join, Some(2)).unwrap_err().to_string();
        assert!(e.contains("--accept-snapshot-overwrite"), "{e}");
        let mut spec = JoinSpec::new("h:1");
        spec.accept_snapshot_overwrite = true;
        let pl = plan(&p, &InitMode::Join(spec.clone()), Some(2)).unwrap();
        assert!(pl.overwrite);
        let to = move_aside(&p).unwrap();
        assert!(to.join("graph.redb").exists() && !p.store.exists());
        assert!(p.is_empty());
        let pl = plan(&p, &InitMode::Join(spec), Some(2)).unwrap();
        assert!(!pl.overwrite, "nothing left to move");
        json(2).write(p.node_json.as_ref().unwrap()).unwrap();
        let pl = plan(&p, &join, None).unwrap();
        assert!(pl.existing.is_some(), "an initialized directory restarts");
    }

    /// QA 1: the log and the store must agree once a node is initialized.
    #[test]
    fn a_lost_log_or_store_is_refused() {
        use crate::raft::log_store::LogProbe;
        let dir = Path::new("/d");
        let blank = LogProbe::default();
        let voted = LogProbe {
            exists: true,
            vote: true,
            entries: 3,
            purged: false,
        };
        // A lost (or blank) log beside a store that applied entries.
        let e = check_log_and_store(dir, blank, true, 5)
            .unwrap_err()
            .to_string();
        assert!(e.contains("vote twice") && e.contains("missing"), "{e}");
        let empty = LogProbe {
            exists: true,
            ..LogProbe::default()
        };
        let e = check_log_and_store(dir, empty, true, 5)
            .unwrap_err()
            .to_string();
        assert!(e.contains("empty"), "{e}");
        // A lost store beside a log with state.
        let e = check_log_and_store(dir, voted, false, 0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("store (graph.redb) is missing"), "{e}");
        // Consistent states.
        check_log_and_store(dir, voted, true, 5).unwrap();
        check_log_and_store(dir, blank, true, 0).unwrap();
        check_log_and_store(dir, blank, false, 0).unwrap();
    }

    /// Dev review 7: an uninitialized node adopts a cluster id only from
    /// an RPC that makes it a member (not a Vote); once it has one, the
    /// header is required and must match.
    #[test]
    fn cluster_ids_are_adopted_only_from_a_leader_and_then_required() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("node.json");
        let mut j = json(2);
        j.cluster_id = None;
        j.write(&path).unwrap();
        let id = ClusterIdentity::for_node(&path, j);
        // No header, no id: nothing to check.
        id.check_or_adopt(None, true).unwrap();
        // A Vote does not make this node a member.
        id.check_or_adopt(Some("A"), false).unwrap();
        assert_eq!(id.get(), None);
        // An AppendEntries does, and is persisted.
        id.check_or_adopt(Some("A"), true).unwrap();
        assert_eq!(id.get().as_deref(), Some("A"));
        assert_eq!(
            NodeJson::read(&path)
                .unwrap()
                .unwrap()
                .cluster_id
                .as_deref(),
            Some("A")
        );
        // Now the header is required and must match, for every RPC.
        for adopt in [true, false] {
            id.check_or_adopt(Some("A"), adopt).unwrap();
            let e = id.check_or_adopt(Some("B"), adopt).unwrap_err();
            assert!(e.contains("wrong cluster"), "{e}");
            let e = id.check_or_adopt(None, adopt).unwrap_err();
            assert!(e.contains("named none"), "{e}");
            let e = id.check_or_adopt(Some(""), adopt).unwrap_err();
            assert!(e.contains("named none"), "{e}");
        }
    }

    /// Dev review 13: node.json survives as either the old or the new
    /// file, and no temp file is left behind.
    #[test]
    fn node_json_is_written_durably_via_a_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("node.json");
        json(1).write(&path).unwrap();
        json(1).write(&path).unwrap();
        assert_eq!(NodeJson::read(&path).unwrap(), Some(json(1)));
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["node.json".to_string()]);
        // A node.json from before `bootstrapped` existed reads as false.
        let old = r#"{"node_id":1,"cluster_id":"c","advertise":"h:1","binary_version":"0",
            "protocol_version":1,"store_format_version":1,"extractors_hash":"x","created":0}"#;
        std::fs::write(&path, old).unwrap();
        assert!(!NodeJson::read(&path).unwrap().unwrap().bootstrapped);
    }

    #[test]
    fn hostname_ordinals() {
        assert_eq!(hostname_ordinal("memory-graph-0"), Ok(0));
        assert_eq!(hostname_ordinal("memory-graph-12"), Ok(12));
        assert_eq!(hostname_ordinal("mg-3.memory-graph.ns.svc"), Ok(3));
        assert_eq!(hostname_ordinal("mg-0-1"), Ok(1), "the last `-` counts");
        assert!(hostname_ordinal("memory-graph").is_err());
        assert!(hostname_ordinal("node-").is_err());
        assert!(hostname_ordinal("node-1a").is_err());
        let e = hostname_ordinal("mg-007").unwrap_err();
        assert!(e.contains("leading zero"), "{e}");
        assert!(hostname_ordinal("mg-00").is_err());
        let e = hostname_ordinal("mg-99999999999999999999999").unwrap_err();
        assert!(e.contains("too large"), "{e}");
        let e = hostname_ordinal(&format!("mg-{}", u64::MAX)).unwrap_err();
        assert!(e.contains("too large"), "{e}");
        assert_eq!(
            hostname_ordinal(&format!("mg-{}", u64::MAX - 1)),
            Ok(u64::MAX - 1)
        );
        let e = node_id_from_hostname("mg-007").unwrap_err().to_string();
        assert!(e.contains("leading zero"), "{e}");
        assert_eq!(
            sibling_addrs("mg-0.mg.ns.svc.cluster.local:7000", 3).unwrap(),
            [
                "mg-1.mg.ns.svc.cluster.local:7000",
                "mg-2.mg.ns.svc.cluster.local:7000"
            ]
        );
        assert_eq!(sibling_addrs("my-set-0:7", 2).unwrap(), ["my-set-1:7"]);
        assert!(sibling_addrs("mg-0:7", 1).unwrap().is_empty());
        assert!(sibling_addrs("mg-1.mg:7000", 3).is_err());
        assert!(sibling_addrs("-0:7", 3).is_err());
        assert!(sibling_addrs("mg-0", 3).is_err());
        assert_eq!(node_id_from_hostname("memory-graph-0").unwrap(), 1);
        assert_eq!(node_id_from_hostname("memory-graph-2").unwrap(), 3);
        let e = node_id_from_hostname("laptop").unwrap_err().to_string();
        assert!(e.contains("laptop"), "{e}");
    }

    #[test]
    fn default_advertise_replaces_wildcards() {
        let a: std::net::SocketAddr = "127.0.0.1:7".parse().unwrap();
        assert_eq!(default_advertise(a), "127.0.0.1:7");
        let w: std::net::SocketAddr = "0.0.0.0:7".parse().unwrap();
        assert!(default_advertise(w).ends_with(":7"));
        assert!(!default_advertise(w).starts_with("0.0.0.0"));
    }

    #[test]
    fn cluster_ids_are_128_bit_hex() {
        let a = mint_cluster_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, mint_cluster_id());
    }
}
