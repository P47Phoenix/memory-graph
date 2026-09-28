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
//!   leader keeps one promotion task per node).
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

fn uri(addr: &str) -> String {
    if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

async fn admin(
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
enum Next {
    /// Try again (after a back-off), at this address.
    Retry(String),
    /// A refusal: give up.
    Fail(StoreError),
}

fn classify(current: &str, st: &tonic::Status) -> Next {
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
                 accepted the request; last error: {last}",
                spec.peer, spec.timeout
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
pub fn spawn_rejoin(raft: RaftNode, peer: String, req: pb::JoinRequest, shutdown: ShutdownHandle) {
    let me = raft.node_id;
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
                Ok(Err(e)) => tracing::debug!(%target, error = %e.message(), "re-join"),
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
