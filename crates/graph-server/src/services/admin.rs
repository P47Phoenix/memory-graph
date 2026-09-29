//! `memory_graph.v1.Admin` (ADR 0004 D9/D10): `Status`, `SysInfo`,
//! `Compact` and `Shutdown` (stage A); `Members`, `Leader`,
//! `TriggerSnapshot` (stage B); membership administration with its guards
//! (stage C): `AddLearner`, `Promote`, `Remove`, `TransferLeader`, `Join`,
//! plus `TriggerElect` (the transfer's second half) and `ReadIndex` (a
//! follower's linearizable read barrier).
//!
//! Membership changes may be sent to any node: a follower forwards them to
//! the leader ([`crate::forward`]), which enforces the guards:
//!
//! * `AddLearner` / `Join`: the new node is asked who it is (`Status`) and
//!   refused when it is another node, belongs to another cluster
//!   (`WrongCluster`) or runs other extractors; `Join` also checks the
//!   store format and protocol versions, and refuses a node id that is a
//!   member at another address or already a voter.
//! * `Promote`: an unknown id, a voter, or a node whose extractor version
//!   set hash differs (asked again at promotion time) is refused.
//! * `Remove`: the leader itself ("transfer leadership first"), 3 voters
//!   down to 2 without `force`, and any removal for which the voters that
//!   are reachable now are fewer than a quorum of the new voter set or of
//!   the old one (joint consensus needs both), or no voter would be left,
//!   are refused.
//!
//! `TransferLeader` (openraft 0.9 has no transfer of its own): the leader
//! checks the target is a voter with a replication lag of zero, takes the
//! one transfer slot (a concurrent second transfer is refused), refuses new
//! writes and membership changes (`NoLeader`, the client retries), waits
//! until no proposal that passed its check earlier is still in flight, and
//! checks the target's lag again (an entry appended behind the transfer's
//! back leaves the target a shorter log, and nobody votes for it). It then
//! waits, best effort, for every reachable voter's appends to settle, stops
//! its heartbeats and its own elections, and asks the target to campaign
//! every [`TRANSFER_POLL`] (`Admin.TriggerElect` -> `Raft::trigger().elect()`)
//! until it leads or [`TRANSFER_WAIT`] passes. Voters grant the vote once
//! their leader lease (`election_timeout_max` since the last heartbeat or
//! acknowledged append) runs out, before any of them campaigns on its own
//! (the lease plus at least `election_timeout_min`), so the target, asking
//! every poll, wins. A campaigning target ignores requests for
//! [`ELECT_SETTLE`] after its own election starts, so the votes of a
//! campaign (each persisted with an fsync) can come back before the next.
//! A `TriggerElect` that fails or takes over [`TRIGGER_TIMEOUT`] ends the
//! transfer at once, well within a lease: heartbeats resume before any
//! follower's lease runs out, so a transfer to a dead target disturbs
//! nobody. Heartbeats, elections and writes are switched back on whatever
//! the outcome (a guard).
use super::{status, Ctx};
use crate::forward::{forward_error, Route};
use crate::raft::NodeId;
use crate::SERVER_VERSION;
use graph_proto::{pb, PROTOCOL_VERSION};
use graph_store::StoreError;
use openraft::ServerState;
use std::collections::BTreeSet;
use std::io::Read;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::{Request, Response, Status};

pub struct AdminService {
    pub ctx: Arc<Ctx>,
}

/// How long `TriggerSnapshot` waits for the build.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(600);

/// How long a membership change waits for the node it names to answer
/// `Status`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `TransferLeader` waits for the target to catch up before it
/// refuses.
pub const TRANSFER_CATCH_UP: Duration = Duration::from_secs(10);

/// How long `TransferLeader` keeps asking the target to campaign.
pub const TRANSFER_WAIT: Duration = Duration::from_secs(20);

/// How long the leader keeps trying to promote an `--auto-promote` joiner
/// (it waits for the joiner's lag to reach zero). The joiner asks again
/// while it is a learner, so a leader change does not lose the intent.
pub const AUTO_PROMOTE_WAIT: Duration = Duration::from_secs(3600);

pub(crate) fn role(s: ServerState) -> &'static str {
    match s {
        ServerState::Leader => "leader",
        ServerState::Follower => "follower",
        ServerState::Candidate => "candidate",
        ServerState::Learner => "learner",
        ServerState::Shutdown => "shutdown",
    }
}

fn rejected(msg: String) -> Status {
    status(StoreError::Rejected(msg))
}

