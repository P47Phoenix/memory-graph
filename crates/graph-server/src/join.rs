//! The joiner's side of `serve --join <peer>` (ADR 0004 D6/D9).
//!
//! * First start (no cluster id yet): once this node serves (the leader
//!   asks it who it is before adding it), send `Admin.Join` to the peer,
//!   which forwards it to the leader; retry with back-off while no leader
//!   answers (no leader yet, the peer unreachable, a `NotLeader` naming the
//!   leader) until `--join-timeout`; any refusal (other extractors, another
//!   store format, the node id taken) fails the start at once. The answer
//!   carries the cluster id, adopted and written to `node.json`; the leader
//!   has added this node as a learner and catches it up by log or snapshot.
//! * Restart (a cluster id in `node.json`): before anything is opened, the
//!   peer is asked for its cluster id ([`check_peer_cluster`]); another
//!   cluster is refused with `WrongCluster`, an unreachable peer only
//!   warned about (a whole cluster restarting at once must not wait for
//!   the peer it happened to name).
//! * `--auto-promote`: the leader promotes the node once its lag is zero
//!   (a task on the leader). While this node is still a learner it sends
//!   `Join` again every [`REJOIN_INTERVAL`] ([`spawn_rejoin`]), so neither
//!   a restart of the joiner nor a change of leader loses the intent (the
//!   leader keeps one promotion task per node). Such a re-send is marked
//!   `rejoin`: a leader that no longer lists the node (it was removed, and
//!   a removed node may never see the entry that removed it) refuses it
//!   rather than adding it back, and the joiner stops asking (a warning).
use crate::paths::{ClusterIdentity, JoinSpec};
use crate::raft::{NodeId, RaftNode};
use crate::server::ShutdownHandle;
use graph_proto::error::WireError;
use graph_proto::{pb, SendVersion, PROTOCOL_VERSION};
use graph_store::StoreError;
use std::time::{Duration, Instant};
use tonic::Code;

/// How often a learner that asked for `--auto-promote` asks again.
pub const REJOIN_INTERVAL: Duration = Duration::from_secs(2);

/// One `Join` attempt (the leader probes this node and commits a
/// membership entry) may take this long.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the restart check waits for the peer's cluster id.
const PEER_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn uri(addr: &str) -> String {
    if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

pub(crate) async fn admin(
    addr: &str,
    connect_timeout: Duration,
) -> Result<
    pb::admin_client::AdminClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, SendVersion>,
    >,
    tonic::Status,
> {
    let ch = tonic::transport::Endpoint::from_shared(uri(addr))
        .map_err(|e| tonic::Status::invalid_argument(format!("bad address `{addr}`: {e}")))?
        .connect_timeout(connect_timeout)
        .connect()
        .await
        .map_err(|e| tonic::Status::unavailable(format!("cannot reach {addr}: {e}")))?;
    Ok(pb::admin_client::AdminClient::with_interceptor(
        ch,
        SendVersion,
    ))
}

/// Restart with `--join`: refuse (`WrongCluster`) a peer of another
/// cluster than `mine`, the one recorded in `node.json`. An unreachable
/// peer (or one that has no cluster id yet) is not a refusal.
pub async fn check_peer_cluster(peer: &str, mine: &str) -> Result<(), StoreError> {
    let asked = tokio::time::timeout(PEER_CHECK_TIMEOUT, async {
        admin(peer, PEER_CHECK_TIMEOUT)
            .await?
            .status(pb::StatusRequest {})
            .await
            .map(tonic::Response::into_inner)
    })
    .await;
    match asked {
        Ok(Ok(st)) if !st.cluster_id.is_empty() && st.cluster_id != mine => {
            Err(StoreError::WrongCluster {
                expected: st.cluster_id,
                found: mine.to_string(),
            })
        }
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(peer, error = %e.message(), "--join: the peer did not answer; restarting from persisted state");
            Ok(())
        }
        Err(_) => {
            tracing::warn!(
                peer,
                "--join: the peer did not answer in time; restarting from persisted state"
            );
            Ok(())
        }
    }
}

