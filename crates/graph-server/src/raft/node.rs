//! [`RaftNode`]: start the one-member Raft over the store (ADR 0004 D5),
//! initialize it on first start, resume from the log after, and answer the
//! leader questions the services ask.
use super::log_store::{log_path, RedbLogStore};
use super::network::LoopbackNetwork;
use super::state_machine::StoreStateMachine;
use super::types::{LogRequest, LogResponse, NodeId, TypeConfig};
use crate::slot::StoreSlot;
use graph_store::StoreError;
use openraft::error::{CheckIsLeaderError, ClientWriteError, RaftError};
use openraft::impls::BasicNode;
use openraft::{Config, Raft, RaftMetrics, ServerState, SnapshotPolicy};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The suggested client back-off when no leader is known (`NoLeader`).
pub const NO_LEADER_RETRY_MS: u64 = 200;

#[derive(Clone)]
pub struct RaftNode {
    pub raft: Raft<TypeConfig>,
    pub log_store: RedbLogStore,
    pub node_id: NodeId,
    pub addr: String,
    /// Test hook ([`crate::server::TestingHooks::withhold_leader`]).
    pub withhold_leader: bool,
}

/// The leader as this node knows it: id and advertised address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaderInfo {
    pub id: Option<NodeId>,
    pub addr: Option<String>,
}

fn fatal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Storage(format!("raft: {e}"))
}

impl RaftNode {
    /// Open the log next to the store, spawn the Raft task, initialize the
    /// one-member cluster on a fresh log, and wait until this node leads.
    pub async fn start(
        node_id: NodeId,
        addr: String,
        slot: Arc<StoreSlot>,
    ) -> Result<Self, StoreError> {
        let config = Config {
            cluster_name: "memory-graph".into(),
            // A single node elects itself at the first election timeout;
            // short timeouts keep a (test) server's start-up quick, and no
            // peer exists to disturb.
            heartbeat_interval: 50,
            election_timeout_min: 100,
            election_timeout_max: 200,
            // Stage A never builds a snapshot on its own (the log is kept
            // whole); stage B adds the purge policy.
            snapshot_policy: SnapshotPolicy::Never,
            max_in_snapshot_log_to_keep: u64::MAX / 2,
            ..Config::default()
        }
        .validate()
        .map_err(fatal)?;
        let log_store = RedbLogStore::open(&log_path(slot.path()))?;
        let sm = StoreStateMachine::new(slot);
        let raft = Raft::new(
            node_id,
            Arc::new(config),
            LoopbackNetwork,
            log_store.clone(),
            sm,
        )
        .await
        .map_err(fatal)?;
        if !raft.is_initialized().await.map_err(fatal)? {
            let mut members = BTreeMap::new();
            members.insert(node_id, BasicNode::new(&addr));
            raft.initialize(members).await.map_err(fatal)?;
        }
        raft.wait(Some(Duration::from_secs(30)))
            .state(ServerState::Leader, "single-node leader")
            .await
            .map_err(|e| fatal(format!("waiting to become leader: {e}")))?;
        Ok(Self {
            raft,
            log_store,
            node_id,
            addr,
            withhold_leader: false,
        })
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

    /// Propose one command and wait for it to be applied on this node
    /// (ADR 0004 D7): returns the entry's response and its log index.
    pub async fn propose(&self, req: LogRequest) -> Result<(LogResponse, u64), StoreError> {
        if self.withhold_leader {
            return Err(StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            });
        }
        match self.raft.client_write(req).await {
            Ok(resp) => {
                let index = resp.log_id().index;
                Ok((resp.response().clone(), index))
            }
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(f))) => {
                Err(StoreError::NotLeader {
                    leader_id: f.leader_id,
                    leader_addr: f.leader_node.map(|n| n.addr),
                })
            }
            Err(RaftError::APIError(e)) => Err(fatal(e)),
            Err(RaftError::Fatal(e)) => Err(fatal(e)),
        }
    }

    /// openraft's read barrier (ADR 0004 D8, `LINEARIZABLE`): confirms
    /// leadership with a quorum and waits for the applied index to reach
    /// the read index.
    pub async fn ensure_linearizable(&self) -> Result<(), StoreError> {
        if self.withhold_leader {
            return Err(StoreError::NoLeader {
                retry_after_ms: NO_LEADER_RETRY_MS,
            });
        }
        match self.raft.ensure_linearizable().await {
            Ok(_) => Ok(()),
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

    pub async fn shutdown(&self) {
        if let Err(e) = self.raft.shutdown().await {
            tracing::warn!(error = %e, "raft shutdown");
        }
    }
}