/// Refuse a leader-only operation on another node (the forwarded case:
/// the client goes to the leader itself).
fn ensure_leader(ctx: &Ctx) -> Result<(), Status> {
    if ctx.raft.is_leader() {
        Ok(())
    } else {
        Err(status(ctx.raft.not_leader()))
    }
}

/// [`ensure_leader`] for a membership change: also `NoLeader` while a
/// leadership transfer runs here (the client retries, and reaches the new
/// leader).
fn ensure_leader_idle(ctx: &Ctx) -> Result<(), Status> {
    ensure_leader(ctx)?;
    ctx.raft.no_leader_while_transferring().map_err(status)
}

/// Ask the server at `addr` who it is (`Admin.Status`) and refuse
/// (`FAILED_PRECONDITION`) a server that is another node, belongs to
/// another cluster (`WrongCluster`), or runs other extractors;
/// `UNAVAILABLE` when it cannot be asked. Otherwise a foreign or dead
/// member would sit in the membership, and a leader would replicate to it
/// forever.
async fn probe_node(ctx: &Ctx, id: NodeId, addr: &str) -> Result<pb::StatusResponse, Status> {
    let uri = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    let st = async {
        let ch = tonic::transport::Endpoint::from_shared(uri)
            .map_err(|e| Status::invalid_argument(format!("bad address `{addr}`: {e}")))?
            .connect_timeout(PROBE_TIMEOUT)
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("cannot reach node {id} at {addr}: {e}")))?;
        pb::admin_client::AdminClient::with_interceptor(ch, graph_proto::SendVersion)
            .status(pb::StatusRequest {})
            .await
            .map(tonic::Response::into_inner)
            .map_err(|e| {
                Status::unavailable(format!(
                    "node {id} at {addr} did not answer Status: {}",
                    e.message()
                ))
            })
    };
    let st = tokio::time::timeout(PROBE_TIMEOUT, st)
        .await
        .map_err(|_| {
            Status::unavailable(format!(
                "node {id} at {addr} did not answer within {PROBE_TIMEOUT:?}"
            ))
        })??;
    if st.node_id != id {
        return Err(Status::failed_precondition(format!(
            "the server at {addr} is node {}, not node {id}",
            st.node_id
        )));
    }
    let ours = ctx.info.cluster_id();
    if !st.cluster_id.is_empty() && st.cluster_id != ours {
        return Err(status(StoreError::WrongCluster {
            expected: ours,
            found: st.cluster_id,
        }));
    }
    check_extractors(ctx, id, addr, &st.extractors_hash)?;
    Ok(st)
}

fn check_extractors(ctx: &Ctx, id: NodeId, addr: &str, theirs: &str) -> Result<(), Status> {
    if theirs != ctx.info.extractors_hash {
        return Err(Status::failed_precondition(format!(
            "node {id} at {addr} runs extractor version set `{theirs}`, this cluster `{}`: a \
             replica must extract identically (ADR 0004 D5)",
            ctx.info.extractors_hash
        )));
    }
    Ok(())
}

fn members_of(ctx: &Ctx) -> Vec<pb::Member> {
    let m = ctx.raft.metrics();
    let mem = m.membership_config.membership();
    let voters: BTreeSet<NodeId> = mem.voter_ids().collect();
    mem.nodes()
        .map(|(id, n)| pb::Member {
            node_id: *id,
            addr: n.addr.clone(),
            role: if voters.contains(id) {
                "voter".into()
            } else {
                "learner".into()
            },
            // Only this node's own hash is known here; the others are
            // checked (asked) at add, join and promotion time.
            extractors_hash: if *id == ctx.info.node_id {
                ctx.info.extractors_hash.clone()
            } else {
                String::new()
            },
        })
        .collect()
}

/// `Promote` on the leader, with its guards.
async fn promote_guarded(ctx: &Ctx, id: NodeId) -> Result<u64, Status> {
    ensure_leader_idle(ctx)?;
    let m = ctx.raft.metrics();
    let mem = m.membership_config.membership();
    let Some(node) = mem.get_node(&id) else {
        return Err(rejected(format!(
            "node {id} is not a member; add it as a learner first (`serve --join`, or \
             `cluster add-learner`)"
        )));
    };
    if mem.voter_ids().any(|v| v == id) {
        return Err(rejected(format!("node {id} is already a voter")));
    }
    let addr = node.addr.clone();
    // The promotion gate (ADR 0004 D9): asked now, not remembered from the
    // join, so a learner restarted with other extractors is caught.
    probe_node(ctx, id, &addr).await?;
    // An add of one voter (not "these are the voters"), so concurrent
    // promotes do not undo each other.
    ctx.raft.promote(id).await.map_err(status)
}