/// What a `Join` attempt's failure means for the loop.
pub(crate) enum Next {
    /// Try again (after a back-off), at this address.
    Retry(String),
    /// A refusal: give up.
    Fail(StoreError),
}

pub(crate) fn classify(current: &str, st: &tonic::Status) -> Next {
    if st.details().is_empty()
        && (st.code() == Code::Unavailable
            || graph_proto::error::is_transport_loss(st.code(), st.message()))
    {
        return Next::Retry(current.to_string());
    }
    match WireError::from(st) {
        WireError::NotLeader {
            leader_addr: Some(addr),
            ..
        } => Next::Retry(addr),
        WireError::NotLeader { .. } | WireError::NoLeader { .. } => {
            Next::Retry(current.to_string())
        }
        WireError::Store(StoreError::Locked(_)) => Next::Retry(current.to_string()),
        other => Next::Fail(other.into()),
    }
}

/// The `Join` request of this node.
pub fn join_request(
    node_id: NodeId,
    advertise: &str,
    extractors_hash: &str,
    auto_promote: bool,
) -> pb::JoinRequest {
    pb::JoinRequest {
        node_id,
        advertise: advertise.to_string(),
        extractors_hash: extractors_hash.to_string(),
        store_format_version: graph_store::SCHEMA_VERSION,
        protocol_version: PROTOCOL_VERSION,
        auto_promote,
        rejoin: false,
    }
}

/// The first join: `Join` through `spec.peer` until a leader accepts it or
/// `spec.timeout` passes; adopts the cluster id it answers.
pub async fn join(
    spec: &JoinSpec,
    req: pb::JoinRequest,
    identity: &ClusterIdentity,
) -> Result<pb::JoinResponse, StoreError> {
    let deadline = Instant::now() + spec.timeout;
    let mut target = spec.peer.clone();
    let mut delay = Duration::from_millis(100);
    let mut last: String;
    loop {
        let attempt = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
            admin(&target, Duration::from_secs(5))
                .await?
                .join(req.clone())
                .await
                .map(tonic::Response::into_inner)
        })
        .await;
        match attempt {
            Ok(Ok(resp)) => {
                adopt(identity, &resp.cluster_id)?;
                tracing::info!(
                    peer = %spec.peer,
                    cluster_id = %resp.cluster_id,
                    leader = ?resp.leader_id,
                    "joined the cluster as a learner"
                );
                return Ok(resp);
            }
            Ok(Err(st)) => match classify(&target, &st) {
                Next::Fail(e) => {
                    return Err(match e {
                        e @ (StoreError::WrongCluster { .. }
                        | StoreError::Protocol(_)
                        | StoreError::SchemaMismatch { .. }) => e,
                        e => StoreError::Rejected(format!(
                            "joining the cluster through {} was refused: {e}",
                            spec.peer
                        )),
                    })
                }
                Next::Retry(next) => {
                    last = st.message().to_string();
                    if next != target {
                        tracing::info!(from = %target, to = %next, "--join: following the leader");
                        target = next;
                        delay = Duration::from_millis(100);
                    }
                }
            },
            Err(_) => last = format!("no answer within {ATTEMPT_TIMEOUT:?}"),
        }
        if Instant::now() + delay > deadline {
            return Err(StoreError::Rejected(format!(
                "could not join the cluster through {} within {:?} (--join-timeout): no leader \
                 accepted the request; last error: {last}. If the leader added node {} as a \
                 learner before the answer was lost, it stays in the membership as a learner \
                 that never runs: remove it (`memory-graph cluster remove {}`) or start this \
                 node again with the same --join to resume",
                spec.peer, spec.timeout, req.node_id, req.node_id
            )));
        }
        tracing::debug!(target = %target, error = %last, ?delay, "--join: retrying");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

