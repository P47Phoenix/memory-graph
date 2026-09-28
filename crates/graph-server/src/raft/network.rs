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
//!   wrong cluster is a busy loop). A transport failure drops the cached
//!   channel so the next attempt reconnects; a plain timeout does not (the
//!   connection is fine, the peer is slow). Each failure is recorded per
//!   peer ([`NetStats`], shown in `Admin.Status` replication) and logged
//!   with its gRPC code, at most once per peer every
//!   [`WARN_EVERY`].
//! * `AppendEntries` and openraft's timeout: openraft 0.9 wraps every
//!   `append_entries` call in a hard timeout of `heartbeat_interval`, with
//!   no separate knob (only snapshots have `install_snapshot_timeout`). An
//!   entry of several MiB may need longer than a heartbeat to cross a slow
//!   link and be fsynced; if dropping our future cancelled the RPC, such an
//!   entry would never arrive (a livelock). So an `AppendEntries` that
//!   carries entries runs as its own task ([`APPEND_TRANSFER_TIMEOUT`]
//!   bounds it), and openraft's timeout only stops the wait: its retry of
//!   the same entries (same vote, previous log id and entry ids) joins the
//!   transfer still under way instead of starting over. Heartbeats (no
//!   entries) are sent directly and meanwhile still reach the follower, so
//!   a long transfer does not cause an election.
//! * Channels are cached per target node id and rebuilt when the member's
//!   address changes or a transport call failed.
//! * [`FaultPlan`] / [`FaultyNetwork`]: test fault injection (partitions,
//!   dropped `AppendEntries`), a wrapper that stage C/D tests reuse.
use super::snapshot_dir::read_sidecar;
use super::types::{NodeId, TypeConfig};
use super::wire;
use crate::paths::{ClusterIdentity, CLUSTER_ID_HEADER, EXTRACTORS_HASH_HEADER};
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

/// Soft cap on the entry bytes of one `AppendEntries` RPC: more entries
/// than fit are answered locally with `PayloadTooLarge` and openraft sends
/// fewer. A single entry larger than the cap (an `IndexChunk` is cut at
/// `RAFT_ENTRY_MAX_BYTES` = 8 MiB, and one file bigger than that is an
/// entry of its own) is sent alone.
pub const RAFT_RPC_MAX_BYTES: usize = 4 << 20;

/// The longest one `AppendEntries` transfer that carries entries may take
/// (it outlives openraft's per-call timeout, see the module docs).
pub const APPEND_TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

/// At most one warning per peer this often.
pub const WARN_EVERY: Duration = Duration::from_secs(10);

/// Per-peer network outcomes (shared by every connection of a node).
#[derive(Clone, Default)]
pub struct NetStats {
    inner: Arc<Mutex<NetStatsInner>>,
}

#[derive(Default)]
struct NetStatsInner {
    peers: HashMap<NodeId, PeerStats>,
    payload_too_large: u64,
    joined_transfers: u64,
}

#[derive(Default)]
struct PeerStats {
    last_error: Option<String>,
    last_warned: Option<std::time::Instant>,
    suppressed: u64,
}