/// The voters (other than this leader) that are reachable right now: RPCs
/// to them succeed (no `last_error`) and something was matched.
fn reachable_voters(ctx: &Ctx, voters: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
    let m = ctx.raft.metrics();
    let last = m.last_log_index.unwrap_or(0);
    let repl = m.replication.clone().unwrap_or_default();
    voters
        .iter()
        .copied()
        .filter(|id| {
            if *id == ctx.info.node_id {
                return true;
            }
            let Some(Some(matched)) = repl.get(id) else {
                return false;
            };
            let lag = last.saturating_sub(matched.index);
            ctx.raft.net_stats.last_error(*id, lag).is_none()
        })
        .collect()
}

/// `Remove` on the leader, with its guards.
async fn remove_guarded(ctx: &Ctx, id: NodeId, force: bool) -> Result<u64, Status> {
    ensure_leader_idle(ctx)?;
    if id == ctx.info.node_id {
        return Err(rejected(format!(
            "node {id} is the leader; transfer leadership first (`cluster transfer-leader \
             <other voter>`), then remove it"
        )));
    }
    let m = ctx.raft.metrics();
    let mem = m.membership_config.membership();
    if mem.get_node(&id).is_none() {
        return Err(rejected(format!("node {id} is not a member")));
    }
    let voters: BTreeSet<NodeId> = mem.voter_ids().collect();
    if !voters.contains(&id) {
        // A learner: no quorum is affected.
        return ctx.raft.remove(id, false).await.map_err(status);
    }
    let after: BTreeSet<NodeId> = voters.iter().copied().filter(|v| *v != id).collect();
    if after.is_empty() {
        return Err(rejected(format!("removing node {id} would leave no voter")));
    }
    // Joint consensus: the change commits only with a quorum of the old
    // voter set AND one of the new set, so both are checked (the removed
    // node counts toward the old set when it answers).
    for (set, which) in [(&after, "remain"), (&voters, "vote on the change")] {
        let quorum = set.len() / 2 + 1;
        let up = reachable_voters(ctx, set);
        if up.len() < quorum {
            let down: Vec<NodeId> = set.difference(&up).copied().collect();
            return Err(rejected(format!(
                "removing node {id} would drop below quorum: {} voters would {which} \
                 ({set:?}), a quorum is {quorum}, and only {} of them are reachable now \
                 ({down:?} are not); bring them back first. --force does not override this. \
                 (A node that just came back may still show its last error for a moment; \
                 retry shortly.)",
                set.len(),
                up.len()
            )));
        }
    }
    if voters.len() == 3 && after.len() == 2 && !force {
        return Err(rejected(format!(
            "removing node {id} takes the cluster from 3 voters to 2, which tolerates no \
             failure (a quorum of 2 is 2); pass --force to do it anyway, or add a voter first"
        )));
    }
    ctx.raft.remove(id, true).await.map_err(status)
}

/// Holds the transfer slot (new writes and membership changes answer
/// `NoLeader`) and switches this node's own elections off (the transfer
/// switches its heartbeats off later); all undone when dropped, whatever
/// the outcome.
struct TransferGuard<'a> {
    ctx: &'a Ctx,
}

impl<'a> TransferGuard<'a> {
    /// Take the transfer; a second concurrent `TransferLeader` is refused.
    fn new(ctx: &'a Ctx) -> Result<Self, Status> {
        if ctx
            .raft
            .transferring
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(rejected(
                "a leadership transfer is already in progress; wait for it to end".into(),
            ));
        }
        ctx.raft.raft.runtime_config().elect(false);
        Ok(Self { ctx })
    }
}

impl Drop for TransferGuard<'_> {
    fn drop(&mut self) {
        let rc = self.ctx.raft.raft.runtime_config();
        rc.heartbeat(true);
        rc.elect(true);
        self.ctx.raft.transferring.store(false, Ordering::SeqCst);
    }
}

/// How long `TransferLeader` waits for the other voters' in-flight appends
/// to settle before it asks the target to campaign.
pub const TRANSFER_SETTLE: Duration = Duration::from_secs(5);

/// A transfer target that is campaigning ignores a new `TriggerElect` for
/// this long after the election it started (see `trigger_elect`): well
/// under `election_timeout_min`, so the target still campaigns before any
/// other follower's own timer fires.
pub const ELECT_SETTLE: Duration = Duration::from_millis(300);

/// How long one `TriggerElect` to the transfer target may take: well under
/// a leader lease (heartbeats are off meanwhile).
pub const TRIGGER_TIMEOUT: Duration = Duration::from_millis(500);