fn adopt(identity: &ClusterIdentity, cluster_id: &str) -> Result<(), StoreError> {
    if let Some(mine) = identity.get() {
        if mine != cluster_id {
            return Err(StoreError::WrongCluster {
                expected: cluster_id.to_string(),
                found: mine,
            });
        }
        return Ok(());
    }
    identity
        .check_or_adopt(Some(cluster_id), true)
        .map_err(StoreError::Storage)
}

/// `--auto-promote`: while this node is a learner, send `Join` again every
/// [`REJOIN_INTERVAL`] (to the leader it knows, else the peer). Ends once
/// it is a voter, or at shutdown.
pub fn spawn_rejoin(
    raft: RaftNode,
    peer: String,
    mut req: pb::JoinRequest,
    shutdown: ShutdownHandle,
) {
    let me = raft.node_id;
    // Never an add: a leader that no longer lists this node refuses it (it
    // was removed) instead of adding it back.
    req.rejoin = true;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.wait() => return,
                _ = tokio::time::sleep(REJOIN_INTERVAL) => {}
            }
            let m = raft.metrics();
            let mem = m.membership_config.membership();
            if mem.voter_ids().any(|v| v == me) {
                tracing::info!("promoted to voter");
                return;
            }
            if mem.get_node(&me).is_none() {
                // Not a member (yet): the leader's first AppendEntries
                // has not arrived.
                continue;
            }
            let target = raft.leader().addr.unwrap_or_else(|| peer.clone());
            let r = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
                admin(&target, Duration::from_secs(5))
                    .await?
                    .join(req.clone())
                    .await
            })
            .await;
            match r {
                Ok(Ok(_)) => {}
                Ok(Err(st)) => match classify(&target, &st) {
                    Next::Retry(_) => {
                        tracing::debug!(%target, error = %st.message(), "re-join; retrying")
                    }
                    // A refusal (removed, a member at another address,
                    // other extractors): asking again changes nothing.
                    Next::Fail(e) => {
                        tracing::warn!(
                            %target,
                            error = %e,
                            "the leader refused this node's --auto-promote re-join; not asking \
                             again (re-add it with `cluster add-learner` if it was removed)"
                        );
                        return;
                    }
                },
                Err(_) => tracing::debug!(%target, "re-join timed out"),
            }
        }
    });
}

/// Whether `raft` is a learner (a member that is not a voter).
pub fn is_learner(raft: &RaftNode) -> bool {
    let m = raft.metrics();
    let mem = m.membership_config.membership();
    mem.get_node(&raft.node_id).is_some() && !mem.voter_ids().any(|v| v == raft.node_id)
}

// ---------------------------------------------------------------------------
// `--bootstrap-or-join` (ADR 0004 D10).

/// What ordinal 0 found among its siblings ([`discover`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// No sibling reported a cluster id: bootstrap.
    NoCluster,
    /// `via` answered `cluster_id`; its membership is `members`.
    Cluster {
        cluster_id: String,
        via: String,
        members: Vec<pb::Member>,
    },
}

/// How long one sibling probe (connect + `Status`) may take.
const PROBE_ATTEMPT: Duration = Duration::from_secs(2);
/// Pause between probe rounds.
const PROBE_ROUND: Duration = Duration::from_millis(200);

/// What one sibling probe found.
#[derive(Debug)]
enum Probe {
    /// It answered `Status`.
    Answered(Box<pb::StatusResponse>),
    /// No TCP connection could be made within [`PROBE_ATTEMPT`]: its name
    /// did not resolve, the connection was refused, or nothing answered
    /// the handshake (nothing runs there).
    Unreachable,
    /// A TCP connection was made but `Status` was not answered within
    /// [`PROBE_ATTEMPT`]: something runs there and may be a live member.
    Silent,
}

