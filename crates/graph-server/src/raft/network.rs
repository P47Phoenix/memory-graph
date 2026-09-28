//! The Raft network over gRPC (ADR 0004 D5): [`GrpcNetwork`] is openraft's
//! `RaftNetworkFactory`, [`GrpcConnection`] one peer's `RaftNetwork`, both
//! talking to the peer's `memory_graph.v1.Raft` service on its one port.
//!
//! * Entries travel in the log store's binary framing (see
//!   [`super::wire`]); an `AppendEntries` whose entries exceed
//!   [`RAFT_RPC_MAX_BYTES`] is answered locally with `PayloadTooLarge`
//!   (a hint of how many entries fit), and openraft retries at once with
//!   fewer, so one RPC stays bounded (a single larger entry goes alone).
//! * Snapshots: `full_snapshot` streams an `InstallSnapshotHeader` (vote,
//!   meta, size, sha256, store format, extractors hash, all from the
//!   snapshot's `.meta`) then the file in 1 MiB chunks.
//! * Errors: every transport failure, timeout or remote status is
//!   `Unreachable`, so openraft backs off before retrying (a `Network`
//!   error would retry at once, which on a persistent refusal such as a
//!   wrong cluster is a busy loop); the cached channel is dropped so the
//!   next attempt reconnects.
//! * Channels are cached per target node id and rebuilt when the member's
//!   address changes or a call failed.
//! * [`FaultPlan`] / [`FaultyNetwork`]: test fault injection (partitions,
//!   dropped `AppendEntries`), a wrapper that stage C/D tests reuse.
use super::snapshot_dir::read_sidecar;
use super::types::{NodeId, TypeConfig};
use super::wire;
use crate::paths::{ClusterIdentity, CLUSTER_ID_HEADER};
use graph_proto::pb;
use graph_proto::pb::raft_client::RaftClient;
use graph_proto::PROTOCOL_VERSION_HEADER;
use openraft::error::{
    Fatal, PayloadTooLarge, RPCError, RaftError, ReplicationClosed, StreamingError, Unreachable,
};
use openraft::impls::BasicNode;
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Snapshot, Vote};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};

/// Soft cap on the entry bytes of one `AppendEntries` RPC (a single larger
/// entry is sent alone).
pub const RAFT_RPC_MAX_BYTES: usize = 16 << 20;

/// Snapshot stream chunk size.
pub const SNAPSHOT_CHUNK_BYTES: usize = 1 << 20;

const NO_LIMIT: usize = usize::MAX;

fn unreachable(target: NodeId, what: impl std::fmt::Display) -> Unreachable {
    Unreachable::new(&std::io::Error::other(format!("node {target}: {what}")))
}

/// Adds the protocol version and this node's cluster id to every call.
#[derive(Clone)]
pub struct RaftHeaders {
    identity: Arc<ClusterIdentity>,
}

impl Interceptor for RaftHeaders {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let md = req.metadata_mut();
        md.insert(
            PROTOCOL_VERSION_HEADER,
            graph_proto::PROTOCOL_VERSION
                .to_string()
                .parse()
                .expect("a number is valid ASCII metadata"),
        );
        if let Some(id) = self.identity.get() {
            if let Ok(v) = id.parse() {
                md.insert(CLUSTER_ID_HEADER, v);
            }
        }
        Ok(req)
    }
}

type Client = RaftClient<InterceptedService<Channel, RaftHeaders>>;

/// openraft's network factory over gRPC.
#[derive(Clone)]
pub struct GrpcNetwork {
    identity: Arc<ClusterIdentity>,
    channels: Arc<Mutex<HashMap<NodeId, (String, Channel)>>>,
    connect_timeout: Duration,
}

impl GrpcNetwork {
    pub fn new(identity: Arc<ClusterIdentity>) -> Self {
        Self {
            identity,
            channels: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout: Duration::from_secs(2),
        }
    }

    /// The cached channel to `target` at `addr`, rebuilt if the address
    /// changed (lazy: connecting happens on the first call).
    fn channel(&self, target: NodeId, addr: &str) -> Result<Channel, String> {
        let mut g = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((a, ch)) = g.get(&target) {
            if a == addr {
                return Ok(ch.clone());
            }
        }
        let uri = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let ch = Endpoint::from_shared(uri)
            .map_err(|e| format!("bad address `{addr}`: {e}"))?
            .connect_timeout(self.connect_timeout)
            .tcp_nodelay(true)
            .connect_lazy();
        g.insert(target, (addr.to_string(), ch.clone()));
        Ok(ch)
    }

    fn forget(&self, target: NodeId) {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&target);
    }
}

impl RaftNetworkFactory<TypeConfig> for GrpcNetwork {
    type Network = GrpcConnection;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        GrpcConnection {
            target,
            addr: node.addr.clone(),
            net: self.clone(),
        }
    }
}

/// One peer.
pub struct GrpcConnection {
    target: NodeId,
    addr: String,
    net: GrpcNetwork,
}

