//! [`RaftNode`]: start the Raft node over the store (ADR 0004 D5-D7),
//! initialize a one-member cluster when asked (`--bootstrap`, or `--db`
//! mode's first start), resume from the log after, run the snapshot policy,
//! and answer the leader and membership questions the services ask.
use super::log_store::{AppendObserver, RedbLogStore};
use super::network::{FaultPlan, FaultyNetwork, GrpcNetwork, NetStats};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The suggested client back-off when no leader is known (`NoLeader`).
pub const NO_LEADER_RETRY_MS: u64 = 200;

/// How long a blocking `AddLearner` waits for the learner to catch up.
pub const ADD_LEARNER_CATCH_UP: Duration = Duration::from_secs(60);

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
    /// A leader that has heard from no quorum for this long while a
    /// proposal waits answers that proposal `NoLeader` (retryable), so a
    /// client's write deadline holds during a partition (issue #116).
    /// `None`: `election_max_ms` (by then a follower cut off from a leader
    /// would have started an election). Slow but healthy writes are not
    /// affected: heartbeats keep the quorum acknowledgement fresh however
    /// long an entry takes (`--quorum-loss-timeout`).
    pub quorum_loss_ms: Option<u64>,
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
            quorum_loss_ms: None,
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

impl RaftSettings {
    /// The quorum-loss window (see [`RaftSettings::quorum_loss_ms`]).
    pub fn quorum_loss_window(&self) -> Duration {
        Duration::from_millis(self.quorum_loss_ms.unwrap_or(self.election_max_ms))
    }

