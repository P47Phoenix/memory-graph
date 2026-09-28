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
    /// Forwarding to the leader (writes, membership changes, the
    /// linearizable read barrier) and its counter.
    pub fwd: crate::forward::Forwarder,
    /// Joiners the leader promotes once they caught up (`--auto-promote`);
    /// one task per node id.
    pub auto_promoting: std::sync::Mutex<std::collections::BTreeSet<u64>>,
}

/// How long a follower's `LINEARIZABLE` read waits to apply the leader's
/// read index before it answers `NoLeader`.
pub const READ_INDEX_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn status(e: StoreError) -> Status {
    store_error_to_status(&e)
}

fn join_err(e: tokio::task::JoinError) -> Status {
    Status::internal(format!("blocking task failed: {e}"))
}

impl Ctx {
    /// The read barrier of a `LINEARIZABLE` read (ADR 0004 D8). On the
    /// leader, openraft's own (`ensure_linearizable`). On any other node it
    /// is forwarded: the leader runs its barrier and answers its read index
    /// (`Admin.ReadIndex`), and this node serves the read once it has
    /// applied that index itself, so the read sees every write acknowledged
    /// before it began. No leader, or none reachable: `NoLeader`.
    ///
    /// The forwarded `ReadIndex` gets the bounded default deadline; the
    /// client's own `grpc-timeout` is enforced on this handler by tonic's
    /// server (which drops, and so cancels, the forwarded call with it).
    pub async fn linearizable_barrier(&self) -> Result<(), Status> {
        use crate::forward::{within, Forwarder, Route, FORWARD_UNARY_TIMEOUT};
        if self.raft.withhold_leader {
            self.raft.ensure_linearizable().await.map_err(status)?;
            return Ok(());
        }
        // One routing decision: `Local` exactly when this node leads (a
        // fresh request is never marked forwarded).
        let addr = match self.fwd.route(&self.raft, &tonic::Request::new(()))? {
            Route::Local => {
                self.raft.ensure_linearizable().await.map_err(status)?;
                return Ok(());
            }
            Route::Leader { addr, .. } => addr,
        };
        let deadline = self
            .fwd
            .deadline(&tonic::metadata::MetadataMap::new(), FORWARD_UNARY_TIMEOUT);
        let mut client = self.fwd.admin_client(&addr)?;
        let index = within(
            deadline,
            client.read_index(Forwarder::request(
                graph_proto::pb::ReadIndexRequest {},
                deadline,
            )),
        )
        .await
        .map_err(crate::forward::forward_error)?
        .into_inner()
        .read_index;
        self.raft
            .raft
            .wait(Some(READ_INDEX_WAIT))
            .applied_index_at_least(Some(index), "the leader's read index")
            .await
            .map_err(|e| {
                tracing::debug!(index, error = %e, "read index not applied in time");
                status(StoreError::NoLeader {
                    retry_after_ms: crate::raft::node::NO_LEADER_RETRY_MS,
                })
            })?;
        Ok(())
    }

    /// Run a read against the requested view (ADR 0004 D8): `Local` on the
    /// store as it is, `Linearizable` after the read barrier
    /// ([`linearizable_barrier`](Self::linearizable_barrier)), or a
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
                    self.linearizable_barrier().await?;
                }
                // Fails fast (UNAVAILABLE) while a snapshot install swaps
                // the file (D8): the client moves to its next endpoint.
                tokio::task::spawn_blocking(move || slot.with_store_read(|s| f(s)))
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