impl NetStats {
    fn with<T>(&self, f: impl FnOnce(&mut NetStatsInner) -> T) -> T {
        f(&mut self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// A failed RPC to `target`: remember it and warn (rate limited).
    fn failed(&self, target: NodeId, what: &str) {
        self.with(|s| {
            let p = s.peers.entry(target).or_default();
            p.last_error = Some(what.to_string());
            let now = std::time::Instant::now();
            if p.last_warned.is_none_or(|t| now.duration_since(t) >= WARN_EVERY) {
                tracing::warn!(
                    target_node = target,
                    error = what,
                    suppressed = p.suppressed,
                    "raft RPC failed"
                );
                p.last_warned = Some(now);
                p.suppressed = 0;
            } else {
                p.suppressed += 1;
            }
        });
    }

    fn succeeded(&self, target: NodeId) {
        self.with(|s| {
            if let Some(p) = s.peers.get_mut(&target) {
                p.last_error = None;
            }
        });
    }

    /// The last error of the last failed RPC to `target`, unless an RPC
    /// succeeded since.
    pub fn last_error(&self, target: NodeId) -> Option<String> {
        self.with(|s| s.peers.get(&target).and_then(|p| p.last_error.clone()))
    }

    /// `AppendEntries` answered locally with `PayloadTooLarge`.
    pub fn payload_too_large(&self) -> u64 {
        self.with(|s| s.payload_too_large)
    }

    /// openraft retries that joined a transfer still under way.
    pub fn joined_transfers(&self) -> u64 {
        self.with(|s| s.joined_transfers)
    }
}

/// Snapshot stream chunk size.
pub const SNAPSHOT_CHUNK_BYTES: usize = 1 << 20;

const NO_LIMIT: usize = usize::MAX;

fn unreachable(target: NodeId, what: impl std::fmt::Display) -> Unreachable {
    Unreachable::new(&std::io::Error::other(format!("node {target}: {what}")))
}

/// Adds the protocol version, this node's cluster id and its extractor
/// version set hash to every call.
#[derive(Clone)]
pub struct RaftHeaders {
    identity: Arc<ClusterIdentity>,
    extractors_hash: Arc<str>,
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
        if let Ok(v) = self.extractors_hash.parse() {
            md.insert(EXTRACTORS_HASH_HEADER, v);
        }
        Ok(req)
    }
}

type Client = RaftClient<InterceptedService<Channel, RaftHeaders>>;

/// openraft's network factory over gRPC.
#[derive(Clone)]
pub struct GrpcNetwork {
    identity: Arc<ClusterIdentity>,
    extractors_hash: Arc<str>,
    channels: Arc<Mutex<HashMap<NodeId, (String, Channel)>>>,
    connect_timeout: Duration,
    stats: NetStats,
}

impl GrpcNetwork {
    pub fn new(identity: Arc<ClusterIdentity>, extractors_hash: &str, stats: NetStats) -> Self {
        Self {
            identity,
            extractors_hash: Arc::from(extractors_hash),
            channels: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout: Duration::from_secs(2),
            stats,
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
            inflight: None,
        }
    }
}

/// What makes two `AppendEntries` the same transfer: the leader's vote,
/// the previous log id, and the first and last entry ids with the count.
type AppendKey = (
    pb::RaftVote,
    Option<pb::RaftLogId>,
    Option<(u64, u64)>,
    Option<(u64, u64)>,
    usize,
);

fn append_key(r: &AppendEntriesRequest<TypeConfig>) -> AppendKey {
    let id = |e: &super::types::Entry| (e.log_id.leader_id.term, e.log_id.index);
    (
        wire::vote_to_pb(&r.vote),
        r.prev_log_id.as_ref().map(wire::log_id_to_pb),
        r.entries.first().map(id),
        r.entries.last().map(id),
        r.entries.len(),
    )
}

type Transfer = tokio::task::JoinHandle<Result<pb::AppendEntriesResponse, String>>;

/// One peer.
pub struct GrpcConnection {
    target: NodeId,
    addr: String,
    net: GrpcNetwork,
    /// The last `AppendEntries` with entries, possibly still under way.
    inflight: Option<(AppendKey, Transfer)>,
}

/// Whether a status is the transport failing (reconnect) rather than the
/// peer answering.
fn is_transport(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable | tonic::Code::Unknown | tonic::Code::Cancelled
    )
}

impl GrpcConnection {
    fn client(&self) -> Result<Client, Unreachable> {
        let ch = self
            .net
            .channel(self.target, &self.addr)
            .map_err(|e| self.failed(e))?;
        let headers = RaftHeaders {
            identity: Arc::clone(&self.net.identity),
            extractors_hash: Arc::clone(&self.net.extractors_hash),
        };
        Ok(RaftClient::with_interceptor(ch, headers)
            .max_decoding_message_size(NO_LIMIT)
            .max_encoding_message_size(NO_LIMIT))
    }

