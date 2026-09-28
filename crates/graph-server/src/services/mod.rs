//! The tonic services (ADR 0004 D1): [`store::StoreService`] (reads,
//! `Hello`, snapshot handles), [`write::WriteService`] (every write, through
//! Raft) and [`admin::AdminService`] (status, sysinfo, compact, shutdown).
//! [`Ctx`] is what they share.
use crate::raft::RaftNode;
use crate::server::{ShutdownHandle, SysInfoFn};
use crate::slot::StoreSlot;
use graph_proto::{store_error_to_status, View};
use graph_store::{StoreError, StoreRead};
use std::sync::Arc;
use std::time::Instant;
use tonic::Status;

pub mod admin;
pub mod raft;
pub mod store;
pub mod write;

/// Facts about this server that never change while it runs.
pub struct ServerInfo {
    pub node_id: u64,
    /// The cluster id (learned later by an uninitialized member).
    pub identity: Arc<crate::paths::ClusterIdentity>,
    pub extractors_hash: String,
    pub db_path: String,
    pub listen_addr: String,
    pub started: Instant,
    /// The protocol version `Hello` reports (the real one outside tests).
    pub hello_protocol_version: u32,
    /// `serve --data-dir` (empty in `--db` mode).
    pub data_dir: String,
    /// This node's advertised address.
    pub advertise: String,
}

impl ServerInfo {
    /// The cluster id, empty while an uninitialized member has none.
    pub fn cluster_id(&self) -> String {
        self.identity.get().unwrap_or_default()
    }
}

pub struct Ctx {
    pub slot: Arc<StoreSlot>,
    pub raft: RaftNode,
    pub info: ServerInfo,
    pub shutdown: ShutdownHandle,
    pub sysinfo: Option<SysInfoFn>,
    /// [`crate::server::TestingHooks::stall_writes_after`].
    pub stall_writes_after: Option<usize>,
    /// Write proposals seen so far (counted only with a stall hook set).
    pub writes_proposed: std::sync::atomic::AtomicUsize,
}

pub fn status(e: StoreError) -> Status {
    store_error_to_status(&e)
}

fn join_err(e: tokio::task::JoinError) -> Status {
    Status::internal(format!("blocking task failed: {e}"))
}

impl Ctx {
    /// Run a read against the requested view (ADR 0004 D8): `Local` on the
    /// store as it is, `Linearizable` after openraft's read barrier, or a
    /// snapshot handle. The read itself runs on the blocking pool.
    pub async fn read<T, F>(&self, view: Option<graph_proto::pb::View>, f: F) -> Result<T, Status>
    where
        T: Send + 'static,
        F: FnOnce(&dyn StoreRead) -> Result<T, StoreError> + Send + 'static,
    {
        let view = View::try_from(view)?;
        let slot = Arc::clone(&self.slot);
        match view {
            View::Local | View::Linearizable => {
                if view == View::Linearizable {
                    self.raft.ensure_linearizable().await.map_err(status)?;
                }
                tokio::task::spawn_blocking(move || slot.with_store(|s| f(s)))
                    .await
                    .map_err(join_err)?
                    .map_err(status)
            }
            View::Snapshot(id) => {
                let snap = slot.snapshots().get(id).map_err(status)?;
                tokio::task::spawn_blocking(move || {
                    let g = snap
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    f(&*g)
                })
                .await
                .map_err(join_err)?
                .map_err(status)
            }
        }
    }

    /// Run a node-local write (a dry-run prune, compact) on the blocking
    /// pool.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, Status>
    where
        T: Send + 'static,
        F: FnOnce(&StoreSlot) -> Result<T, StoreError> + Send + 'static,
    {
        let slot = Arc::clone(&self.slot);
        tokio::task::spawn_blocking(move || f(&slot))
            .await
            .map_err(join_err)?
            .map_err(status)
    }
}