impl GrpcConnection {
    fn client(&self) -> Result<Client, Unreachable> {
        let ch = self
            .net
            .channel(self.target, &self.addr)
            .map_err(|e| unreachable(self.target, e))?;
        let headers = RaftHeaders {
            identity: Arc::clone(&self.net.identity),
        };
        Ok(RaftClient::with_interceptor(ch, headers)
            .max_decoding_message_size(NO_LIMIT)
            .max_encoding_message_size(NO_LIMIT))
    }

    /// A failed call: drop the channel so the next one reconnects.
    fn failed(&self, what: impl std::fmt::Display) -> Unreachable {
        self.net.forget(self.target);
        unreachable(self.target, what)
    }

    /// Run one unary call under the RPC's hard timeout.
    async fn unary<T>(
        &self,
        option: &RPCOption,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T, Unreachable> {
        match tokio::time::timeout(option.hard_ttl(), fut).await {
            Ok(Ok(r)) => Ok(r.into_inner()),
            Ok(Err(st)) => Err(self.failed(format!("{}: {}", st.code(), st.message()))),
            Err(_) => Err(self.failed(format!("timed out after {:?}", option.hard_ttl()))),
        }
    }
}

/// How many leading entries of `sizes` fit in `cap` bytes (at least one).
pub fn entries_that_fit(sizes: &[usize], cap: usize) -> usize {
    let mut total = 0usize;
    for (i, s) in sizes.iter().enumerate() {
        total += s;
        if total > cap {
            return i.max(1);
        }
    }
    sizes.len()
}

type RpcResult<T> = Result<T, RPCError<NodeId, BasicNode, RaftError<NodeId>>>;

impl RaftNetwork<TypeConfig> for GrpcConnection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> RpcResult<AppendEntriesResponse<NodeId>> {
        let req = wire::append_to_pb(&rpc);
        let sizes: Vec<usize> = req.entries.iter().map(Vec::len).collect();
        let fit = entries_that_fit(&sizes, RAFT_RPC_MAX_BYTES);
        if fit < sizes.len() {
            return Err(RPCError::PayloadTooLarge(
                PayloadTooLarge::new_entries_hint(fit as u64),
            ));
        }
        let mut c = self.client()?;
        let resp = self.unary(&option, c.append_entries(req)).await?;
        wire::append_resp_from_pb(resp).map_err(|e| RPCError::Unreachable(self.failed(e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> RpcResult<VoteResponse<NodeId>> {
        let mut c = self.client()?;
        let resp = self
            .unary(&option, c.vote(wire::vote_req_to_pb(&rpc)))
            .await?;
        wire::vote_resp_from_pb(resp).map_err(|e| RPCError::Unreachable(self.failed(e)))
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<NodeId>,
        snapshot: Snapshot<TypeConfig>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<NodeId>, StreamingError<TypeConfig, Fatal<NodeId>>> {
        let path = snapshot.snapshot.path.clone();
        let side = read_sidecar(&path).map_err(|e| self.failed(e))?;
        // Open before streaming: a newer build may remove the file later,
        // and an open handle keeps its bytes readable.
        let file = std::fs::File::open(&path).map_err(|e| self.failed(e))?;
        let header = pb::InstallSnapshotHeader {
            vote: Some(wire::vote_to_pb(&vote)),
            last_log_id: snapshot.meta.last_log_id.as_ref().map(wire::log_id_to_pb),
            membership_json: wire::membership_to_json(&snapshot.meta.last_membership),
            snapshot_id: snapshot.meta.snapshot_id.clone(),
            store_format_version: side.store_format_version,
            extractors_hash: side.extractors_hash.clone(),
            size: side.size,
            sha256: side.sha256.clone(),
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<pb::InstallSnapshotRequest>(4);
        let reader = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut file = file;
            let first = pb::InstallSnapshotRequest {
                msg: Some(pb::install_snapshot_request::Msg::Header(header)),
            };
            if tx.blocking_send(first).is_err() {
                return Ok(());
            }
            let mut buf = vec![0u8; SNAPSHOT_CHUNK_BYTES];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                let msg = pb::InstallSnapshotRequest {
                    msg: Some(pb::install_snapshot_request::Msg::Chunk(buf[..n].to_vec())),
                };
                if tx.blocking_send(msg).is_err() {
                    return Ok(());
                }
            }
        });
        let mut c = self.client()?;
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let r = tokio::select! {
            r = c.install_snapshot(stream) => r,
            closed = cancel => {
                reader.abort();
                return Err(StreamingError::Closed(closed));
            }
        };
        match reader.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(self.failed(format!("reading the snapshot: {e}")).into()),
            Err(e) => return Err(self.failed(format!("snapshot reader: {e}")).into()),
        }
        let resp = r
            .map_err(|st| self.failed(format!("{}: {}", st.code(), st.message())))?
            .into_inner();
        let vote = wire::vote_from_pb(resp.vote).map_err(|e| self.failed(e))?;
        Ok(SnapshotResponse::new(vote))
    }
}

/// Which RPC a [`FaultPlan`] is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcKind {
    AppendEntries,
    Vote,
    Snapshot,
}

#[derive(Debug, Default)]
struct Faults {
    /// Nodes in different groups cannot reach each other; nodes in no
    /// group reach everyone.
    groups: Vec<BTreeSet<NodeId>>,
    drop_append_to: BTreeSet<NodeId>,
}

/// Test fault injection shared by every node of a testbed: each node's
/// [`FaultyNetwork`] asks it before sending an RPC and fails the RPC as
/// `Unreachable` when it says so.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    inner: Arc<Mutex<Faults>>,
}

