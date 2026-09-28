//! A loopback `RaftNetworkFactory` (ADR 0004 stage A): a one-member cluster
//! never sends an RPC, so every method reports the peer unreachable. Stage
//! B replaces this with a gRPC network over the `Raft` service.
use super::types::{NodeId, TypeConfig};
use openraft::error::{RPCError, RaftError, ReplicationClosed, StreamingError, Unreachable};
use openraft::impls::BasicNode;
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Snapshot, Vote};
use std::future::Future;

pub struct LoopbackNetwork;

pub struct NoPeer {
    target: NodeId,
}

fn unreachable(target: NodeId) -> Unreachable {
    Unreachable::new(&std::io::Error::other(format!(
        "node {target}: no network in a single-node server (stage A)"
    )))
}

impl RaftNetworkFactory<TypeConfig> for LoopbackNetwork {
    type Network = NoPeer;

    async fn new_client(&mut self, target: NodeId, _node: &BasicNode) -> Self::Network {
        NoPeer { target }
    }
}

impl RaftNetwork<TypeConfig> for NoPeer {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(RPCError::Unreachable(unreachable(self.target)))
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(RPCError::Unreachable(unreachable(self.target)))
    }

    async fn full_snapshot(
        &mut self,
        _vote: Vote<NodeId>,
        _snapshot: Snapshot<TypeConfig>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<NodeId>, StreamingError<TypeConfig, openraft::error::Fatal<NodeId>>>
    {
        Err(StreamingError::Unreachable(unreachable(self.target)))
    }
}
