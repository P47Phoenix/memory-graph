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
//!   bounds it), and openraft's timeout only stops the wait: its retry
//!   with the same vote, previous log id and first entry joins the
//!   transfer still under way instead of starting over (a retry asking
//!   for more entries, the log having grown, gets `PartialSuccess` up to
//!   the last entry the transfer carried; see [`plan`]). Any other
//!   request, and any request under another vote, aborts the transfer
//!   first, so at most one runs per peer and stale ones never split the
//!   link's bandwidth; dropping the connection aborts it too. Heartbeats
//!   (no entries) are sent directly and meanwhile still reach the
//!   follower, so a long transfer does not cause an election.
//! * Channels are cached per target node id and rebuilt when the member's
//!   address changes or a transport call failed.
//! * [`FaultPlan`] / [`FaultyNetwork`]: test fault injection (partitions,
//!   dropped `AppendEntries`), a wrapper that stage C/D tests reuse.
use super::snapshot_dir::read_sidecar;
use super::types::{LogId, NodeId, TypeConfig};
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
    partial_joins: u64,
    inflight_aborted: u64,
}

#[derive(Default)]
struct PeerStats {
    /// The last failure, until entries or a snapshot reach the peer again.
    last_error: Option<String>,
    /// No RPC of any kind (a heartbeat, say) succeeded since that failure.
    failing: bool,
    /// When the peer last answered an RPC of any kind (heartbeats
    /// included): whether it is reachable, whatever a slow transfer does.
    last_answer: Option<std::time::Instant>,
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
            p.failing = true;
            let now = std::time::Instant::now();
            if p.last_warned
                .is_none_or(|t| now.duration_since(t) >= WARN_EVERY)
            {
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

    /// An RPC to `target` succeeded; `delivered`: it carried entries or a
    /// snapshot (a heartbeat does not clear a replication error).
    fn succeeded(&self, target: NodeId, delivered: bool) {
        self.with(|s| {
            let p = s.peers.entry(target).or_default();
            p.failing = false;
            p.last_answer = Some(std::time::Instant::now());
            if delivered {
                p.last_error = None;
            }
        });
    }

    /// `target` answered a liveness probe (see [`probe`]): reachable now,
    /// though no Raft RPC to it completed (one may be in a slow transfer).
    pub fn probed(&self, target: NodeId) {
        self.with(|s| {
            s.peers.entry(target).or_default().last_answer = Some(std::time::Instant::now());
        });
    }

    /// Whether `target` answered an RPC (a heartbeat counts) within
    /// `window`: the quorum-loss check of a pending write (issue #116).
    pub fn answered_within(&self, target: NodeId, window: Duration) -> bool {
        self.with(|s| {
            s.peers
                .get(&target)
                .and_then(|p| p.last_answer)
                .is_some_and(|t| t.elapsed() < window)
        })
    }

    /// The last replication error to `target` worth reporting: while RPCs
    /// to it keep failing, or while it lags (`lag > 0`) and no entries or
    /// snapshot reached it since the error.
    pub fn last_error(&self, target: NodeId, lag: u64) -> Option<String> {
        self.with(|s| {
            s.peers
                .get(&target)
                .filter(|p| p.failing || lag > 0)
                .and_then(|p| p.last_error.clone())
        })
    }

    /// `AppendEntries` answered locally with `PayloadTooLarge`.
    pub fn payload_too_large(&self) -> u64 {
        self.with(|s| s.payload_too_large)
    }

    /// openraft retries that joined a transfer still under way.
    pub fn joined_transfers(&self) -> u64 {
        self.with(|s| s.joined_transfers)
    }

    /// Joins of a transfer that carried fewer entries than the retry asked
    /// for (the log grew meanwhile), answered as a partial success.
    pub fn partial_joins(&self) -> u64 {
        self.with(|s| s.partial_joins)
    }

    /// Transfers still under way that were aborted: superseded by a
    /// request for another vote or another place in the log, or their
    /// connection dropped.
    pub fn inflight_aborted(&self) -> u64 {
        self.with(|s| s.inflight_aborted)
    }
}

/// Whether the server at `addr` answers a `grpc.health.v1` check within
/// `timeout`, on a connection of its own: openraft 0.9 sends a peer no
/// heartbeat while an `AppendEntries` to it is under way, so a slow
/// transfer to a live peer looks like silence to the leader. The quorum
/// loss check (issue #116) asks this before calling a peer gone.
pub async fn probe(addr: &str, timeout: Duration) -> bool {
    let uri = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    let Ok(ep) = Endpoint::from_shared(uri) else {
        return false;
    };
    let check = async {
        let ch = ep
            .connect_timeout(timeout)
            .tcp_nodelay(true)
            .connect()
            .await?;
        tonic_health::pb::health_client::HealthClient::new(ch)
            .check(tonic_health::pb::HealthCheckRequest::default())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    matches!(tokio::time::timeout(timeout, check).await, Ok(Ok(())))
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

/// What identifies an `AppendEntries` transfer: the leader's vote, the
/// previous log id, the first entry's id and the last one's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendKey {
    vote: pb::RaftVote,
    prev: Option<pb::RaftLogId>,
    first: Option<pb::RaftLogId>,
    last: Option<LogId>,
}

impl AppendKey {
    pub fn of(r: &AppendEntriesRequest<TypeConfig>) -> Self {
        Self {
            vote: wire::vote_to_pb(&r.vote),
            prev: r.prev_log_id.as_ref().map(wire::log_id_to_pb),
            first: r.entries.first().map(|e| wire::log_id_to_pb(&e.log_id)),
            last: r.entries.last().map(|e| e.log_id),
        }
    }
}

/// What a new `AppendEntries` does about the transfer still under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Wait for it: same vote, same previous log id, same first entry.
    /// `upto`: it carries fewer entries than asked (openraft's log grew
    /// between the tries), so its success only proves the follower matches
    /// up to this id and is answered as `PartialSuccess(upto)`.
    Join { upto: Option<LogId> },
    /// Abort it (stale: another vote or another place in the log) and send
    /// this one.
    Replace,
}

/// See [`Plan`]. A differing vote never joins: an answer to the old
/// leader term's request is not an answer to this one.
pub fn plan(inflight: &AppendKey, new: &AppendKey) -> Plan {
    let p = plan_unchecked(inflight, new);
    debug_assert!(
        inflight.first != new.first || inflight.last.is_some() == new.last.is_some(),
        "same first entry, but last {:?} vs {:?}",
        inflight.last,
        new.last
    );
    p
}

/// [`plan`] without its debug assertion, so the fallback for the
/// impossible case is testable in a debug build.
fn plan_unchecked(inflight: &AppendKey, new: &AppendKey) -> Plan {
    if inflight.vote != new.vote || inflight.prev != new.prev || inflight.first != new.first {
        return Plan::Replace;
    }
    // The first entries are equal, so either both requests carry entries or
    // neither does: a mixed pair cannot happen. Were it to (a bug), joining
    // could pass a success of no entries off as a success of some, so it is
    // replaced instead (and `plan` trips a debug assertion).
    let shorter = match (&inflight.last, &new.last) {
        (Some(a), Some(b)) => a.index < b.index,
        (None, None) => false,
        _ => return Plan::Replace,
    };
    Plan::Join {
        upto: if shorter { inflight.last } else { None },
    }
}

/// A joined transfer's answer for the request that joined it (see
/// [`Plan::Join`]): a success of fewer entries is partial.
pub fn joined_response(
    r: AppendEntriesResponse<NodeId>,
    upto: Option<LogId>,
) -> AppendEntriesResponse<NodeId> {
    match (r, upto) {
        (AppendEntriesResponse::Success, Some(u)) => AppendEntriesResponse::PartialSuccess(Some(u)),
        (r, _) => r,
    }
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

impl Drop for GrpcConnection {
    /// openraft drops a connection when it stops replicating to the peer
    /// (a new leader term, a membership change): its transfer must not
    /// keep sending.
    fn drop(&mut self) {
        if let Some((_, t)) = self.inflight.take() {
            if !t.is_finished() {
                t.abort();
                self.net.stats.with(|s| s.inflight_aborted += 1);
            }
        }
    }
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
                self.net.stats.succeeded(self.target, false);
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
    /// Returns the answer and, when it joined a transfer of fewer entries,
    /// the last entry that transfer carried (see [`Plan::Join`]).
    async fn transfer(
        &mut self,
        key: AppendKey,
        req: pb::AppendEntriesRequest,
    ) -> Result<(pb::AppendEntriesResponse, Option<LogId>), Unreachable> {
        let decided = self.inflight.as_ref().map(|(k, _)| plan(k, &key));
        let upto = if let Some(Plan::Join { upto }) = decided {
            self.net.stats.with(|s| {
                s.joined_transfers += 1;
                if upto.is_some() {
                    s.partial_joins += 1;
                }
            });
            upto
        } else {
            // A stale transfer (another vote, another place in the log)
            // would split the link's bandwidth with this one: stop it.
            if let Some((_, old)) = self.inflight.take() {
                if !old.is_finished() {
                    old.abort();
                    self.net.stats.with(|s| s.inflight_aborted += 1);
                }
            }
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
            None
        };
        let (_, task) = self.inflight.as_mut().expect("set above");
        // Cancel-safe: if openraft's timeout drops this future, the task
        // (and the handle in `inflight`) stays for the retry to join.
        let out = task.await;
        self.inflight = None;
        match out {
            Ok(Ok(r)) => {
                self.net.stats.succeeded(self.target, true);
                Ok((r, upto))
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
        let (resp, upto) = if req.entries.is_empty() {
            let mut c = self.client()?;
            (self.unary(&option, c.append_entries(req)).await?, None)
        } else {
            self.transfer(AppendKey::of(&rpc), req).await?
        };
        wire::append_resp_from_pb(resp)
            .map(|r| joined_response(r, upto))
            .map_err(|e| RPCError::Unreachable(self.failed(e)))
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
        let resp = r.map_err(|st| self.failed_status(&st))?.into_inner();
        let vote = wire::vote_from_pb(resp.vote).map_err(|e| self.failed(e))?;
        self.net.stats.succeeded(self.target, true);
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

    /// Whether a partition leaves `from` and `to` connected (the
    /// forwarding of client requests to the leader consults it too, so a
    /// partitioned node cannot reach the leader by the back door).
    pub fn connected(&self, from: NodeId, to: NodeId) -> bool {
        self.with(|f| {
            let group = |n: NodeId| f.groups.iter().position(|g| g.contains(&n));
            match (group(from), group(to)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        })
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

    fn lid(term: u64, index: u64) -> LogId {
        LogId::new(openraft::CommittedLeaderId::new(term, 1), index)
    }

    fn req(term: u64, prev: u64, first: u64, last: u64) -> AppendEntriesRequest<TypeConfig> {
        AppendEntriesRequest {
            vote: Vote::new_committed(term, 1),
            prev_log_id: Some(lid(1, prev)),
            leader_commit: None,
            entries: (first..=last)
                .map(|i| super::super::types::Entry {
                    log_id: lid(1, i),
                    payload: openraft::EntryPayload::Blank,
                })
                .collect(),
        }
    }

    #[test]
    fn a_retry_joins_only_the_same_transfer() {
        let k = |t, p, f, l| AppendKey::of(&req(t, p, f, l));
        // The same request, or a longer one (the log grew): join; the
        // longer one's success is partial, up to what was sent.
        assert_eq!(
            plan(&k(2, 4, 5, 8), &k(2, 4, 5, 8)),
            Plan::Join { upto: None }
        );
        assert_eq!(
            plan(&k(2, 4, 5, 8), &k(2, 4, 5, 12)),
            Plan::Join {
                upto: Some(lid(1, 8))
            }
        );
        assert_eq!(
            plan(&k(2, 4, 5, 12), &k(2, 4, 5, 8)),
            Plan::Join { upto: None }
        );
        // Another place in the log: replace.
        assert_eq!(plan(&k(2, 4, 5, 8), &k(2, 8, 9, 12)), Plan::Replace);
        // A vote change never reuses the old answer, same entries or not.
        assert_eq!(plan(&k(2, 4, 5, 8), &k(3, 4, 5, 8)), Plan::Replace);
        // A joined success of fewer entries is partial; other answers pass.
        assert_eq!(
            joined_response(AppendEntriesResponse::Success, Some(lid(1, 8))),
            AppendEntriesResponse::PartialSuccess(Some(lid(1, 8)))
        );
        assert_eq!(
            joined_response(AppendEntriesResponse::Conflict, Some(lid(1, 8))),
            AppendEntriesResponse::Conflict
        );
        assert_eq!(
            joined_response(AppendEntriesResponse::Success, None),
            AppendEntriesResponse::Success
        );
        // A higher vote is passed through as is, partial join or not: the
        // leader must step down, not count a match.
        let hv = || AppendEntriesResponse::HigherVote(Vote::new_committed(5, 2));
        assert_eq!(joined_response(hv(), Some(lid(1, 8))), hv());
        assert_eq!(joined_response(hv(), None), hv());
        // A partial answer from the follower itself stays what it was.
        assert_eq!(
            joined_response(
                AppendEntriesResponse::PartialSuccess(Some(lid(1, 6))),
                Some(lid(1, 8))
            ),
            AppendEntriesResponse::PartialSuccess(Some(lid(1, 6)))
        );
    }

    /// Two heartbeats (no entries) with the same vote and previous log id
    /// join with no `upto`.
    #[test]
    fn heartbeats_join_as_full() {
        let hb = || {
            let mut r = req(2, 4, 5, 4);
            r.entries.clear();
            AppendKey::of(&r)
        };
        assert_eq!(plan(&hb(), &hb()), Plan::Join { upto: None });
    }

    /// Entries against none with the same first entry cannot happen; the
    /// fallback replaces, never joins as a success.
    #[test]
    fn a_mixed_pair_is_replaced() {
        let with = AppendKey::of(&req(2, 4, 5, 8));
        let mut without = with.clone();
        without.last = None;
        assert_eq!(plan_unchecked(&without, &with), Plan::Replace);
        assert_eq!(plan_unchecked(&with, &without), Plan::Replace);
    }

    /// Same vote and previous log id, another first entry: replace.
    #[test]
    fn another_first_entry_is_replaced() {
        let a = AppendKey::of(&req(2, 4, 5, 8));
        let mut b = a.clone();
        b.first = AppendKey::of(&req(2, 4, 6, 8)).first;
        assert_eq!(plan(&a, &b), Plan::Replace);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "same first entry")]
    fn a_mixed_pair_asserts_in_debug() {
        let with = AppendKey::of(&req(2, 4, 5, 8));
        let mut without = with.clone();
        without.last = None;
        let _ = plan(&without, &with);
    }

    /// A peer that accepts TCP connections and never answers: every
    /// transfer to it stays under way until aborted.
    async fn silent_peer() -> (String, tokio::task::JoinHandle<()>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s);
            }
        });
        (addr, h)
    }

    fn connection(addr: &str, stats: &NetStats) -> GrpcConnection {
        GrpcConnection {
            target: 2,
            addr: addr.to_string(),
            net: GrpcNetwork::new(
                Arc::new(crate::paths::ClusterIdentity::fixed("c")),
                "h",
                stats.clone(),
            ),
            inflight: None,
        }
    }

    /// Start `r` as openraft does and give up waiting after a moment (its
    /// per-call timeout): the transfer stays under way in `inflight`.
    async fn start(c: &mut GrpcConnection, r: AppendEntriesRequest<TypeConfig>) {
        let key = AppendKey::of(&r);
        let pb = wire::append_to_pb(&r);
        let waited = tokio::time::timeout(Duration::from_millis(50), c.transfer(key, pb)).await;
        assert!(waited.is_err(), "a silent peer never answers");
    }

    fn inflight_handle(c: &GrpcConnection) -> tokio::task::AbortHandle {
        c.inflight.as_ref().expect("under way").1.abort_handle()
    }

    async fn wait_finished(h: &tokio::task::AbortHandle) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !h.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the aborted transfer ended");
    }

    /// A retry after a vote change does not join (and so never takes) the
    /// old term's answer: the old transfer is aborted and a new one sent.
    /// A retry of the same request joins. Dropping the connection aborts
    /// what is under way.
    #[tokio::test]
    async fn stale_transfers_are_aborted_and_a_vote_change_never_joins() {
        let (addr, peer) = silent_peer().await;
        let stats = NetStats::default();
        let mut c = connection(&addr, &stats);
        start(&mut c, req(2, 4, 5, 8)).await;
        let first = inflight_handle(&c);
        start(&mut c, req(2, 4, 5, 10)).await;
        assert_eq!(stats.joined_transfers(), 1);
        assert_eq!(stats.partial_joins(), 1);
        assert_eq!(inflight_handle(&c).id(), first.id(), "joined, not resent");
        assert!(!first.is_finished());
        start(&mut c, req(3, 4, 5, 8)).await;
        wait_finished(&first).await;
        assert_eq!(stats.inflight_aborted(), 1);
        assert_eq!(stats.joined_transfers(), 1, "a new vote did not join");
        let second = inflight_handle(&c);
        assert_ne!(second.id(), first.id());
        drop(c);
        wait_finished(&second).await;
        assert_eq!(stats.inflight_aborted(), 2, "the drop aborted it");
        peer.abort();
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
