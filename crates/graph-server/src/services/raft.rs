//! `memory_graph.v1.Raft` (ADR 0004 D5): the receiving side of the Raft
//! network, on the same port as the client services.
//!
//! Every call first checks two headers, refusing with `FAILED_PRECONDITION`:
//!
//! * the sender's cluster id (`mg-cluster-id`): required once this node has
//!   one and must match; a node that has none yet (an uninitialized member)
//!   adopts it from a leader's `AppendEntries` or `InstallSnapshot`, which
//!   make it a member, but not from a `Vote`, which any candidate may send;
//! * the sender's extractor version set hash (`mg-extractors-hash`): a
//!   replica must extract identically (D5), so Raft traffic from a node
//!   built with other extractors is refused in both directions.
//!
//! Disk (D7): a follower whose disk guard trips refuses an `AppendEntries`
//! that carries entries with `RESOURCE_EXHAUSTED` before appending
//! anything; the leader backs off and retries, so the follower lags while
//! the rest of the cluster commits, and catches up once space is freed.
//! Heartbeats are still answered (no election). `InstallSnapshot` needs
//! room for two copies of the snapshot (the received file and the staged
//! store) on top of `--min-free-disk`, and is refused the same way before
//! receiving. (A leader's own disk is checked before it proposes a write;
//! an I/O error while appending or applying anyway stops the node, see
//! the state machine.)
//!
//! `InstallSnapshot` writes the streamed chunks to a temp file under the
//! snapshots directory, checks size and SHA-256 against the header, refuses
//! another extractor version set hash, or a store format that is neither
//! this binary's nor one it upgrades on open, with `FAILED_PRECONDITION` (a
//! replica built with other extractors would not answer queries
//! identically, D5; an upgradable older format is upgraded in place when
//! the installed store opens), and only then hands the file to
//! openraft (`Raft::install_full_snapshot`), whose state machine swaps the
//! store under the slot's write lock.
use crate::disk::DiskGuard;
use crate::paths::{ClusterIdentity, CLUSTER_ID_HEADER, EXTRACTORS_HASH_HEADER};
use crate::raft::snapshot_dir::{hex, SnapshotDir};
use crate::raft::types::{SnapshotFile, SnapshotMeta};
use crate::raft::{wire, TypeConfig};
use graph_proto::pb;
use openraft::error::RaftError;
use openraft::{Raft, Snapshot};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

pub struct RaftService {
    pub raft: Raft<TypeConfig>,
    pub identity: Arc<ClusterIdentity>,
    pub snapshots: Arc<SnapshotDir>,
    pub disk: DiskGuard,
    /// Test hook ([`crate::TestingHooks::delay_append_entries_ms`]).
    pub delay_append: Option<std::time::Duration>,
    /// Where leader contact and the leader's committed index are recorded:
    /// read staleness (D8) and readiness (D10) both judge by it.
    pub obs: Arc<crate::observe::Observability>,
}

/// The version checks of an `InstallSnapshot` header against this node.
/// The store format must be this binary's or one it upgrades on open
/// (`graph_store::UPGRADABLE_SCHEMA_VERSIONS`, as `--restore` accepts): a
/// leader not yet restarted on a new build, or one whose snapshot predates
/// its upgrade, sends the older format, and the installed store is upgraded
/// in place when it opens (#151). The extractor version set hash must match
/// exactly (D5).
pub fn check_snapshot_header(format: u64, hash: &str, local_hash: &str) -> Result<(), Status> {
    if format != graph_store::SCHEMA_VERSION
        && !graph_store::UPGRADABLE_SCHEMA_VERSIONS.contains(&format)
    {
        return Err(Status::failed_precondition(format!(
            "snapshot store format {format} differs from this node's {} and is not one it \
             upgrades",
            graph_store::SCHEMA_VERSION
        )));
    }
    if hash != local_hash {
        return Err(Status::failed_precondition(format!(
            "snapshot extractor version set hash {hash} differs from this node's {local_hash}: \
             a replica must extract identically (ADR 0004 D5)"
        )));
    }
    Ok(())
}

fn raft_status<E: std::fmt::Display>(e: RaftError<u64, E>) -> Status {
    Status::unavailable(format!("raft: {e}"))
}

fn bad(e: impl std::fmt::Display) -> Status {
    Status::invalid_argument(e.to_string())
}

fn disk_full(e: graph_store::StoreError) -> Status {
    Status::resource_exhausted(e.to_string())
}

impl RaftService {
    /// The header checks every call starts with; `adopt`: this RPC may
    /// make an uninitialized node a member of the sender's cluster.
    fn check_headers<T>(&self, req: &Request<T>, adopt: bool) -> Result<(), Status> {
        let header = |name: &str| req.metadata().get(name).and_then(|v| v.to_str().ok());
        let theirs = header(EXTRACTORS_HASH_HEADER).unwrap_or("");
        let mine = self.snapshots.extractors_hash();
        if theirs != mine {
            return Err(Status::failed_precondition(format!(
                "extractor version set hash `{theirs}` of the sender differs from this node's \
                 `{mine}`: a replica must extract identically (ADR 0004 D5)"
            )));
        }
        self.identity
            .check_or_adopt(header(CLUSTER_ID_HEADER), adopt)
            .map_err(Status::failed_precondition)
    }
}