    /// Checks the timings. Always: `election_min_ms < election_max_ms` and
    /// `heartbeat_ms < election_min_ms` (or followers call elections while
    /// the leader is alive). For a cluster (`multi_member`, the
    /// `--data-dir` mode) also `3 * heartbeat_ms < election_min_ms`: the
    /// freshness lease ([`freshness_lease`]) is `election_min_ms - 2 *
    /// heartbeat_ms`, so it must outlast one heartbeat, or every local read
    /// says `stale_possible` (lease 0) or the flag flickers between
    /// heartbeats. A sole voter (`--db`) is always fresh, so that rule does
    /// not apply to it.
    pub fn validate(&self, multi_member: bool) -> Result<(), String> {
        if self.election_min_ms >= self.election_max_ms {
            return Err(format!(
                "election timeout min ({} ms) must be below election timeout max ({} ms)",
                self.election_min_ms, self.election_max_ms
            ));
        }
        if self.heartbeat_ms >= self.election_min_ms {
            return Err(format!(
                "heartbeat interval ({} ms) must be below election timeout min ({} ms), \
                 or followers call elections while the leader is alive",
                self.heartbeat_ms, self.election_min_ms
            ));
        }
        // openraft refreshes its "last quorum ack" metric at least every
        // 1.5 heartbeats, so a shorter window could fire on a healthy
        // leader between two refreshes.
        let q = self.quorum_loss_window().as_millis() as u64;
        if q < self.heartbeat_ms.saturating_mul(2) {
            return Err(format!(
                "quorum-loss timeout ({q} ms) must be at least twice the heartbeat interval \
                 ({} ms), or a healthy leader could answer NoLeader",
                self.heartbeat_ms
            ));
        }
        if multi_member && self.heartbeat_ms.saturating_mul(3) >= self.election_min_ms {
            return Err(format!(
                "heartbeat interval ({} ms) must be below a third of election timeout min \
                 ({} ms): the read freshness lease is election_min - 2 * heartbeat ({} ms) \
                 and must outlast one heartbeat, or local reads report stale_possible \
                 always or intermittently",
                self.heartbeat_ms,
                self.election_min_ms,
                freshness_lease(self).as_millis()
            ));
        }
        Ok(())
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
    /// Test-only: the log lives on this storage backend, not in a file.
    pub storage_backend: Option<crate::powercut::BackendFactory>,
    /// This node's extractor version set hash (sent on every Raft RPC).
    pub extractors_hash: String,
    pub settings: RaftSettings,
    pub disk: DiskGuard,
    /// Test hooks.
    pub faults: Option<FaultPlan>,
    pub failpoints: SmFailpoints,
    pub append_observer: Option<AppendObserver>,
    /// Metrics and readiness inputs this node records (stage E).
    pub obs: Arc<crate::observe::Observability>,
    /// Testing only: see [`crate::raft::state_machine::TestingApplyGate`].
    #[doc(hidden)]
    pub testing_apply_gate: Option<crate::raft::state_machine::TestingApplyGate>,
}

#[derive(Clone)]
pub struct RaftNode {
    pub raft: Raft<TypeConfig>,
    pub log_store: RedbLogStore,
    pub node_id: NodeId,
    pub addr: String,
    pub snapshots: Arc<SnapshotDir>,
    pub disk: DiskGuard,
    /// Per-peer network outcomes (the last error, shown in `Status`).
    pub net_stats: NetStats,
    /// Test fault injection, also obeyed by the quorum-loss probe.
    pub faults: Option<FaultPlan>,
    /// Test hook ([`crate::server::TestingHooks::withhold_leader`]).
    pub withhold_leader: bool,
    /// The timings this node runs with.
    pub settings: RaftSettings,
    /// Set while this leader hands leadership over (`TransferLeader`): new
    /// proposals, membership changes and read barriers answer `NoLeader`
    /// (the client retries) so the log stops growing and the target, caught
    /// up, gets the old leader's vote. Taken with a compare-exchange: one
    /// transfer at a time.
    pub transferring: Arc<AtomicBool>,
    /// Proposals (writes and membership changes) between their check of
    /// `transferring` and their end: a transfer waits for zero after it
    /// set the flag, so no entry can be appended behind its back.
    pub in_flight: Arc<AtomicUsize>,
    /// Test hook ([`crate::server::TestingHooks::hold_proposal_ms`]): a
    /// write proposal waits this long after it was counted in
    /// [`in_flight`](Self::in_flight), before it reaches Raft.
    pub hold_proposal: Option<Duration>,
    /// Metrics and readiness inputs (apply timings, RPC timings, the last
    /// leader contact and the leader's committed index as heard over
    /// `AppendEntries`): what readiness (D10) and a `LOCAL` read's
    /// [`ReadMeta`](graph_proto::ReadMeta) (D8) both judge by.
    pub obs: Arc<crate::observe::Observability>,
    /// Linearizable reads on this node waiting to apply the leader's read
    /// index right now (a test observes a read parked there).
    pub read_index_waits: Arc<AtomicUsize>,
}

/// One proposal in flight ([`RaftNode::in_flight`]); leaves on drop.
pub struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The leader as this node knows it: id and advertised address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaderInfo {
    pub id: Option<NodeId>,
    pub addr: Option<String>,
}

/// Whether `node` (a leader with a write pending for `waited`) has lost
/// its quorum. Both must hold:
/// * some voter configuration (two during a joint change) lacks a
///   majority of voters that `answered` any RPC within the window (this
///   node counts): a heartbeat answer counts, so a slow transfer to a live
///   follower never makes it look gone;
/// * openraft's own quorum acknowledgement is `window` old (never one yet:
///   counted from the write's start).
///
/// Not while it follows (openraft answers the write itself), nor as the
/// sole voter (it is its own quorum).
pub fn quorum_lost(
    m: &RaftMetrics<NodeId, BasicNode>,
    node: NodeId,
    waited: Duration,
    window: Duration,
    answered: impl Fn(NodeId) -> bool,
) -> bool {
    if m.state != ServerState::Leader || m.current_leader != Some(node) {
        return false;
    }
    let mem = m.membership_config.membership();
    if mem.voter_ids().count() <= 1 {
        return false;
    }
    let reachable = mem.get_joint_config().iter().all(|set| {
        let up = set.iter().filter(|v| **v == node || answered(**v)).count();
        up > set.len() / 2
    });
    if reachable {
        return false;
    }
    match m.millis_since_quorum_ack {
        Some(ms) => Duration::from_millis(ms) >= window,
        None => waited >= window,
    }
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

/// How long a membership change waits for its entries to commit. A change
/// that cannot commit (a new voter that refuses every append, a lost
/// quorum) fails with this instead of holding the request forever; its
/// entry stays in the log and commits if the cluster recovers.
pub const MEMBERSHIP_COMMIT_WAIT: Duration = Duration::from_secs(60);

async fn committed<T>(
    change: impl std::future::Future<
        Output = Result<T, RaftError<NodeId, ClientWriteError<NodeId, BasicNode>>>,
    >,
) -> Result<T, StoreError> {
    match tokio::time::timeout(MEMBERSHIP_COMMIT_WAIT, change).await {
        Ok(r) => r.map_err(write_err),
        Err(_) => Err(StoreError::Rejected(format!(
            "the membership change did not commit within {MEMBERSHIP_COMMIT_WAIT:?} (a node \
             it adds does not accept the leader's log, or a quorum is unreachable); it may \
             still commit if the cluster recovers: check `cluster members`"
        ))),
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
        let mut log_store = RedbLogStore::open_with(
            &p.log_path,
            p.storage_backend.as_ref().map(|f| f(&p.log_path)),
        )?;
        log_store.set_observer(p.append_observer.clone());
        {
            let (slot, snaps, log) = (
                Arc::clone(&p.slot),
                Arc::clone(&p.snapshots),
                log_store.clone(),
            );
            tokio::task::spawn_blocking(move || repair_stale_snapshot(&slot, &snaps, &log))
                .await
                .map_err(fatal)??;
        }
        let sm = StoreStateMachine::new(Arc::clone(&p.slot), Arc::clone(&p.snapshots))
            .with_failpoints(p.failpoints)
            .with_obs(Arc::clone(&p.obs))
            .with_testing_apply_gate(p.testing_apply_gate.clone());
        let net_stats = NetStats::default();
        let net = GrpcNetwork::new(
            Arc::clone(&p.identity),
            &p.extractors_hash,
            net_stats.clone(),
        );
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
            net_stats,
            faults: p.faults,
            withhold_leader: false,
            settings: s,
            transferring: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            hold_proposal: None,
            read_index_waits: Arc::default(),
            obs: p.obs,
        };
        // The metrics are published by the Raft task, so right after
        // `Raft::new`/`initialize` they may not show the membership yet:
        // wait for it on an initialized node before asking `sole_voter`,
        // or a single node could serve before it knows it leads.
        if node.raft.is_initialized().await.map_err(fatal)? {
            node.raft
                .wait(Some(Duration::from_secs(30)))
                .metrics(
                    |m| {
                        m.membership_config
                            .membership()
                            .voter_ids()
                            .next()
                            .is_some()
                    },
                    "membership loaded",
                )
                .await
                .map_err(|e| fatal(format!("waiting for the membership: {e}")))?;
        }
        if node.sole_voter() {
            let me = node.node_id;
            node.raft
                .wait(Some(Duration::from_secs(30)))
                .metrics(
                    |m| m.state == ServerState::Leader && m.current_leader == Some(me),
                    "single-voter leader",
                )
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

    /// Whether this node leads right now (as far as it knows; a proposal
    /// is what confirms it).
    pub fn is_leader(&self) -> bool {
        if self.withhold_leader {
            return false;
        }
        let m = self.metrics();
        m.state == ServerState::Leader && m.current_leader == Some(self.node_id)
    }

    /// The error a leader-only operation answers on another node:
    /// `NotLeader` naming the leader when one is known, else `NoLeader`.
    pub fn not_leader(&self) -> StoreError {
        let l = self.leader();
        match l.id {
            Some(_) if l.addr.is_some() => StoreError::NotLeader {
                leader_id: l.id,
                leader_addr: l.addr,
            },
            _ => StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            },
        }
    }

    /// `NoLeader` while a leadership transfer runs (or the test hook
    /// withholds the leader).
    pub fn no_leader_while_transferring(&self) -> Result<(), StoreError> {
        if self.withhold_leader || self.transferring.load(Ordering::SeqCst) {
            return Err(StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            });
        }
        Ok(())
    }

    /// Enter a proposal: counted in [`in_flight`](Self::in_flight) first,
    /// then refused (`NoLeader`) while a transfer runs. The count drops
    /// when the returned guard does.
    fn proposal(&self) -> Result<InFlight, StoreError> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let guard = InFlight(Arc::clone(&self.in_flight));
        self.no_leader_while_transferring()?;
        Ok(guard)
    }

