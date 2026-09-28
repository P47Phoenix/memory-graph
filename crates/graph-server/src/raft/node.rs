//! [`RaftNode`]: start the Raft node over the store (ADR 0004 D5-D7),
//! initialize a one-member cluster when asked (`--bootstrap`, or `--db`
//! mode's first start), resume from the log after, run the snapshot policy,
//! and answer the leader and membership questions the services ask.
use super::log_store::{AppendObserver, RedbLogStore};
use super::network::{FaultPlan, FaultyNetwork, GrpcNetwork};
use super::snapshot_dir::SnapshotDir;
use super::state_machine::{SmFailpoints, StoreStateMachine};
use super::types::{LogRequest, LogResponse, NodeId, TypeConfig};
use crate::disk::DiskGuard;
use crate::paths::ClusterIdentity;
use crate::slot::StoreSlot;
use graph_store::StoreError;
use openraft::error::{CheckIsLeaderError, ClientWriteError, RaftError};
use openraft::impls::BasicNode;
use openraft::{Config, Raft, RaftMetrics, ServerState, SnapshotPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

/// The suggested client back-off when no leader is known (`NoLeader`).
pub const NO_LEADER_RETRY_MS: u64 = 200;

/// Raft timing and log-retention knobs (the `serve` flags of ADR 0004
/// D5/D7). [`RaftSettings::cluster`] is the `--data-dir` default,
/// [`RaftSettings::standalone`] the stage A `--db` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaftSettings {
    /// Leader heartbeat, and openraft's per-`AppendEntries` timeout.
    pub heartbeat_ms: u64,
    pub election_min_ms: u64,
    pub election_max_ms: u64,
    /// Build a snapshot once this many entries were applied since the last
    /// one (`--snapshot-log-entries`, default 10 000).
    pub snapshot_log_entries: u64,
    /// ... or once this many log bytes were appended since the last one
    /// (`--snapshot-log-bytes`, default 1 GiB).
    pub snapshot_log_bytes: u64,
    /// Entries kept in the log below the snapshot (`--log-keep-entries`,
    /// default 1000), so a briefly lagging follower catches up from the
    /// log rather than by a snapshot.
    pub log_keep_entries: u64,
    /// Purge only once this many entries can go at once (default 1).
    pub purge_batch_size: u64,
    /// At most this many entries per `AppendEntries` (the byte cap in the
    /// network also applies).
    pub max_payload_entries: u64,
}

impl RaftSettings {
    /// `--data-dir` defaults: heartbeats every 250 ms, elections after
    /// 1-2 s of silence.
    pub const fn cluster() -> Self {
        Self {
            heartbeat_ms: 250,
            election_min_ms: 1000,
            election_max_ms: 2000,
            snapshot_log_entries: 10_000,
            snapshot_log_bytes: 1 << 30,
            log_keep_entries: 1000,
            // 1, not a bigger batch: an entry carries up to 8 MiB of
            // source, so waiting for 64 purgeable entries could keep
            // 512 MiB of log after a snapshot (a whole corpus index is a
            // handful of entries). A purge is one range delete.
            purge_batch_size: 1,
            max_payload_entries: 64,
        }
    }

    /// `--db` (stage A) defaults: one member elects itself at the first
    /// short timeout, so a (test) server starts quickly.
    pub const fn standalone() -> Self {
        Self {
            heartbeat_ms: 50,
            election_min_ms: 100,
            election_max_ms: 200,
            ..Self::cluster()
        }
    }
}

impl Default for RaftSettings {
    fn default() -> Self {
        Self::cluster()
    }
}

/// Everything [`RaftNode::start`] needs.
pub struct NodeStart {
    pub node_id: NodeId,
    /// Advertised address (`BasicNode.addr` of this node).
    pub advertise: String,
    pub slot: Arc<StoreSlot>,
    pub log_path: std::path::PathBuf,
    pub snapshots: Arc<SnapshotDir>,
    pub identity: Arc<ClusterIdentity>,
    /// Initialize a one-member cluster if the log is not initialized yet.
    pub initialize: bool,
    pub settings: RaftSettings,
    pub disk: DiskGuard,
    /// Test hooks.
    pub faults: Option<FaultPlan>,
    pub failpoints: SmFailpoints,
    pub append_observer: Option<AppendObserver>,
}

#[derive(Clone)]
pub struct RaftNode {
    pub raft: Raft<TypeConfig>,
    pub log_store: RedbLogStore,
    pub node_id: NodeId,
    pub addr: String,
    pub snapshots: Arc<SnapshotDir>,
    pub disk: DiskGuard,
    /// Test hook ([`crate::server::TestingHooks::withhold_leader`]).
    pub withhold_leader: bool,
}

/// The leader as this node knows it: id and advertised address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaderInfo {
    pub id: Option<NodeId>,
    pub addr: Option<String>,
}