async fn probe(addr: &str) -> Probe {
    // First the plain TCP connection, which separates "nothing there" from
    // "there but silent" (a process that accepts and never answers).
    let host_port = addr.split_once("://").map_or(addr, |(_, rest)| rest);
    let host_port = host_port.trim_end_matches('/');
    match tokio::time::timeout(PROBE_ATTEMPT, tokio::net::TcpStream::connect(host_port)).await {
        Ok(Ok(tcp)) => drop(tcp),
        Ok(Err(_)) | Err(_) => return Probe::Unreachable,
    }
    let asked = tokio::time::timeout(PROBE_ATTEMPT, async {
        admin(addr, PROBE_ATTEMPT)
            .await?
            .status(pb::StatusRequest {})
            .await
            .map(tonic::Response::into_inner)
    })
    .await;
    match asked {
        Ok(Ok(st)) => Probe::Answered(Box::new(st)),
        Ok(Err(_)) | Err(_) => Probe::Silent,
    }
}

async fn probe_status(addr: &str) -> Option<pb::StatusResponse> {
    match probe(addr).await {
        Probe::Answered(st) => Some(*st),
        Probe::Unreachable | Probe::Silent => None,
    }
}

/// Ask `siblings` (`Admin.Status`, in rounds, until `timeout`) whether a
/// cluster exists. Returns as soon as one reports a cluster id, or once
/// every sibling answered that it has none (a first deployment: they wait
/// to join ordinal 0). At the deadline, with none reporting a cluster: a
/// sibling that was reached but did not answer (a timeout, not a name that
/// does not resolve or a refused connection) fails the start, since it may
/// be a live member; otherwise (every silent sibling unreachable)
/// [`Discovery::NoCluster`]. Siblings reporting two different cluster ids
/// are refused.
pub async fn discover(siblings: &[String], timeout: Duration) -> Result<Discovery, StoreError> {
    let deadline = Instant::now() + timeout;
    let mut empty: std::collections::BTreeSet<String> = Default::default();
    // Siblings that, in the latest round, were reached but did not answer.
    let mut silent: std::collections::BTreeSet<String> = Default::default();
    loop {
        let round = futures_join_all(siblings.iter().cloned().map(|s| async move {
            let st = probe(&s).await;
            (s, st)
        }))
        .await;
        let mut found: Vec<(String, String)> = Vec::new();
        for (addr, st) in round {
            silent.remove(&addr);
            match st {
                Probe::Answered(st) if !st.cluster_id.is_empty() => {
                    found.push((addr, st.cluster_id))
                }
                Probe::Answered(_) => {
                    empty.insert(addr);
                }
                Probe::Silent => {
                    silent.insert(addr);
                }
                Probe::Unreachable => {}
            }
        }
        if let Some((via, cluster_id)) = found.first().cloned() {
            if let Some((other, id2)) = found.iter().find(|(_, c)| *c != cluster_id) {
                return Err(StoreError::Rejected(format!(
                    "--bootstrap-or-join: the other members disagree: {via} is in cluster \
                     {cluster_id} but {other} is in cluster {id2}; refusing to bootstrap or \
                     join until that is resolved"
                )));
            }
            let members = tokio::time::timeout(PROBE_ATTEMPT, async {
                admin(&via, PROBE_ATTEMPT)
                    .await?
                    .members(pb::MembersRequest {})
                    .await
                    .map(tonic::Response::into_inner)
            })
            .await
            .map_err(|_| tonic::Status::deadline_exceeded("Members"))
            .and_then(|r| r)
            .map_err(|e| {
                StoreError::Rejected(format!(
                    "--bootstrap-or-join: {via} is in cluster {cluster_id} but did not list its \
                     members: {}",
                    e.message()
                ))
            })?
            .members;
            return Ok(Discovery::Cluster {
                cluster_id,
                via,
                members,
            });
        }
        if siblings.iter().all(|s| empty.contains(s)) {
            return Ok(Discovery::NoCluster);
        }
        if Instant::now() >= deadline {
            // A sibling that is there but does not answer may be a live
            // member of a cluster (overloaded, partitioned): bootstrapping
            // next to it would create a second, empty cluster that reports
            // ready. Refuse; `--force-bootstrap` is the escape hatch.
            let hung: Vec<&str> = silent
                .iter()
                .map(String::as_str)
                .filter(|s| !empty.contains(*s))
                .collect();
            if !hung.is_empty() {
                return Err(StoreError::Rejected(format!(
                    "--bootstrap-or-join: {hung:?} accepted a TCP connection but did not \
                     answer Status within {timeout:?}; one of them may be a live member of an \
                     existing cluster, so this node neither bootstraps nor joins. Retry once \
                     they answer, or pass --force-bootstrap if every other member is known to \
                     be gone"
                )));
            }
            let down: Vec<&str> = siblings
                .iter()
                .map(String::as_str)
                .filter(|s| !empty.contains(*s))
                .collect();
            tracing::warn!(
                ?down,
                ?timeout,
                "--bootstrap-or-join: these members could not be reached (no such name, or \
                 connection refused); none that answered is in a cluster, so this node \
                 bootstraps one"
            );
            return Ok(Discovery::NoCluster);
        }
        tokio::time::sleep(PROBE_ROUND.min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

async fn futures_join_all<F: std::future::Future + Send + 'static>(
    futs: impl Iterator<Item = F>,
) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

/// Resolve [`InitMode::BootstrapOrJoin`](crate::InitMode::BootstrapOrJoin)
/// into the mode this start uses:
///
/// * an ordinal other than 0: [`InitMode::Join`](crate::InitMode::Join)
///   through the named peer (a restart when initialized);
/// * ordinal 0 on an initialized data directory: a restart;
/// * ordinal 0 on an uninitialized one: [`discover`] among the siblings.
///   No cluster: bootstrap. A cluster (pod 0 lost its volume): join it
///   through the sibling that answered. If the cluster still lists this
///   node id (it was a voter or a learner), that member is the lost
///   incarnation: it is removed first (a voter that lost its log must not
///   vote again under its id), provided it is recorded at this node's
///   advertised address and nothing that is in a cluster answers there (a
///   live duplicate is refused, never removed); the node then joins as a learner and, with
///   `--auto-promote`, becomes a voter once caught up.
///
/// `node_id` and `advertise` are this node's (`--node-id` /
/// `--node-id-from-hostname`, `--advertise`).
pub async fn resolve_bootstrap_or_join(
    spec: &crate::paths::BootstrapOrJoin,
    data_dir: &std::path::Path,
    node_id: Option<NodeId>,
    advertise: Option<&str>,
) -> Result<crate::InitMode, StoreError> {
    use crate::InitMode;
    if spec.ordinal != 0 {
        return Ok(InitMode::Join(spec.join.clone()));
    }
    let paths = crate::NodePaths::for_data_dir(data_dir);
    let initialized = match &paths.node_json {
        Some(p) => crate::NodeJson::read(p)?.is_some(),
        None => false,
    };
    let bootstrap = InitMode::Bootstrap { restore: None };
    if initialized || spec.force_bootstrap || spec.siblings.is_empty() {
        if spec.force_bootstrap && !initialized {
            tracing::warn!("--force-bootstrap: not asking the other members for a cluster");
        }
        return Ok(bootstrap);
    }
    tracing::info!(siblings = ?spec.siblings, "--bootstrap-or-join: ordinal 0 on an empty data directory; asking the other members whether a cluster exists");
    let (cluster_id, via, members) = match discover(&spec.siblings, spec.probe_timeout).await? {
        Discovery::NoCluster => return Ok(bootstrap),
        Discovery::Cluster {
            cluster_id,
            via,
            members,
        } => (cluster_id, via, members),
    };
    let me = node_id.ok_or_else(|| {
        StoreError::Rejected(
            "--bootstrap-or-join needs a node id (--node-id or --node-id-from-hostname)".into(),
        )
    })?;
    tracing::warn!(
        %cluster_id,
        %via,
        node_id = me,
        "--bootstrap-or-join: a cluster already exists (this pod lost its data directory); \
         joining it instead of bootstrapping a new one"
    );
    if let Some(m) = members.iter().find(|m| m.node_id == me) {
        let recovery = format!(
            "remove the old member (`memory-graph --server {via} cluster remove {me} --force`) \
             and start this node again, or pass --force-bootstrap to create a new cluster (its \
             data would be separate from cluster {cluster_id})"
        );
        match advertise {
            Some(a) if a == m.addr => {}
            Some(a) => {
                return Err(StoreError::Rejected(format!(
                    "--bootstrap-or-join: cluster {cluster_id} lists node {me} at {}, not at this \
                     node's address {a}; refusing to replace it. To recover: {recovery}",
                    m.addr
                )))
            }
            None => {
                return Err(StoreError::Rejected(format!(
                    "--bootstrap-or-join: cluster {cluster_id} lists node {me} at {}; without \
                     --advertise this node cannot tell whether that is its lost incarnation. \
                     Pass --advertise, or to recover: {recovery}",
                    m.addr
                )))
            }
        }
        // Same address: is anything alive there? This node does not serve
        // yet, so an answer carrying a cluster id is a live incarnation (a
        // duplicate pod, a stale process), never the lost one: removing it
        // would remove a healthy member.
        if let Some(st) = probe_status(&m.addr).await {
            if !st.cluster_id.is_empty() {
                return Err(StoreError::Rejected(format!(
                    "--bootstrap-or-join: cluster {cluster_id} lists node {me} at {}, and a live \
                     node {} of cluster {} answers there; that is not a lost incarnation, so it \
                     is not removed. Stop the other process (or give this one another node id \
                     or address), or to recover: {recovery}",
                    m.addr, st.node_id, st.cluster_id
                )));
            }
        }
        if m.role == "voter" && !spec.join.auto_promote {
            tracing::warn!(
                node_id = me,
                "--bootstrap-or-join: removing a voter without --auto-promote; the cluster runs \
                 with one voter fewer until this node is promoted again (`cluster promote {me}`)"
            );
        }
        tracing::warn!(
            node_id = me,
            role = %m.role,
            "--bootstrap-or-join: removing this node's lost incarnation from the cluster before \
             joining again"
        );
        remove_lost_self(&via, me, spec.join.timeout)
            .await
            .map_err(|e| {
                StoreError::Rejected(format!(
                    "--bootstrap-or-join: node {me} lost its data directory and is still a \
                     member of cluster {cluster_id}; removing the old member failed: {e}. To \
                     recover: {recovery}"
                ))
            })?;
    }
    let mut join = spec.join.clone();
    join.peer = via;
    Ok(InitMode::Join(join))
}

/// `Admin.Remove` of `id` through `peer` (forwarded to the leader), with
/// `force` (3 voters to 2: the node is back as a voter once it caught up),
/// retrying while no leader answers, or the leader refuses transiently,
/// until `timeout`. Public only for the membership tests.
#[doc(hidden)]
pub async fn remove_lost_self(peer: &str, id: NodeId, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut target = peer.to_string();
    let mut delay = Duration::from_millis(100);
    loop {
        let attempt = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
            admin(&target, Duration::from_secs(5))
                .await?
                .remove(pb::RemoveRequest {
                    node_id: id,
                    force: true,
                })
                .await
        })
        .await;
        let last = match attempt {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(st)) => {
                // Removed meanwhile (a retry after a lost answer): done.
                if st.message().contains(&format!("node {id} is not a member")) {
                    return Ok(());
                }
                match classify(&target, &st) {
                    Next::Fail(e) => {
                        // The quorum check refuses transiently while the
                        // other members still settle (a typed flag, issue
                        // #225); retry that too. The text match covers a
                        // leader one release older (no flag yet) in a
                        // mixed-version cluster: drop it after one release.
                        if !graph_proto::error::is_transient_rejection(&st)
                            && !st
                                .message()
                                .contains(crate::services::admin::QUORUM_REFUSAL)
                        {
                            return Err(e.to_string());
                        }
                        st.message().to_string()
                    }
                    Next::Retry(next) => {
                        target = next;
                        st.message().to_string()
                    }
                }
            }
            Err(_) => format!("no answer within {ATTEMPT_TIMEOUT:?}"),
        };
        if Instant::now() + delay > deadline {
            return Err(format!(
                "no leader removed it within {timeout:?}; last error: {last}"
            ));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}