    /// Snapshots this node installed from a leader since it started.
    pub fn snapshots_installed(&self) -> u64 {
        self.snapshots.installed()
    }

    /// Propose one command and wait for it to be applied on this node
    /// (ADR 0004 D7): returns the entry's response and its log index. The
    /// disk guard runs first (`RESOURCE_EXHAUSTED` on the wire).
    pub async fn propose(&self, req: LogRequest) -> Result<(LogResponse, u64), StoreError> {
        let _in_flight = self.proposal()?;
        if let Some(hold) = self.hold_proposal {
            tokio::time::sleep(hold).await;
        }
        // Only the node that would append checks its disk; a follower
        // answers `NotLeader` naming the leader, whatever its disk.
        if self.metrics().state == ServerState::Leader {
            self.disk.check("write")?;
        }
        let write = self.raft.client_write(req);
        tokio::pin!(write);
        tokio::select! {
            r = &mut write => match r {
                Ok(resp) => {
                    let index = resp.log_id().index;
                    Ok((resp.response().clone(), index))
                }
                Err(e) => Err(write_err(e)),
            },
            e = self.quorum_lost() => Err(e),
        }
    }

    /// Resolves (with `NoLeader`) once this node leads but has heard from
    /// no quorum for [`RaftSettings::quorum_loss_window`] (issue #116): a
    /// leader cut off from its majority cannot commit, and openraft would
    /// keep the proposal pending until the partition heals. The entry may
    /// still commit later (if this node keeps leading once healed); every
    /// write is idempotent on a retry (ADR 0004 D2/D7), so the client's
    /// retry is safe either way. Never resolves on a sole voter, or while
    /// the quorum answers, however long the write itself takes.
    /// Probe (see [`super::network::probe`]) every voter that answered
    /// nothing within `window`, in parallel, each within `timeout`; record
    /// the answers. Whether any answered. A test partition
    /// ([`FaultPlan`]) cuts probes as it cuts Raft RPCs.
    async fn probe_silent_voters(&self, window: Duration, timeout: Duration) -> bool {
        let m = self.raft.metrics().borrow().clone();
        let mem = m.membership_config.membership();
        let silent: Vec<(NodeId, String)> = mem
            .voter_ids()
            .filter(|id| *id != self.node_id && !self.net_stats.answered_within(*id, window))
            .filter(|id| {
                self.faults
                    .as_ref()
                    .is_none_or(|f| f.connected(self.node_id, *id))
            })
            .filter_map(|id| mem.get_node(&id).map(|n| (id, n.addr.clone())))
            .collect();
        let mut probes = tokio::task::JoinSet::new();
        for (id, addr) in silent {
            probes.spawn(async move { (id, super::network::probe(&addr, timeout).await) });
        }
        let mut any = false;
        while let Some(r) = probes.join_next().await {
            if let Ok((id, true)) = r {
                self.net_stats.probed(id);
                any = true;
            }
        }
        any
    }