fn fatal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Storage(format!("raft: {e}"))
}

fn write_err(e: RaftError<NodeId, ClientWriteError<NodeId, BasicNode>>) -> StoreError {
    match e {
        RaftError::APIError(ClientWriteError::ForwardToLeader(f)) => StoreError::NotLeader {
            leader_id: f.leader_id,
            leader_addr: f.leader_node.map(|n| n.addr),
        },
        RaftError::APIError(ClientWriteError::ChangeMembershipError(e)) => {
            StoreError::Rejected(format!("membership change: {e}"))
        }
        RaftError::Fatal(e) => fatal(e),
    }
}

impl RaftNode {
    /// Open the log, spawn the Raft task, initialize a one-member cluster
    /// when asked and the log is fresh, and, when this node is the only
    /// voter, wait until it leads (so a single node serves writes at once).
    pub async fn start(p: NodeStart) -> Result<Self, StoreError> {
        let s = p.settings;
        let config = Config {
            cluster_name: "memory-graph".into(),
            heartbeat_interval: s.heartbeat_ms,
            election_timeout_min: s.election_min_ms,
            election_timeout_max: s.election_max_ms,
            // The policy task below triggers snapshots (entries or bytes
            // since the last one, after the disk guard), so openraft's own
            // policy is off.
            snapshot_policy: SnapshotPolicy::Never,
            max_in_snapshot_log_to_keep: s.log_keep_entries,
            purge_batch_size: s.purge_batch_size.max(1),
            max_payload_entries: s.max_payload_entries.max(1),
            // Our `full_snapshot` streams the whole file in one RPC and
            // races openraft's cancel signal instead of a per-chunk timer.
            install_snapshot_timeout: 60_000,
            ..Config::default()
        }
        .validate()
        .map_err(fatal)?;
        let mut log_store = RedbLogStore::open(&p.log_path)?;
        log_store.set_observer(p.append_observer.clone());
        let sm = StoreStateMachine::new(Arc::clone(&p.slot), Arc::clone(&p.snapshots))
            .with_failpoints(p.failpoints);
        let net = GrpcNetwork::new(Arc::clone(&p.identity));
        let config = Arc::new(config);
        let raft = match p.faults.clone() {
            Some(plan) => {
                let net = FaultyNetwork {
                    inner: net,
                    me: p.node_id,
                    plan,
                };
                Raft::new(p.node_id, config, net, log_store.clone(), sm).await
            }
            None => Raft::new(p.node_id, config, net, log_store.clone(), sm).await,
        }
        .map_err(fatal)?;
        if p.initialize && !raft.is_initialized().await.map_err(fatal)? {
            let mut members = BTreeMap::new();
            members.insert(p.node_id, BasicNode::new(&p.advertise));
            raft.initialize(members).await.map_err(fatal)?;
        }
        let node = Self {
            raft,
            log_store,
            node_id: p.node_id,
            addr: p.advertise,
            snapshots: p.snapshots,
            disk: p.disk,
            withhold_leader: false,
        };
        if node.sole_voter() {
            node.raft
                .wait(Some(Duration::from_secs(30)))
                .state(ServerState::Leader, "single-voter leader")
                .await
                .map_err(|e| fatal(format!("waiting to become leader: {e}")))?;
        }
        tokio::spawn(snapshot_policy(node.clone(), s));
        Ok(node)
    }

    /// Whether this node is the one and only voter.
    fn sole_voter(&self) -> bool {
        let m = self.metrics();
        let voters: Vec<NodeId> = m.membership_config.membership().voter_ids().collect();
        voters == [self.node_id]
    }

    pub fn metrics(&self) -> RaftMetrics<NodeId, BasicNode> {
        self.raft.metrics().borrow().clone()
    }

    pub fn leader(&self) -> LeaderInfo {
        if self.withhold_leader {
            return LeaderInfo::default();
        }
        let m = self.metrics();
        let id = m.current_leader;
        let addr = id.and_then(|id| {
            m.membership_config
                .membership()
                .get_node(&id)
                .map(|n| n.addr.clone())
        });
        LeaderInfo { id, addr }
    }

    /// Snapshots this node installed from a leader since it started.
    pub fn snapshots_installed(&self) -> u64 {
        self.snapshots.installed()
    }