    /// A failed call: record it (and warn, rate limited).
    fn failed(&self, what: impl std::fmt::Display) -> Unreachable {
        let what = what.to_string();
        self.net.stats.failed(self.target, &what);
        unreachable(self.target, what)
    }

    /// A failed call with a status: also drop the channel on a transport
    /// failure so the next call reconnects.
    fn failed_status(&self, st: &tonic::Status) -> Unreachable {
        if is_transport(st.code()) {
            self.net.forget(self.target);
        }
        self.failed(format!("{:?}: {}", st.code(), st.message()))
    }

    /// Run one unary call under the RPC's hard timeout. A timeout keeps
    /// the channel (the connection works, the peer is slow).
    async fn unary<T>(
        &self,
        option: &RPCOption,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T, Unreachable> {
        match tokio::time::timeout(option.hard_ttl(), fut).await {
            Ok(Ok(r)) => {
                self.net.stats.succeeded(self.target);
                Ok(r.into_inner())
            }
            Ok(Err(st)) => Err(self.failed_status(&st)),
            Err(_) => Err(self.failed(format!(
                "DeadlineExceeded: no answer within {:?}",
                option.hard_ttl()
            ))),
        }
    }

    /// Send (or join) an `AppendEntries` that carries entries, as a task
    /// that outlives openraft's timeout on this call.
    async fn transfer(
        &mut self,
        key: AppendKey,
        req: pb::AppendEntriesRequest,
    ) -> Result<pb::AppendEntriesResponse, Unreachable> {
        let joined = matches!(&self.inflight, Some((k, _)) if *k == key);
        if joined {
            self.net.stats.with(|s| s.joined_transfers += 1);
        } else {
            // Another transfer (older entries, an older vote) may still be
            // running: it finishes or times out on its own; its answer is
            // not this call's.
            let mut c = self.client()?;
            let net = self.net.clone();
            let target = self.target;
            let task = tokio::spawn(async move {
                match tokio::time::timeout(APPEND_TRANSFER_TIMEOUT, c.append_entries(req)).await {
                    Ok(Ok(r)) => Ok(r.into_inner()),
                    Ok(Err(st)) => {
                        if is_transport(st.code()) {
                            net.forget(target);
                        }
                        Err(format!("{:?}: {}", st.code(), st.message()))
                    }
                    Err(_) => Err(format!(
                        "DeadlineExceeded: the transfer took over {APPEND_TRANSFER_TIMEOUT:?}"
                    )),
                }
            });
            self.inflight = Some((key, task));
        }
        let (_, task) = self.inflight.as_mut().expect("set above");
        // Cancel-safe: if openraft's timeout drops this future, the task
        // (and the handle in `inflight`) stays for the retry to join.
        let out = task.await;
        self.inflight = None;
        match out {
            Ok(Ok(r)) => {
                self.net.stats.succeeded(self.target);
                Ok(r)
            }
            Ok(Err(e)) => Err(self.failed(e)),
            Err(e) => Err(self.failed(format!("append task: {e}"))),
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
            self.net.stats.with(|s| s.payload_too_large += 1);
            return Err(RPCError::PayloadTooLarge(
                PayloadTooLarge::new_entries_hint(fit as u64),
            ));
        }
        let resp = if req.entries.is_empty() {
            let mut c = self.client()?;
            self.unary(&option, c.append_entries(req)).await?
        } else {
            self.transfer(append_key(&rpc), req).await?
        };
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
            .map_err(|st| self.failed_status(&st))?
            .into_inner();
        let vote = wire::vote_from_pb(resp.vote).map_err(|e| self.failed(e))?;
        self.net.stats.succeeded(self.target);
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
