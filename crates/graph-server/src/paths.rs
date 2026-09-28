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
//! state, or start as an uninitialized member that a leader adds.
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
    /// refused (pass `--bootstrap`, or in stage C `--join`).
    #[default]
    Restart,
    /// Start empty and wait to be added by a leader (`AddLearner`); the
    /// cluster id is adopted from the first leader that contacts this
    /// node. On a directory that already has a `node.json`, a restart.
    /// Stage C's `--join <peer>` builds on this.
    Uninitialized,
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
        });
    }
    if !paths.is_empty() && (matches!(init, InitMode::Restart) || !failed_first_start(paths)) {
        return Err(StoreError::Rejected(format!(
            "`{}` holds a store or a Raft log but no node.json; it was not created by \
             `serve --data-dir` (or node.json was lost). Refusing to guess its identity",
            dir.display()
        )));
    }
    let restore = match init {
        InitMode::Restart => {
            return Err(StoreError::Rejected(format!(
                "`{}` is not initialized: pass --bootstrap to create a new cluster \
                 (or, from stage C, --join <peer> to join one)",
                dir.display()
            )))
        }
        InitMode::Bootstrap { restore } => restore.clone(),
        InitMode::Uninitialized => None,
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
    })
}

/// Whether a data directory without `node.json` holds only what a first
/// start that failed before writing `node.json` leaves behind: a log with
/// no Raft state at all, and a store (if any) with no Raft marker and no
/// data. Such a directory is started as the empty one it effectively is
/// (a `--bootstrap` or uninitialized first start retried after, say, the
/// port was in use); anything else is refused, because its identity cannot
/// be known.
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
    let tmp = with_suffix(store, ".restore.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(snapshot, &tmp)
        .map_err(|e| StoreError::Storage(format!("copying `{}`: {e}", snapshot.display())))?;
    let stripped = graph_store::V2Store::open(&tmp).and_then(|s| s.clear_raft_state());
    if let Err(e) = stripped {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, store)
        .map_err(|e| StoreError::Storage(format!("placing `{}`: {e}", store.display())))
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