    /// Propose one command and wait for it to be applied on this node
    /// (ADR 0004 D7): returns the entry's response and its log index. The
    /// disk guard runs first (`RESOURCE_EXHAUSTED` on the wire).
    pub async fn propose(&self, req: LogRequest) -> Result<(LogResponse, u64), StoreError> {
        if self.withhold_leader {
            return Err(StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            });
        }
        // Only the node that would append checks its disk; a follower
        // answers `NotLeader` naming the leader, whatever its disk.
        if self.metrics().state == ServerState::Leader {
            self.disk.check("write")?;
        }
        match self.raft.client_write(req).await {
            Ok(resp) => {
                let index = resp.log_id().index;
                Ok((resp.response().clone(), index))
            }
            Err(e) => Err(write_err(e)),
        }
    }

    /// Add `id` at `addr` as a learner (a membership log entry); with
    /// `blocking`, wait until it has caught up. Returns the entry's index.
    pub async fn add_learner(
        &self,
        id: NodeId,
        addr: &str,
        blocking: bool,
    ) -> Result<u64, StoreError> {
        let r = self
            .raft
            .add_learner(id, BasicNode::new(addr), blocking)
            .await
            .map_err(write_err)?;
        Ok(r.log_id().index)
    }

    /// Make exactly `voters` the voters (joint consensus, learners kept).
    /// Returns the final membership entry's index.
    pub async fn change_membership(&self, voters: BTreeSet<NodeId>) -> Result<u64, StoreError> {
        let r = self
            .raft
            .change_membership(voters, true)
            .await
            .map_err(write_err)?;
        Ok(r.log_id().index)
    }

    /// Build a snapshot now (after the disk guard) and wait until the
    /// current snapshot covers what was applied when asked; returns its
    /// `(index, term)`.
    pub async fn snapshot_now(&self, timeout: Duration) -> Result<(u64, u64), StoreError> {
        self.disk.check("snapshot")?;
        let applied = self.metrics().last_applied.map_or(0, |l| l.index);
        self.raft.trigger().snapshot().await.map_err(fatal)?;
        let m = self
            .raft
            .wait(Some(timeout))
            .metrics(
                |m| m.snapshot.is_some_and(|s| s.index >= applied),
                "snapshot covers the applied index",
            )
            .await
            .map_err(|e| fatal(format!("waiting for the snapshot: {e}")))?;
        let s = m.snapshot.unwrap_or_default();
        Ok((s.index, s.leader_id.term))
    }

    /// openraft's read barrier (ADR 0004 D8, `LINEARIZABLE`): confirms
    /// leadership with a quorum and waits for the applied index to reach
    /// the read index.
    pub async fn ensure_linearizable(&self) -> Result<(), StoreError> {
        if self.withhold_leader {
            return Err(StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            });
        }
        match self.raft.ensure_linearizable().await {
            Ok(_) => Ok(()),
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(f))) => {
                Err(StoreError::NotLeader {
                    leader_id: f.leader_id,
                    leader_addr: f.leader_node.map(|n| n.addr),
                })
            }
            Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(_))) => {
                Err(StoreError::NoLeader {
                    retry_after_ms: NO_LEADER_RETRY_MS,
                })
            }
            Err(RaftError::Fatal(e)) => Err(fatal(e)),
        }
    }

    pub async fn shutdown(&self) {
        if let Err(e) = self.raft.shutdown().await {
            tracing::warn!(error = %e, "raft shutdown");
        }
    }

    /// The log file's size.
    pub fn log_bytes(&self) -> u64 {
        self.log_store.file_bytes()
    }
}

/// The snapshot policy (ADR 0004 D7): build a snapshot once
/// `snapshot_log_entries` entries were applied or `snapshot_log_bytes` log
/// bytes appended since the last one, if the disk guard allows; openraft
/// then purges the log below it, keeping `log_keep_entries`. Ends when the
/// Raft node shuts down.
async fn snapshot_policy(node: RaftNode, s: RaftSettings) {
    let mut rx = node.raft.metrics();
    let entries = s.snapshot_log_entries.max(1);
    let mut last_snap = rx.borrow().snapshot.map(|l| l.index);
    let mut base_bytes = node.log_store.appended_bytes();
    let mut pending: Option<u64> = None;
    let mut disk_refused = false;
    loop {
        let (applied, snap, running) = {
            let m = rx.borrow();
            (
                m.last_applied.map_or(0, |l| l.index),
                m.snapshot.map(|l| l.index),
                m.running_state.is_ok(),
            )
        };
        if !running {
            return;
        }
        if snap != last_snap {
            last_snap = snap;
            base_bytes = node.log_store.appended_bytes();
            if pending.is_some_and(|p| snap.unwrap_or(0) >= p) {
                pending = None;
            }
        }
        let since = applied.saturating_sub(snap.unwrap_or(0));
        let bytes = node.log_store.appended_bytes().saturating_sub(base_bytes);
        let due = applied > 0 && (since >= entries || bytes >= s.snapshot_log_bytes);
        if due && pending.is_none() {
            match node.disk.check("snapshot") {
                Ok(()) => {
                    disk_refused = false;
                    tracing::info!(applied, since, bytes, "building a snapshot");
                    if node.raft.trigger().snapshot().await.is_err() {
                        return;
                    }
                    pending = Some(applied);
                }
                Err(e) => {
                    if !disk_refused {
                        tracing::warn!(error = %e, "snapshot postponed");
                        disk_refused = true;
                    }
                }
            }
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}