    async fn quorum_lost(&self) -> StoreError {
        let window = self.settings.quorum_loss_window();
        let started = tokio::time::Instant::now();
        let mut rx = self.raft.metrics();
        let tick = Duration::from_millis(self.settings.heartbeat_ms.max(1));
        loop {
            let lost = {
                let m = rx.borrow_and_update();
                quorum_lost(&m, self.node_id, started.elapsed(), window, |id| {
                    self.net_stats.answered_within(id, window)
                })
            };
            // Before calling the quorum gone, ask the silent voters directly:
            // a slow transfer to a live peer also silences it (openraft
            // sends no heartbeat meanwhile). An answer counts for a window.
            if lost && self.probe_silent_voters(window, tick).await {
                continue;
            }
            if lost {
                tracing::warn!(
                    window_ms = window.as_millis() as u64,
                    "no quorum acknowledged this leader within the window; \
                     answering the pending write NoLeader"
                );
                return StoreError::NoLeader {
                    retry_after_ms: NO_LEADER_RETRY_MS,
                };
            }
            // Metrics change at least every 1.5 heartbeats (openraft's
            // tick); the sleep also bounds the wait when they do not.
            let _ = tokio::time::timeout(tick, rx.changed()).await;
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
        let in_flight = self.proposal()?;
        // openraft's own `blocking` waits only its default half second and
        // then answers success whatever the learner's state, so the wait
        // for catch-up is ours, with a real timeout and a real error.
        let r = committed(self.raft.add_learner(id, BasicNode::new(addr), false)).await?;
        let index = r.log_id().index;
        // The entry is committed; the catch-up wait below appends nothing,
        // so it must not hold up a transfer's drain.
        drop(in_flight);
        if blocking && id != self.node_id {
            self.raft
                .wait(Some(ADD_LEARNER_CATCH_UP))
                .metrics(
                    |m| {
                        m.replication
                            .as_ref()
                            .and_then(|r| r.get(&id))
                            .and_then(|l| l.as_ref())
                            .is_some_and(|l| l.index >= index)
                    },
                    "the new learner caught up",
                )
                .await
                .map_err(|e| {
                    StoreError::Rejected(format!(
                        "node {id} at {addr} was added as a learner (log index {index}) but did \
                         not catch up within {ADD_LEARNER_CATCH_UP:?}: {e}"
                    ))
                })?;
        }
        Ok(index)
    }

    /// Make exactly `voters` the voters (joint consensus, learners kept).
    /// Returns the final membership entry's index.
    pub async fn change_membership(&self, voters: BTreeSet<NodeId>) -> Result<u64, StoreError> {
        let _in_flight = self.proposal()?;
        let r = committed(self.raft.change_membership(voters, true)).await?;
        Ok(r.log_id().index)
    }

    /// Make `id` (already a learner) a voter, keeping every other member
    /// as it is at the moment the change is applied: an add, not a
    /// replacement of the voter set, so two promotes racing each other
    /// both take effect. Returns the final membership entry's index.
    pub async fn promote(&self, id: NodeId) -> Result<u64, StoreError> {
        let _in_flight = self.proposal()?;
        let r = committed(self.raft.change_membership(
            openraft::ChangeMembers::AddVoterIds(BTreeSet::from([id])),
            true,
        ))
        .await?;
        Ok(r.log_id().index)
    }

    /// Remove `id` from the membership (joint consensus for a voter, which
    /// is not kept as a learner). The guards are the caller's
    /// (`Admin.Remove`). Returns the final membership entry's index.
    pub async fn remove(&self, id: NodeId, voter: bool) -> Result<u64, StoreError> {
        let _in_flight = self.proposal()?;
        let ids = BTreeSet::from([id]);
        let change = if voter {
            openraft::ChangeMembers::RemoveVoters(ids)
        } else {
            openraft::ChangeMembers::RemoveNodes(ids)
        };
        let r = committed(self.raft.change_membership(change, false)).await?;
        Ok(r.log_id().index)
    }

    /// Replace member `id`'s address with `addr` (`--update-advertise`):
    /// one membership entry, voters and learners unchanged
    /// (`ChangeMembers::SetNodes`; every replication stream is rebuilt with
    /// the new address). The guards are the caller's
    /// (`Admin.UpdateAdvertise`). Returns the entry's index.
    pub async fn set_node_addr(&self, id: NodeId, addr: &str) -> Result<u64, StoreError> {
        let _in_flight = self.proposal()?;
        let r = committed(self.raft.change_membership(
            openraft::ChangeMembers::SetNodes(std::collections::BTreeMap::from([(
                id,
                BasicNode::new(addr),
            )])),
            false,
        ))
        .await?;
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
    /// the read index, which it returns (a follower's read waits until it
    /// applied that index: `Admin.ReadIndex`).
    pub async fn ensure_linearizable(&self) -> Result<u64, StoreError> {
        self.no_leader_while_transferring()?;
        match self.raft.ensure_linearizable().await {
            Ok(read) => Ok(read.map_or(0, |l| l.index)),
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

    /// How fresh a `LOCAL` read on this node is (ADR 0004 D8), a
    /// best-effort hint (see [`freshness_lease`]). The leader is fresh
    /// while a quorum acknowledged it within the lease (a sole voter always
    /// is); a follower while it heard from a leader within the lease and
    /// has applied the commit index that leader sent. No known leader:
    /// stale possible.
    pub fn read_meta(&self) -> graph_proto::ReadMeta {
        let m = self.metrics();
        let applied = m.last_applied.map_or(0, |l| l.index);
        let lease = freshness_lease(&self.settings);
        let known = !self.withhold_leader && m.current_leader.is_some();
        if known && m.state == ServerState::Leader && m.current_leader == Some(self.node_id) {
            let voters = m.membership_config.membership().voter_ids().count();
            let acked = leader_within_lease(voters, m.millis_since_quorum_ack, lease);
            return graph_proto::ReadMeta {
                applied_index: applied,
                leader_committed_index: Some(applied),
                stale_possible: !acked || self.transferring.load(Ordering::SeqCst),
            };
        }
        let last = self.obs.last_leader_contact();
        let committed = last.and_then(|(_, c)| c);
        let recent = last.is_some_and(|(at, _)| at.elapsed() < lease);
        graph_proto::ReadMeta {
            applied_index: applied,
            leader_committed_index: committed,
            stale_possible: follower_stale(known, recent, applied, committed),
        }
    }

    pub async fn shutdown(&self) {
        if let Err(e) = self.raft.shutdown().await {
            tracing::warn!(error = %e, "raft shutdown");
        }
        // A background compaction after a purge keeps `raft.redb` open;
        // wait for it so a restart in this process can reopen the file.
        let log = self.log_store.clone();
        let drained = tokio::task::spawn_blocking(move || {
            log.wait_compaction(super::log_store::COMPACT_DRAIN)
        })
        .await
        .unwrap_or(false);
        if !drained {
            tracing::warn!("raft shutdown: the log compaction did not end in time");
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
///
/// It re-evaluates on every metrics change and at least every
/// [`POLICY_TICK`], so a build the disk guard refused is retried once space
/// frees up even on an idle cluster; a triggered build that never produced
/// a snapshot (openraft logs a failed build and carries on) is retried
/// after [`SNAPSHOT_RETRY`]. The byte trigger counts from what the log held
/// above the snapshot at start, so a restart does not reset it.
async fn snapshot_policy(node: RaftNode, s: RaftSettings) {
    let mut rx = node.raft.metrics();
    let entries = s.snapshot_log_entries.max(1);
    let mut last_snap = rx.borrow().snapshot.map(|l| l.index);
    let mut base_bytes = {
        let log = node.log_store.clone();
        let snap = last_snap.unwrap_or(0);
        let above = tokio::task::spawn_blocking(move || log.bytes_after(snap))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        node.log_store.appended_bytes().saturating_sub(above)
    };
    let mut pending: Option<(u64, std::time::Instant)> = None;
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
            if pending.is_some_and(|(p, _)| snap.unwrap_or(0) >= p) {
                pending = None;
            }
        }
        if pending.is_some_and(|(_, at)| at.elapsed() >= SNAPSHOT_RETRY) {
            tracing::warn!("a triggered snapshot build produced no snapshot; retrying");
            pending = None;
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
                    pending = Some((applied, std::time::Instant::now()));
                }
                Err(e) => {
                    if !disk_refused {
                        tracing::warn!(error = %e, "snapshot postponed");
                        disk_refused = true;
                    }
                }
            }
        }
        tokio::select! {
            r = rx.changed() => if r.is_err() { return },
            _ = tokio::time::sleep(POLICY_TICK) => {}
        }
    }
}

/// The snapshot policy re-evaluates at least this often.
pub const POLICY_TICK: Duration = Duration::from_secs(1);

/// A triggered build that produced no snapshot is retried after this.
pub const SNAPSHOT_RETRY: Duration = Duration::from_secs(30);

/// Start-up repair of a crash between a snapshot install's store swap and
/// the promotion of the received file to the current snapshot (ADR 0004
/// D7): the store is then at the installed index N while the current
/// snapshot is an older M, and the log does not hold the entries between
/// (they came in the snapshot, never as entries). openraft would take M as
/// this node's snapshot while its applied state is N; a laggard it later
/// leads would get M and then need entries this node never had, forever.
/// So when the store's marker is above the current snapshot and the log
/// does not reach the marker, a snapshot of the store is built now, before
/// openraft starts. Returns whether it built one.
pub fn repair_stale_snapshot(
    slot: &StoreSlot,
    snaps: &SnapshotDir,
    log: &RedbLogStore,
) -> Result<bool, StoreError> {
    let marker = slot.with_store(|s| s.raft_marker())?.map_or(0, |m| m.index);
    let snap = snaps.current().map_or(0, |(s, _)| s.index);
    if marker == 0 || snap >= marker {
        return Ok(false);
    }
    let log_last = log.last_index()?.unwrap_or(0);
    if log_last >= marker {
        return Ok(false);
    }
    tracing::warn!(
        marker,
        snapshot = snap,
        log_last,
        "the store is ahead of the current snapshot and the log (an interrupted snapshot \
         install); building a snapshot of the store before starting"
    );
    let started = std::time::Instant::now();
    snaps.build(slot)?;
    tracing::info!(
        marker,
        took_ms = started.elapsed().as_millis() as u64,
        "built a snapshot of the store at start-up"
    );
    Ok(true)
}

/// How long a leader's last quorum acknowledgement (or a follower's last
/// `AppendEntries` from its leader) keeps a `LOCAL` read "fresh"
/// (`stale_possible = false`, ADR 0004 D8): `election_timeout_min` minus
/// two heartbeats. openraft measures a leader's quorum ack from the send
/// time of the acknowledged request, and a follower that received it
/// neither campaigns nor grants a vote for at least
/// `election_timeout_min` after (openraft 0.9 is stricter still: a
/// follower of a known leader refuses votes for `election_timeout_max`
/// after its last heartbeat and campaigns only after that plus an election
/// timeout), so within the lease no other leader, and no write committed
/// elsewhere, can exist. The two-heartbeat margin covers the leader's
/// metrics being published once per core tick (every 1.5 heartbeats, so
/// the age read here can be that much low) plus clock drift; the
/// `election_timeout_min` base keeps the hint sound even if openraft's
/// vote lease were shortened. It stays a hint, not a
/// guarantee: only `--read linearizable` guarantees freshness (a follower
/// that still hears from a deposed leader cut off from the majority, for
/// one, stays "fresh" while it does).
pub fn freshness_lease(s: &RaftSettings) -> Duration {
    Duration::from_millis(
        s.election_min_ms
            .saturating_sub(s.heartbeat_ms.saturating_mul(2)),
    )
}

/// Whether a leader's quorum acknowledgement `millis_since_quorum_ack` ago
/// is within `lease` ([`freshness_lease`]); a sole voter always is.
pub fn leader_within_lease(
    voters: usize,
    millis_since_quorum_ack: Option<u64>,
    lease: Duration,
) -> bool {
    voters <= 1 || millis_since_quorum_ack.is_some_and(|ms| u128::from(ms) < lease.as_millis())
}

/// A follower's `stale_possible`: no known leader, no contact within the
/// lease, or the applied index below the leader's commit index as last
/// received.
pub fn follower_stale(known: bool, recent: bool, applied: u64, leader_commit: Option<u64>) -> bool {
    !known || !recent || leader_commit.is_some_and(|c| applied < c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leader_metrics(voters: &[NodeId], ack_ms: Option<u64>) -> RaftMetrics<NodeId, BasicNode> {
        let mut m = RaftMetrics::new_initial(1);
        m.state = ServerState::Leader;
        m.current_leader = Some(1);
        m.millis_since_quorum_ack = ack_ms;
        let nodes: BTreeMap<NodeId, BasicNode> = voters
            .iter()
            .map(|i| (*i, BasicNode::new(format!("h:{i}"))))
            .collect();
        let set: BTreeSet<NodeId> = voters.iter().copied().collect();
        m.membership_config = Arc::new(openraft::StoredMembership::new(
            None,
            openraft::Membership::new(vec![set], nodes),
        ));
        m
    }

    /// Issue #116: only a leader of several voters whose quorum has been
    /// silent for the window counts as having lost it.
    #[test]
    fn quorum_loss_needs_a_silent_quorum_for_the_window() {
        let w = Duration::from_millis(2000);
        let short = Duration::from_millis(10);
        let long = Duration::from_millis(5000);
        let none = |_| false;
        let all = |_| true;
        let only2 = |id| id == 2;
        let q = |ack, waited, answered: &dyn Fn(NodeId) -> bool| {
            quorum_lost(&leader_metrics(&[1, 2, 3], ack), 1, waited, w, answered)
        };
        // Acked recently: healthy, however long the write has waited.
        assert!(!q(Some(250), long, &none));
        // openraft's ack is stale but the peers answer heartbeats (a slow
        // transfer to live followers): healthy.
        assert!(!q(Some(5000), long, &all));
        // One peer of two answering is a majority with this node.
        assert!(!q(Some(5000), long, &only2));
        // Silent for the window, nobody answering: lost.
        assert!(q(Some(2000), short, &none));
        // Never acked: counted from the write's start.
        assert!(!q(None, short, &none));
        assert!(q(None, long, &none));
        // A sole voter is its own quorum.
        assert!(!quorum_lost(&leader_metrics(&[1], None), 1, long, w, none));
        // A follower: openraft answers the write itself.
        let mut f = leader_metrics(&[1, 2, 3], None);
        f.state = ServerState::Follower;
        f.current_leader = Some(2);
        assert!(!quorum_lost(&f, 1, long, w, none));
    }

    #[test]
    fn quorum_loss_window_defaults_to_election_max_and_is_validated() {
        let s = RaftSettings::cluster();
        assert_eq!(
            s.quorum_loss_window(),
            Duration::from_millis(s.election_max_ms)
        );
        let tight = RaftSettings {
            quorum_loss_ms: Some(s.heartbeat_ms),
            ..s
        };
        assert!(tight.validate(true).unwrap_err().contains("quorum-loss"));
        let ok = RaftSettings {
            quorum_loss_ms: Some(s.heartbeat_ms * 2),
            ..s
        };
        ok.validate(true).unwrap();
    }

    /// The lease ends before any other node can have been elected: below
    /// `election_timeout_min` by two heartbeats, for every shipped timing.
    #[test]
    fn freshness_lease_ends_before_an_election_can() {
        // `standalone` (`--db`) is a one-member cluster: its sole voter is
        // always fresh, whatever the lease.
        for s in [RaftSettings::cluster(), crate::testing::TEST_RAFT] {
            let lease = freshness_lease(&s);
            assert_eq!(
                lease,
                Duration::from_millis(s.election_min_ms - 2 * s.heartbeat_ms),
                "{s:?}"
            );
            assert!(lease.as_millis() < u128::from(s.election_min_ms), "{s:?}");
            // A healthy leader (acked by the last heartbeat) is inside it.
            assert!(lease > Duration::from_millis(s.heartbeat_ms), "{s:?}");
        }
        // Misconfigured timings: no lease, never "fresh" (the hint errs safe).
        let s = RaftSettings {
            heartbeat_ms: 600,
            election_min_ms: 1000,
            ..RaftSettings::cluster()
        };
        assert_eq!(freshness_lease(&s), Duration::ZERO);
    }

    /// Every timing `validate` accepts for a cluster has a lease longer
    /// than one heartbeat (exhaustive over a small grid); the shipped
    /// presets pass in their own modes.
    #[test]
    fn validated_cluster_settings_have_a_lease_above_one_heartbeat() {
        let mut accepted = 0;
        for heartbeat_ms in 0..=120u64 {
            for election_min_ms in 0..=300u64 {
                for election_max_ms in [election_min_ms, election_min_ms + 1, 1000] {
                    let s = RaftSettings {
                        heartbeat_ms,
                        election_min_ms,
                        election_max_ms,
                        ..RaftSettings::cluster()
                    };
                    if s.validate(true).is_ok() {
                        accepted += 1;
                        assert!(
                            freshness_lease(&s) > Duration::from_millis(heartbeat_ms),
                            "{s:?}"
                        );
                    }
                }
            }
        }
        assert!(accepted > 0);
        assert!(RaftSettings::cluster().validate(true).is_ok());
        assert!(crate::testing::TEST_RAFT.validate(true).is_ok());
        assert!(RaftSettings::standalone().validate(false).is_ok());
        // The boundary: 3 * heartbeat == election_min is refused.
        let edge = RaftSettings {
            heartbeat_ms: 200,
            election_min_ms: 600,
            election_max_ms: 1200,
            ..RaftSettings::cluster()
        };
        assert!(edge.validate(true).unwrap_err().contains("lease"));
        assert!(edge.validate(false).is_ok());
    }

    /// The window the review found: an ack older than the lease but younger
    /// than `election_timeout_max` (the old window) is stale now, including
    /// between `election_timeout_min` and the max, when another node may
    /// already lead.
    #[test]
    fn leader_is_stale_past_the_lease() {
        let s = crate::testing::TEST_RAFT;
        let lease = freshness_lease(&s);
        let ms = lease.as_millis() as u64;
        assert!(leader_within_lease(3, Some(0), lease));
        assert!(leader_within_lease(3, Some(ms - 1), lease));
        assert!(!leader_within_lease(3, Some(ms), lease));
        assert!(!leader_within_lease(3, Some(s.election_min_ms), lease));
        assert!(!leader_within_lease(3, Some(s.election_max_ms - 1), lease));
        assert!(!leader_within_lease(3, None, lease));
        // A sole voter cannot be deposed: always fresh.
        assert!(leader_within_lease(1, None, lease));
        assert!(leader_within_lease(1, Some(u64::MAX), lease));
    }

    #[test]
    fn follower_staleness_rules() {
        assert!(!follower_stale(true, true, 5, Some(5)));
        assert!(!follower_stale(true, true, 6, Some(5)));
        assert!(!follower_stale(true, true, 5, None));
        assert!(
            follower_stale(true, true, 4, Some(5)),
            "applied < leader_commit"
        );
        assert!(follower_stale(true, false, 5, Some(5)), "no recent contact");
        assert!(follower_stale(false, true, 5, Some(5)), "no known leader");
    }
}
