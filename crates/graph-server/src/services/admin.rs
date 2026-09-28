//! `memory_graph.v1.Admin` (ADR 0004 D10): `Status`, `SysInfo`, `Compact`
//! and `Shutdown` in stage A; the cluster operations are declared in the
//! proto and answer UNIMPLEMENTED until stage B/C.
use super::Ctx;
use crate::SERVER_VERSION;
use graph_proto::{pb, PROTOCOL_VERSION};
use graph_store::StoreError;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct AdminService {
    pub ctx: Arc<Ctx>,
}

fn later(what: &str) -> Status {
    Status::unimplemented(format!(
        "Admin.{what} arrives with cluster support (stage B/C)"
    ))
}

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
        let committed = m.last_applied.as_ref().map_or(0, |l| l.index);
        Ok(Response::new(pb::StatusResponse {
            node_id: self.ctx.info.node_id,
            cluster_id: self.ctx.info.cluster_id.clone(),
            leader_id: leader.id,
            leader_addr: leader.addr,
            state: format!("{:?}", m.state),
            current_term: m.current_term,
            applied_index: marker.map_or(0, |mk| mk.index),
            applied_term: marker.map_or(0, |mk| mk.term),
            committed_index: committed,
            last_log_index: m.last_log_index.unwrap_or(0),
            server_version: SERVER_VERSION.into(),
            protocol_version: PROTOCOL_VERSION,
            store_format_version: graph_store::SCHEMA_VERSION,
            extractors_hash: self.ctx.info.extractors_hash.clone(),
            db_path: self.ctx.info.db_path.clone(),
            listen_addr: self.ctx.info.listen_addr.clone(),
            uptime_secs: self.ctx.info.started.elapsed().as_secs(),
            snapshots: Some(snapshots.into()),
            snapshot_handles: self.ctx.slot.snapshots().len() as u64,
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
        Err(later("Members"))
    }
    async fn leader(
        &self,
        _req: Request<pb::LeaderRequest>,
    ) -> Result<Response<pb::LeaderResponse>, Status> {
        Err(later("Leader"))
    }
    async fn add_learner(
        &self,
        _req: Request<pb::AddLearnerRequest>,
    ) -> Result<Response<pb::AddLearnerResponse>, Status> {
        Err(later("AddLearner"))
    }
    async fn promote(
        &self,
        _req: Request<pb::PromoteRequest>,
    ) -> Result<Response<pb::PromoteResponse>, Status> {
        Err(later("Promote"))
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
    async fn trigger_snapshot(
        &self,
        _req: Request<pb::TriggerSnapshotRequest>,
    ) -> Result<Response<pb::TriggerSnapshotResponse>, Status> {
        Err(later("TriggerSnapshot"))
    }
    async fn metrics(
        &self,
        _req: Request<pb::MetricsRequest>,
    ) -> Result<Response<pb::MetricsResponse>, Status> {
        Err(later("Metrics"))
    }
}
