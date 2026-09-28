//! `memory_graph.v1.Admin` (ADR 0004 D10): `Status`, `SysInfo`, `Compact`
//! and `Shutdown` (stage A); `Members`, `Leader`, `TriggerSnapshot` and
//! minimal `AddLearner` / `Promote` (stage B: no guards yet, stage C adds
//! them together with `Remove` and `TransferLeader`, UNIMPLEMENTED until
//! then). Membership changes must reach the leader: a follower answers
//! `NotLeader` naming it.
use super::{status, Ctx};
use crate::raft::snapshot_dir::read_sidecar;
use crate::raft::NodeId;
use crate::SERVER_VERSION;
use graph_proto::{pb, PROTOCOL_VERSION};
use graph_store::StoreError;
use openraft::ServerState;
use std::collections::BTreeSet;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tonic::{Request, Response, Status};

pub struct AdminService {
    pub ctx: Arc<Ctx>,
}

fn later(what: &str) -> Status {
    Status::unimplemented(format!(
        "Admin.{what} arrives with membership administration (stage C)"
    ))
}

/// How long `TriggerSnapshot` waits for the build.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(600);

fn role(s: ServerState) -> &'static str {
    match s {
        ServerState::Leader => "leader",
        ServerState::Follower => "follower",
        ServerState::Candidate => "candidate",
        ServerState::Learner => "learner",
        ServerState::Shutdown => "shutdown",
    }
}

impl AdminService {
    fn members(&self) -> Vec<pb::Member> {
        let m = self.ctx.raft.metrics();
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
                // Per-member hashes are exchanged in stage C (the
                // AddLearner/promotion gate); this node knows its own.
                extractors_hash: if *id == self.ctx.info.node_id {
                    self.ctx.info.extractors_hash.clone()
                } else {
                    String::new()
                },
            })
            .collect()
    }
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
                        pb::PeerLag {
                            node_id: *id,
                            matched_index,
                            lag: last_log_index.saturating_sub(matched_index.unwrap_or(0)),
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
            members: self.members(),
            replication,
            log_bytes: self.ctx.raft.log_bytes(),
            store_bytes,
            data_dir: self.ctx.info.data_dir.clone(),
            advertise: self.ctx.info.advertise.clone(),
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
            members: self.members(),
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
        let r = req.into_inner();
        if r.node_id == 0 || r.addr.is_empty() {
            return Err(status(StoreError::Rejected(
                "AddLearner needs a node id (>= 1) and an address".into(),
            )));
        }
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
        let id = req.into_inner().node_id;
        let m = self.ctx.raft.metrics();
        let mem = m.membership_config.membership();
        if mem.get_node(&id).is_none() {
            return Err(status(StoreError::Rejected(format!(
                "node {id} is not a member; add it as a learner first"
            ))));
        }
        let mut voters: BTreeSet<NodeId> = mem.voter_ids().collect();
        voters.insert(id);
        let log_index = self
            .ctx
            .raft
            .change_membership(voters)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::PromoteResponse { log_index }))
    }

    async fn remove(
        &self,
        _req: Request<pb::RemoveRequest>,
    ) -> Result<Response<pb::RemoveResponse>, Status> {
        Err(later("Remove"))
    }

    async fn transfer_leader(
        &self,
        _req: Request<pb::TransferLeaderRequest>,
    ) -> Result<Response<pb::TransferLeaderResponse>, Status> {
        Err(later("TransferLeader"))
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
        let (side, path) = self
            .ctx
            .raft
            .snapshots
            .current()
            .ok_or_else(|| Status::internal("the snapshot was built but is not on disk"))?;
        // Open before answering: a newer build may remove the pair while we
        // stream, and an open handle keeps the bytes readable.
        let file = std::fs::File::open(&path)
            .map_err(|e| Status::internal(format!("opening the snapshot: {e}")))?;
        let _ = read_sidecar(&path);
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
        Err(Status::unimplemented(
            "Admin.Metrics arrives with observability (stage E)",
        ))
    }
}