#[tonic::async_trait]
impl pb::raft_server::Raft for RaftService {
    async fn append_entries(
        &self,
        req: Request<pb::AppendEntriesRequest>,
    ) -> Result<Response<pb::AppendEntriesResponse>, Status> {
        self.check_headers(&req, true)?;
        let req = req.into_inner();
        if !req.entries.is_empty() {
            self.disk.check("replicated append").map_err(disk_full)?;
            if let Some(d) = self.delay_append {
                tokio::time::sleep(d).await;
            }
        }
        let rpc = wire::append_from_pb(req).map_err(bad)?;
        let leader_commit = rpc.leader_commit.map(|l| l.index);
        let resp = self.raft.append_entries(rpc).await.map_err(raft_status)?;
        // Any answer but a higher vote means a live leader of this term
        // reached us (a conflict or partial success only means we lag,
        // which the recorded commit index shows). Heartbeats included: the
        // contact keeps a follower fresh (D8) and ready (D10), and the
        // commit index tells a node far behind (a learner catching up)
        // that it is neither.
        if !matches!(resp, openraft::raft::AppendEntriesResponse::HigherVote(_)) {
            self.obs.note_leader_contact(leader_commit);
        }
        Ok(Response::new(wire::append_resp_to_pb(&resp)))
    }

    async fn vote(
        &self,
        req: Request<pb::VoteRequest>,
    ) -> Result<Response<pb::VoteResponse>, Status> {
        self.check_headers(&req, false)?;
        let rpc = wire::vote_req_from_pb(req.into_inner()).map_err(bad)?;
        let resp = self.raft.vote(rpc).await.map_err(raft_status)?;
        Ok(Response::new(wire::vote_resp_to_pb(&resp)))
    }

    async fn install_snapshot(
        &self,
        req: Request<Streaming<pb::InstallSnapshotRequest>>,
    ) -> Result<Response<pb::InstallSnapshotResponse>, Status> {
        self.check_headers(&req, true)?;
        self.obs.note_heard_from_leader();
        let mut stream = req.into_inner();
        let header = match stream.next().await {
            Some(Ok(pb::InstallSnapshotRequest {
                msg: Some(pb::install_snapshot_request::Msg::Header(h)),
            })) => h,
            Some(Ok(_)) => return Err(bad("InstallSnapshot must start with a header")),
            Some(Err(e)) => return Err(e),
            None => return Err(bad("empty InstallSnapshot stream")),
        };
        check_snapshot_header(
            header.store_format_version,
            &header.extractors_hash,
            self.snapshots.extractors_hash(),
        )?;
        // The received file plus the staged copy the swap renames into
        // place (the old store goes after the swap).
        self.disk
            .check_extra("snapshot install", header.size.saturating_mul(2))
            .map_err(disk_full)?;
        let vote = wire::vote_from_pb(header.vote).map_err(bad)?;
        let meta = SnapshotMeta {
            last_log_id: header.last_log_id.map(wire::log_id_from_pb),
            last_membership: wire::membership_from_json(&header.membership_json).map_err(bad)?,
            snapshot_id: header.snapshot_id,
        };
        let path = self.snapshots.incoming_path();
        let io = |e: std::io::Error| Status::internal(format!("writing the snapshot: {e}"));
        let received = async {
            let mut file = std::fs::File::create(&path).map_err(io)?;
            let mut h = Sha256::new();
            let mut n = 0u64;
            while let Some(msg) = stream.next().await {
                match msg?.msg {
                    Some(pb::install_snapshot_request::Msg::Chunk(c)) => {
                        n += c.len() as u64;
                        if n > header.size {
                            return Err(bad(format!(
                                "snapshot stream exceeds its declared size {}",
                                header.size
                            )));
                        }
                        h.update(&c);
                        file.write_all(&c).map_err(io)?;
                    }
                    _ => return Err(bad("a second header in the InstallSnapshot stream")),
                }
            }
            file.sync_all().map_err(io)?;
            let sha = hex(&h.finalize());
            if n != header.size || sha != header.sha256 {
                return Err(Status::data_loss(format!(
                    "snapshot arrived as {n} bytes with sha256 {sha}, expected {} bytes with {}",
                    header.size, header.sha256
                )));
            }
            Ok(())
        }
        .await;
        if let Err(e) = received {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        let snapshot = Snapshot {
            meta,
            snapshot: Box::new(SnapshotFile { path: path.clone() }),
        };
        let r = self.raft.install_full_snapshot(vote, snapshot).await;
        // openraft ignores a snapshot older than what it has, without
        // calling install: never leave the file behind.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let resp = r.map_err(|e| Status::unavailable(format!("raft: {e}")))?;
        Ok(Response::new(pb::InstallSnapshotResponse {
            vote: Some(wire::vote_to_pb(&resp.vote)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::check_snapshot_header;
    use graph_store::{SCHEMA_VERSION, UPGRADABLE_SCHEMA_VERSIONS};
    use tonic::Code;

    /// #151: the accept/refuse matrix of an `InstallSnapshot` header.
    #[test]
    fn snapshot_header_accepts_current_and_upgradable_formats_with_the_same_hash() {
        assert!(check_snapshot_header(SCHEMA_VERSION, "h", "h").is_ok());
        for &old in UPGRADABLE_SCHEMA_VERSIONS {
            assert!(check_snapshot_header(old, "h", "h").is_ok(), "v{old}");
            let e = check_snapshot_header(old, "other", "h").unwrap_err();
            assert_eq!(e.code(), Code::FailedPrecondition, "v{old}: {e:?}");
            assert!(e.message().contains("extractor"), "{e:?}");
        }
        let oldest = *UPGRADABLE_SCHEMA_VERSIONS.iter().min().unwrap();
        for bad in [SCHEMA_VERSION + 1, oldest - 1, 0] {
            let e = check_snapshot_header(bad, "h", "h").unwrap_err();
            assert_eq!(e.code(), Code::FailedPrecondition, "v{bad}: {e:?}");
            assert!(e.message().contains("store format"), "{e:?}");
        }
        let e = check_snapshot_header(SCHEMA_VERSION, "other", "h").unwrap_err();
        assert_eq!(e.code(), Code::FailedPrecondition, "{e:?}");
    }
}