/// How often `TransferLeader` asks the target to campaign, and checks
/// whether it won.
pub const TRANSFER_POLL: Duration = Duration::from_millis(50);

/// Wait until `to` matched this leader's last log index (lag zero), or
/// refuse after [`TRANSFER_CATCH_UP`].
async fn wait_caught_up(ctx: &Ctx, to: NodeId) -> Result<(), Status> {
    ctx.raft
        .raft
        .wait(Some(TRANSFER_CATCH_UP))
        .metrics(
            |m| {
                let last = m.last_log_index;
                m.replication
                    .as_ref()
                    .and_then(|r| r.get(&to))
                    .is_some_and(|l| l.map(|l| l.index) == last)
            },
            "the transfer target caught up",
        )
        .await
        .map(|_| ())
        .map_err(|_| {
            rejected(format!(
                "node {to} did not catch up with the leader within {TRANSFER_CATCH_UP:?}; \
                 leadership stays here"
            ))
        })
}

/// `TransferLeader` on the leader (see the module doc).
async fn transfer_guarded(ctx: &Ctx, to: NodeId) -> Result<u64, Status> {
    ensure_leader(ctx)?;
    let me = ctx.info.node_id;
    if to == me {
        return Ok(me);
    }
    let m = ctx.raft.metrics();
    let mem = m.membership_config.membership();
    let Some(node) = mem.get_node(&to) else {
        return Err(rejected(format!("node {to} is not a member")));
    };
    if !mem.voter_ids().any(|v| v == to) {
        return Err(rejected(format!(
            "node {to} is a learner; promote it before transferring leadership to it"
        )));
    }
    let addr = node.addr.clone();
    wait_caught_up(ctx, to).await?;
    let _guard = TransferGuard::new(ctx)?;
    // A proposal that passed its check before the flag went up may still
    // append: wait until none is in flight (each ends once applied), so
    // the lag check below sees every entry this leader will append.
    let drained = Instant::now() + TRANSFER_CATCH_UP;
    while ctx.raft.in_flight.load(Ordering::SeqCst) > 0 {
        if Instant::now() >= drained {
            return Err(rejected(format!(
                "writes in flight did not finish within {TRANSFER_CATCH_UP:?}; leadership \
                 stays here"
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Re-check under the guard: no new write is accepted now, so what was
    // in flight when the first check passed must reach the target too.
    wait_caught_up(ctx, to).await?;
    // Every append a voter acknowledges renews leases (the followers', and
    // this leader's own through the quorum ack), and openraft re-sends an
    // append that timed out. So wait, best effort, until no append is in
    // flight to any reachable voter: then, with writes refused and
    // heartbeats off, nothing renews a lease.
    let voters: BTreeSet<NodeId> = ctx
        .raft
        .metrics()
        .membership_config
        .membership()
        .voter_ids()
        .collect();
    let settled = ctx
        .raft
        .raft
        .wait(Some(TRANSFER_SETTLE))
        .metrics(
            |m| {
                let last = m.last_log_index;
                let repl = m.replication.clone().unwrap_or_default();
                voters.iter().all(|v| {
                    *v == m.id
                        || repl.get(v).is_some_and(|l| l.map(|l| l.index) == last)
                        // Best effort: a voter whose last append failed
                        // counts as unreachable and is skipped, so one
                        // that has just recovered may be skipped too.
                        || ctx.raft.net_stats.last_error(*v, 1).is_some()
                })
            },
            "every reachable voter caught up",
        )
        .await
        .is_ok();
    if !settled {
        tracing::warn!("transfer: a voter still lags; transferring anyway");
    }
    if let Some(hold) = ctx.transfer_hold {
        // Test hook: the slot is held (writes refused, heartbeats still on)
        // this long before the transfer proper starts.
        tokio::time::sleep(hold).await;
    }
    tracing::info!(
        to,
        "transferring leadership: heartbeats off, asking the target to campaign"
    );
    ctx.raft.raft.runtime_config().heartbeat(false);
    let deadline = Instant::now() + TRANSFER_WAIT;
    loop {
        let mut client = ctx.fwd.admin_client(&addr)?;
        match tokio::time::timeout(
            TRIGGER_TIMEOUT,
            client.trigger_elect(pb::TriggerElectRequest {}),
        )
        .await
        {
            Ok(Ok(_)) => {}
            // The target cannot be asked: give up at once, well within one
            // lease, so the heartbeats resume (the guard) before any
            // follower's lease runs out and nobody else campaigns.
            Ok(Err(e)) => {
                return Err(rejected(format!(
                    "node {to} did not answer TriggerElect ({}); node {me} keeps leadership",
                    e.message()
                )))
            }
            Err(_) => {
                return Err(rejected(format!(
                    "node {to} did not answer TriggerElect within {TRIGGER_TIMEOUT:?}; node {me} \
                     keeps leadership"
                )))
            }
        }
        let won = ctx
            .raft
            .raft
            .wait(Some(TRANSFER_POLL))
            .metrics(|m| m.current_leader == Some(to), "the target leads")
            .await
            .is_ok();
        if won {
            tracing::info!(from = me, to, "leadership transferred");
            return Ok(to);
        }
        let now = ctx.raft.metrics().current_leader;
        if let Some(other) = now.filter(|l| *l != me && *l != to) {
            return Err(rejected(format!(
                "leadership moved to node {other} instead of node {to}; run the transfer again \
                 on the new leader"
            )));
        }
        if Instant::now() >= deadline {
            return Err(rejected(format!(
                "node {to} did not take over leadership within {TRANSFER_WAIT:?}; node {me} \
                 keeps it"
            )));
        }
    }
}

/// The leader's side of `--auto-promote`: promote `id` once its matched
/// log index equals this leader's last log index (lag zero), retrying a
/// refused change (another membership change in progress) until
/// [`AUTO_PROMOTE_WAIT`]. Ends early when this node stops leading, `id`
/// leaves the membership or becomes a voter. One task per node id.
pub fn spawn_auto_promote(ctx: Arc<Ctx>, id: NodeId) {
    {
        let mut set = ctx
            .auto_promoting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !set.insert(id) {
            return;
        }
    }
    tokio::spawn(async move {
        let deadline = Instant::now() + AUTO_PROMOTE_WAIT;
        let mut rx = ctx.raft.raft.metrics();
        loop {
            if ctx.shutdown.is_triggered() || !ctx.raft.is_leader() || Instant::now() >= deadline {
                break;
            }
            let (member, voter, caught_up) = {
                let m = rx.borrow();
                let mem = m.membership_config.membership();
                let matched = m
                    .replication
                    .as_ref()
                    .and_then(|r| r.get(&id))
                    .and_then(|l| l.map(|l| l.index));
                (
                    mem.get_node(&id).is_some(),
                    mem.voter_ids().any(|v| v == id),
                    matched.is_some() && matched == m.last_log_index,
                )
            };
            if !member || voter {
                break;
            }
            if caught_up {
                match promote_guarded(&ctx, id).await {
                    Ok(index) => {
                        tracing::info!(id, index, "auto-promoted the joiner to voter");
                        break;
                    }
                    Err(e) => tracing::warn!(id, error = %e.message(), "auto-promote; retrying"),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            tokio::select! {
                r = rx.changed() => if r.is_err() { break },
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
        ctx.auto_promoting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    });
}

/// `Join` on the leader.
async fn join_guarded(ctx: &Arc<Ctx>, r: pb::JoinRequest) -> Result<pb::JoinResponse, Status> {
    ensure_leader_idle(ctx)?;
    let (id, addr) = (r.node_id, r.advertise.clone());
    if id == 0 || addr.is_empty() {
        return Err(rejected(
            "Join needs a node id (>= 1) and an advertised address".into(),
        ));
    }
    check_extractors(ctx, id, &addr, &r.extractors_hash)?;
    if r.store_format_version != graph_store::SCHEMA_VERSION {
        return Err(status(StoreError::SchemaMismatch {
            found: r.store_format_version,
        }));
    }
    if r.protocol_version != PROTOCOL_VERSION {
        return Err(status(StoreError::Protocol(format!(
            "node {id} speaks protocol version {}, this cluster {PROTOCOL_VERSION}",
            r.protocol_version
        ))));
    }
    let m = ctx.raft.metrics();
    let mem = m.membership_config.membership();
    let mut log_index = 0;
    match mem.get_node(&id) {
        Some(n) if n.addr != addr => {
            return Err(rejected(format!(
                "node {id} is already a member at {}, not {addr}; remove it first (`cluster \
                 remove {id}`) or join with another --node-id",
                n.addr
            )))
        }
        Some(_) if mem.voter_ids().any(|v| v == id) => {
            return Err(rejected(format!(
                "node {id} is already a voter; a voter restarts from its data directory and \
                 never joins again (an empty directory under a voter's id would have lost its \
                 vote and log: remove the node first)"
            )))
        }
        // Already a learner at this address: a restarted joiner asking
        // again. Nothing to add.
        Some(_) => {}
        None if r.rejoin => {
            return Err(rejected(format!(
                "node {id} is not a member of this cluster (it was removed); it is not added \
                 back on its own re-join. Add it again with `cluster add-learner {id} {addr}`, \
                 or join it from an empty data directory"
            )))
        }
        None => {
            probe_node(ctx, id, &addr).await?;
            log_index = ctx
                .raft
                .add_learner(id, &addr, false)
                .await
                .map_err(status)?;
            tracing::info!(id, %addr, log_index, "added a joining node as a learner");
        }
    }
    if r.auto_promote {
        spawn_auto_promote(Arc::clone(ctx), id);
    }
    Ok(pb::JoinResponse {
        cluster_id: ctx.info.cluster_id(),
        members: members_of(ctx),
        leader_id: Some(ctx.info.node_id),
        log_index,
    })
}

/// Forward an admin request to the leader when this node is not it
/// (counted like a forwarded write), else fall through to the local body.
macro_rules! on_leader {
    ($self:ident, $req:ident, $method:ident) => {
        if let Route::Leader { addr, .. } = $self.ctx.fwd.route(&$self.ctx.raft, &$req)? {
            let deadline = $self
                .ctx
                .fwd
                .deadline($req.metadata(), crate::forward::FORWARD_ADMIN_TIMEOUT);
            let mut client = $self.ctx.fwd.admin_client(&addr)?;
            let resp = crate::forward::within(
                deadline,
                client.$method(crate::forward::Forwarder::request(
                    $req.into_inner(),
                    deadline,
                )),
            )
            .await
            .map_err(forward_error)?;
            $self.ctx.fwd.count();
            return Ok(Response::new(resp.into_inner()));
        }
    };
}

type SnapshotStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::TriggerSnapshotResponse, Status>> + Send>>;

#[tonic::async_trait]
impl pb::admin_server::Admin for AdminService {
    async fn status(
        &self,
        _req: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        let m = self.ctx.raft.metrics();
        let leader = self.ctx.raft.leader();
        let (marker, snapshots) = self
            .ctx
            .blocking(|slot| {
                slot.with_store(|s| {
                    Ok::<_, StoreError>((s.raft_marker()?, graph_store::Store::snapshot_stats(s)))
                })
            })
            .await?;
        let applied = m.last_applied.as_ref().map_or(0, |l| l.index);
        // What this node persisted as committed; never below what it
        // applied (apply only follows commit, and the save may lag).
        let committed = self
            .ctx
            .raft
            .log_store
            .committed_index()
            .unwrap_or(0)
            .max(applied);
        let last_log_index = m.last_log_index.unwrap_or(0);
        let replication = m
            .replication
            .as_ref()
            .map(|r| {
                r.iter()
                    .filter(|(id, _)| **id != self.ctx.info.node_id)
                    .map(|(id, matched)| {
                        let matched_index = matched.as_ref().map(|l| l.index);
                        let lag = last_log_index.saturating_sub(matched_index.unwrap_or(0));
                        pb::PeerLag {
                            node_id: *id,
                            matched_index,
                            lag,
                            last_error: self
                                .ctx
                                .raft
                                .net_stats
                                .last_error(*id, lag)
                                .unwrap_or_default(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let store_bytes = std::fs::metadata(self.ctx.slot.path())
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(Response::new(pb::StatusResponse {
            node_id: self.ctx.info.node_id,
            cluster_id: self.ctx.info.cluster_id(),
            leader_id: leader.id,
            leader_addr: leader.addr,
            state: format!("{:?}", m.state),
            current_term: m.current_term,
            applied_index: marker.map_or(0, |mk| mk.index),
            applied_term: marker.map_or(0, |mk| mk.term),
            committed_index: committed,
            last_log_index,
            server_version: SERVER_VERSION.into(),
            protocol_version: PROTOCOL_VERSION,
            store_format_version: graph_store::SCHEMA_VERSION,
            extractors_hash: self.ctx.info.extractors_hash.clone(),
            db_path: self.ctx.info.db_path.clone(),
            listen_addr: self.ctx.info.listen_addr.clone(),
            uptime_secs: self.ctx.info.started.elapsed().as_secs(),
            snapshots: Some(snapshots.into()),
            snapshot_handles: self.ctx.slot.snapshots().len() as u64,
            role: role(m.state).into(),
            snapshot_index: m.snapshot.map_or(0, |s| s.index),
            purged_index: m.purged.map_or(0, |p| p.index),
            members: members_of(&self.ctx),
            replication,
            log_bytes: self.ctx.raft.log_bytes(),
            store_bytes,
            data_dir: self.ctx.info.data_dir.clone(),
            advertise: self.ctx.info.advertise.clone(),
            writes_forwarded_total: self.ctx.fwd.forwarded_total(),
            rpcs_total: self.ctx.raft.obs.rpc_total(),
            entries_applied_total: self.ctx.raft.obs.apply_total(),
        }))
    }

    async fn sys_info(
        &self,
        _req: Request<pb::SysInfoRequest>,
    ) -> Result<Response<pb::SysInfoResponse>, Status> {
        let json = match &self.ctx.sysinfo {
            Some(f) => {
                let f = Arc::clone(f);
                let db = self.ctx.slot.path().to_path_buf();
                tokio::task::spawn_blocking(move || f(&db))
                    .await
                    .map_err(|e| Status::internal(format!("sysinfo task: {e}")))?
            }
            None => {
                let size = std::fs::metadata(self.ctx.slot.path())
                    .map(|m| m.len())
                    .unwrap_or(0);
                serde_json::json!({
                    "db": self.ctx.info.db_path,
                    "store_size_bytes": size,
                    "pid": std::process::id(),
                    "server_version": SERVER_VERSION,
                    "note": "no sysinfo provider configured on this server",
                })
            }
        };
        Ok(Response::new(pb::SysInfoResponse {
            json: json.to_string(),
        }))
    }

    async fn compact(
        &self,
        _req: Request<pb::CompactRequest>,
    ) -> Result<Response<pb::CompactResponse>, Status> {
        let stats = self.ctx.blocking(|slot| slot.compact()).await?;
        Ok(Response::new(pb::CompactResponse {
            stats: Some(stats.into()),
        }))
    }

    async fn shutdown(
        &self,
        req: Request<pb::ShutdownRequest>,
    ) -> Result<Response<pb::ShutdownResponse>, Status> {
        let grace = req.into_inner().grace_ms;
        tracing::info!(grace_ms = grace, "shutdown requested over Admin");
        self.ctx.shutdown.trigger();
        Ok(Response::new(pb::ShutdownResponse {}))
    }

    async fn members(
        &self,
        _req: Request<pb::MembersRequest>,
    ) -> Result<Response<pb::MembersResponse>, Status> {
        Ok(Response::new(pb::MembersResponse {
            members: members_of(&self.ctx),
            leader_id: self.ctx.raft.leader().id,
        }))
    }

    async fn leader(
        &self,
        _req: Request<pb::LeaderRequest>,
    ) -> Result<Response<pb::LeaderResponse>, Status> {
        let l = self.ctx.raft.leader();
        Ok(Response::new(pb::LeaderResponse {
            leader_id: l.id,
            leader_addr: l.addr,
        }))
    }

    async fn add_learner(
        &self,
        req: Request<pb::AddLearnerRequest>,
    ) -> Result<Response<pb::AddLearnerResponse>, Status> {
        on_leader!(self, req, add_learner);
        let r = req.into_inner();
        if r.node_id == 0 || r.addr.is_empty() {
            return Err(rejected(
                "AddLearner needs a node id (>= 1) and an address".into(),
            ));
        }
        ensure_leader_idle(&self.ctx)?;
        if let Some(n) = self
            .ctx
            .raft
            .metrics()
            .membership_config
            .membership()
            .get_node(&r.node_id)
        {
            if n.addr != r.addr {
                return Err(rejected(format!(
                    "node {} is already a member at {}, not {}",
                    r.node_id, n.addr, r.addr
                )));
            }
        }
        probe_node(&self.ctx, r.node_id, &r.addr).await?;
        let log_index = self
            .ctx
            .raft
            .add_learner(r.node_id, &r.addr, r.blocking)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::AddLearnerResponse { log_index }))
    }

    async fn promote(
        &self,
        req: Request<pb::PromoteRequest>,
    ) -> Result<Response<pb::PromoteResponse>, Status> {
        on_leader!(self, req, promote);
        let log_index = promote_guarded(&self.ctx, req.into_inner().node_id).await?;
        Ok(Response::new(pb::PromoteResponse { log_index }))
    }

    async fn remove(
        &self,
        req: Request<pb::RemoveRequest>,
    ) -> Result<Response<pb::RemoveResponse>, Status> {
        on_leader!(self, req, remove);
        let r = req.into_inner();
        let log_index = remove_guarded(&self.ctx, r.node_id, r.force).await?;
        tracing::info!(node = r.node_id, log_index, "removed a member");
        Ok(Response::new(pb::RemoveResponse { log_index }))
    }

    async fn transfer_leader(
        &self,
        req: Request<pb::TransferLeaderRequest>,
    ) -> Result<Response<pb::TransferLeaderResponse>, Status> {
        on_leader!(self, req, transfer_leader);
        let leader_id = transfer_guarded(&self.ctx, req.into_inner().to_node_id).await?;
        Ok(Response::new(pb::TransferLeaderResponse { leader_id }))
    }

    async fn join(
        &self,
        req: Request<pb::JoinRequest>,
    ) -> Result<Response<pb::JoinResponse>, Status> {
        on_leader!(self, req, join);
        Ok(Response::new(
            join_guarded(&self.ctx, req.into_inner()).await?,
        ))
    }

    async fn trigger_elect(
        &self,
        _req: Request<pb::TriggerElectRequest>,
    ) -> Result<Response<pb::TriggerElectResponse>, Status> {
        // openraft ignores it on a leader and on a learner. The transferring
        // leader asks every `TRANSFER_POLL`; a new election every time would
        // restart this node's campaign before the votes of the last one
        // (each persisted with an fsync on the voter) came back, so while
        // an election it started is younger than `ELECT_SETTLE` the request
        // is a no-op.
        {
            let mut last = self
                .ctx
                .last_elect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let campaigning = self.ctx.raft.metrics().state == ServerState::Candidate;
            if campaigning && last.is_some_and(|t| t.elapsed() < ELECT_SETTLE) {
                return Ok(Response::new(pb::TriggerElectResponse {}));
            }
            *last = Some(Instant::now());
        }
        tracing::info!("campaigning on request (a leadership transfer)");
        self.ctx
            .raft
            .raft
            .trigger()
            .elect()
            .await
            .map_err(|e| status(StoreError::Storage(format!("raft: {e}"))))?;
        Ok(Response::new(pb::TriggerElectResponse {}))
    }

    async fn read_index(
        &self,
        _req: Request<pb::ReadIndexRequest>,
    ) -> Result<Response<pb::ReadIndexResponse>, Status> {
        ensure_leader(&self.ctx)?;
        let read_index = self.ctx.raft.ensure_linearizable().await.map_err(status)?;
        Ok(Response::new(pb::ReadIndexResponse { read_index }))
    }

    type TriggerSnapshotStream = SnapshotStream;

    async fn trigger_snapshot(
        &self,
        req: Request<pb::TriggerSnapshotRequest>,
    ) -> Result<Response<Self::TriggerSnapshotStream>, Status> {
        let download = req.into_inner().download;
        self.ctx
            .raft
            .snapshot_now(SNAPSHOT_WAIT)
            .await
            .map_err(status)?;
        // Open before answering: a newer build may remove the pair while we
        // stream, and an open handle keeps the bytes readable. A build can
        // also replace the pair between listing it and opening it: then the
        // newer pair is current, so look again once.
        let mut opened = None;
        for _ in 0..2 {
            let (side, path) = self
                .ctx
                .raft
                .snapshots
                .current()
                .ok_or_else(|| Status::internal("the snapshot was built but is not on disk"))?;
            match std::fs::File::open(&path) {
                Ok(f) => {
                    opened = Some((side, f));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Status::internal(format!("opening the snapshot: {e}"))),
            }
        }
        let (side, file) = opened.ok_or_else(|| {
            Status::unavailable("the snapshot was replaced twice while opening it; retry")
        })?;
        let info = pb::SnapshotInfo {
            last_applied_index: side.index,
            last_applied_term: side.term,
            size: side.size,
            sha256: side.sha256.clone(),
            extractors_hash: side.extractors_hash.clone(),
            store_format_version: side.store_format_version,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let first = pb::TriggerSnapshotResponse {
                msg: Some(pb::trigger_snapshot_response::Msg::Info(info)),
            };
            if tx.blocking_send(Ok(first)).is_err() || !download {
                return;
            }
            let mut file = file;
            let mut buf = vec![0u8; crate::raft::network::SNAPSHOT_CHUNK_BYTES];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        let msg = pb::TriggerSnapshotResponse {
                            msg: Some(pb::trigger_snapshot_response::Msg::Chunk(buf[..n].to_vec())),
                        };
                        if tx.blocking_send(Ok(msg)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(Status::internal(format!(
                            "reading the snapshot: {e}"
                        ))));
                        return;
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn metrics(
        &self,
        _req: Request<pb::MetricsRequest>,
    ) -> Result<Response<pb::MetricsResponse>, Status> {
        Ok(Response::new(pb::MetricsResponse {
            text: crate::observe::render(&self.ctx),
        }))
    }
}