impl FaultPlan {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut Faults) -> T) -> T {
        f(&mut self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Cut every link between a node of `a` and a node of `b`.
    pub fn partition(&self, a: &[NodeId], b: &[NodeId]) {
        self.with(|f| {
            f.groups = vec![a.iter().copied().collect(), b.iter().copied().collect()];
        });
    }

    /// Remove every fault.
    pub fn heal(&self) {
        self.with(|f| *f = Faults::default());
    }

    /// Fail every `AppendEntries` (replication and heartbeats) to `id`.
    pub fn drop_append_entries_to(&self, id: NodeId) {
        self.with(|f| {
            f.drop_append_to.insert(id);
        });
    }

    /// Whether `from` may send `kind` to `to` right now.
    pub fn allows(&self, from: NodeId, to: NodeId, kind: RpcKind) -> bool {
        self.with(|f| {
            if kind == RpcKind::AppendEntries && f.drop_append_to.contains(&to) {
                return false;
            }
            let group = |n: NodeId| f.groups.iter().position(|g| g.contains(&n));
            match (group(from), group(to)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        })
    }
}

/// A network factory that consults a [`FaultPlan`] before every RPC.
#[derive(Clone)]
pub struct FaultyNetwork<N> {
    pub inner: N,
    pub me: NodeId,
    pub plan: FaultPlan,
}

pub struct FaultyConnection<C> {
    inner: C,
    me: NodeId,
    target: NodeId,
    plan: FaultPlan,
}

impl<C> FaultyConnection<C> {
    fn check(&self, kind: RpcKind) -> Result<(), Unreachable> {
        if self.plan.allows(self.me, self.target, kind) {
            Ok(())
        } else {
            Err(unreachable(
                self.target,
                format!(
                    "testing: {kind:?} from {} dropped by the fault plan",
                    self.me
                ),
            ))
        }
    }
}

impl<N> RaftNetworkFactory<TypeConfig> for FaultyNetwork<N>
where
    N: RaftNetworkFactory<TypeConfig> + Clone,
{
    type Network = FaultyConnection<N::Network>;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        FaultyConnection {
            inner: self.inner.new_client(target, node).await,
            me: self.me,
            target,
            plan: self.plan.clone(),
        }
    }
}

impl<C: RaftNetwork<TypeConfig>> RaftNetwork<TypeConfig> for FaultyConnection<C> {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> RpcResult<AppendEntriesResponse<NodeId>> {
        self.check(RpcKind::AppendEntries)?;
        self.inner.append_entries(rpc, option).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> RpcResult<VoteResponse<NodeId>> {
        self.check(RpcKind::Vote)?;
        self.inner.vote(rpc, option).await
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<NodeId>,
        snapshot: Snapshot<TypeConfig>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<NodeId>, StreamingError<TypeConfig, Fatal<NodeId>>> {
        self.check(RpcKind::Snapshot)?;
        self.inner
            .full_snapshot(vote, snapshot, cancel, option)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_cap_keeps_at_least_one_entry() {
        assert_eq!(entries_that_fit(&[], 10), 0);
        assert_eq!(entries_that_fit(&[4, 4], 10), 2);
        assert_eq!(entries_that_fit(&[4, 4, 4], 10), 2);
        assert_eq!(entries_that_fit(&[40, 4], 10), 1);
    }

    #[test]
    fn fault_plan_partitions_and_drops() {
        let p = FaultPlan::new();
        assert!(p.allows(1, 2, RpcKind::Vote));
        p.partition(&[1], &[2, 3]);
        assert!(!p.allows(1, 2, RpcKind::Vote));
        assert!(!p.allows(3, 1, RpcKind::AppendEntries));
        assert!(p.allows(2, 3, RpcKind::AppendEntries));
        assert!(
            p.allows(4, 1, RpcKind::Vote),
            "an ungrouped node reaches all"
        );
        p.heal();
        assert!(p.allows(1, 2, RpcKind::Vote));
        p.drop_append_entries_to(3);
        assert!(!p.allows(1, 3, RpcKind::AppendEntries));
        assert!(p.allows(1, 3, RpcKind::Vote));
    }
}
