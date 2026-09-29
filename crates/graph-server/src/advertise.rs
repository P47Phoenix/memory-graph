//! `serve --update-advertise <host:port>` (ADR 0004 Q3, epic story 25,
//! issue #107): a member restarted at a new address tells the cluster.
//!
//! The advertised address is part of a node's identity: it is in
//! `node.json` and in every member's copy of the membership, and the leader
//! replicates to it there. A plain restart with another `--advertise` is
//! refused. With `--update-advertise` the restarted node serves at the new
//! address and then asks the leader (`Admin.UpdateAdvertise`, sent to any
//! member it knows, which forwards it; or to itself, should it lead) to
//! replace its address in the membership. The leader first asks the server
//! at the new address who it is (it must be this node id, of this cluster,
//! with this cluster's extractors), then commits one membership entry with
//! openraft's `ChangeMembers::SetNodes` (the voter and learner sets are
//! unchanged; every replication stream is rebuilt with the new address).
//! Only once that entry is committed does the node rewrite `node.json`, so
//! a failure (the timeout, a refusal) leaves the node as it was and the
//! start fails with the reason.
use crate::join::{admin, classify, Next};
use crate::raft::NodeId;
use graph_proto::pb;
use graph_store::StoreError;
use std::time::{Duration, Instant};

/// One `UpdateAdvertise` attempt (the leader probes the node and commits a
/// membership entry) may take this long.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default for how long the start keeps trying before it fails.
pub const DEFAULT_UPDATE_ADVERTISE_TIMEOUT: Duration = Duration::from_secs(120);

/// Ask the cluster, through `endpoints` in turn (following a `NotLeader`
/// that names the leader), to record `addr` as node `node_id`'s address,
/// until one accepts or `timeout` passes. Returns the membership entry's
/// log index. A refusal fails at once.
pub async fn update_advertise(
    endpoints: &[String],
    node_id: NodeId,
    addr: &str,
    timeout: Duration,
) -> Result<u64, StoreError> {
    if endpoints.is_empty() {
        return Err(StoreError::Rejected(
            "--update-advertise: this node knows no member to ask".into(),
        ));
    }
    let deadline = Instant::now() + timeout;
    let req = pb::UpdateAdvertiseRequest {
        node_id,
        addr: addr.to_string(),
    };
    let mut turn = 0usize;
    let mut target = endpoints[0].clone();
    let mut delay = Duration::from_millis(100);
    let mut last: String;
    loop {
        let attempt = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
            admin(&target, Duration::from_secs(5))
                .await?
                .update_advertise(req.clone())
                .await
                .map(tonic::Response::into_inner)
        })
        .await;
        let mut next = None;
        match attempt {
            Ok(Ok(resp)) => {
                tracing::info!(
                    node_id,
                    addr,
                    via = %target,
                    log_index = resp.log_index,
                    "--update-advertise: the cluster records the new address"
                );
                return Ok(resp.log_index);
            }
            Ok(Err(st)) => match classify(&target, &st) {
                Next::Fail(e) => {
                    return Err(StoreError::Rejected(format!(
                        "--update-advertise {addr}: the cluster refused it (asked through \
                         {target}): {e}"
                    )))
                }
                Next::Retry(to) => {
                    last = format!("{target}: {}", st.message());
                    if to != target {
                        next = Some(to);
                    }
                }
            },
            Err(_) => last = format!("{target}: no answer within {ATTEMPT_TIMEOUT:?}"),
        }
        // The leader named by a `NotLeader`, else the next member.
        target = match next {
            Some(leader) => leader,
            None => {
                turn += 1;
                endpoints[turn % endpoints.len()].clone()
            }
        };
        if Instant::now() + delay > deadline {
            return Err(StoreError::Rejected(format!(
                "--update-advertise {addr}: no leader accepted the new address within \
                 {timeout:?} (asked {}); last error: {last}. node.json is unchanged; start the \
                 node again with the same flag to retry",
                endpoints.join(", ")
            )));
        }
        tracing::debug!(%target, error = %last, ?delay, "--update-advertise: retrying");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}
