//! Observability (ADR 0004 D10, stage E): the metrics a node keeps, their
//! Prometheus text rendering (`serve --metrics-listen`, `Admin.Metrics`),
//! the per-RPC tower layer (a tracing span and the RPC histogram for every
//! gRPC call) and the tiny HTTP/1.1 responder that serves `/metrics`.
//!
//! Why a hand-written responder rather than hyper's HTTP/1 server: hyper is
//! already in the tree (tonic's transport), but only with its HTTP/2
//! server; `/metrics` needs `GET` of one path, `Connection: close`, and
//! nothing else, which is ~80 lines over a tokio `TcpListener` with a read
//! timeout and a request size cap. That keeps the dependency set (and the
//! pure-Rust gate's surface) unchanged and avoids hyper-util's server glue.
//!
//! Metric names are a contract (dashboards and alerts key on them), listed
//! in [`METRIC_NAMES`] and documented in docs/guide/observability.md.
use crate::conn::ConnInfo;
use crate::server::ShutdownHandle;
use crate::services::Ctx;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::codegen::http;
use tracing::Instrument;

/// Histogram bucket upper bounds, seconds (`mg_rpc_duration_seconds`,
/// `mg_apply_duration_seconds`): 0.5 ms to 10 s.
pub const DURATION_BUCKETS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Every metric family `/metrics` exports (the contract).
pub const METRIC_NAMES: [&str; 29] = [
    "mg_raft_term",
    "mg_raft_leader_id",
    "mg_raft_role",
    "mg_raft_last_log_index",
    "mg_raft_committed_index",
    "mg_raft_applied_index",
    "mg_raft_snapshot_index",
    "mg_raft_purged_index",
    "mg_raft_replication_lag",
    "mg_store_bytes",
    "mg_log_bytes",
    "mg_snapshot_handles_open",
    "mg_rpc_duration_seconds",
    "mg_rpc_total",
    "mg_writes_forwarded_total",
    "mg_quorum_probes_total",
    "mg_apply_duration_seconds",
    "mg_build_info",
    "mg_backup_last_success_timestamp",
    "mg_backup_last_index",
    "mg_backup_failures_total",
    "mg_backup_bytes_total",
    "mg_read_decodes_total",
    "mg_read_decode_bytes_total",
    "mg_read_decode_nanoseconds_total",
    "mg_read_queries_total",
    "mg_read_query_nanoseconds_total",
    "mg_read_txns_total",
    "mg_read_dict_strings_total",
];

/// The Prometheus text exposition format this module writes.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A fixed-bucket histogram ([`DURATION_BUCKETS`]); counts per bucket are
/// not cumulative here, [`Histogram::render`] accumulates.
#[derive(Debug, Clone, Default)]
pub struct Histogram {
    counts: [u64; DURATION_BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl Histogram {
    pub fn observe(&mut self, secs: f64) {
        let secs = if secs.is_finite() && secs > 0.0 {
            secs
        } else {
            0.0
        };
        if let Some(i) = DURATION_BUCKETS.iter().position(|b| secs <= *b) {
            self.counts[i] += 1;
        }
        self.count += 1;
        self.sum += secs;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// `<name>_bucket{..,le=".."}` (cumulative), `_sum`, `_count`.
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut acc = 0;
        for (b, c) in DURATION_BUCKETS.iter().zip(self.counts) {
            acc += c;
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {acc}");
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}",
            self.count
        );
        let braces = |s: &str| {
            if s.is_empty() {
                String::new()
            } else {
                format!("{{{s}}}")
            }
        };
        let _ = writeln!(out, "{name}_sum{} {}", braces(labels), self.sum);
        let _ = writeln!(out, "{name}_count{} {}", braces(labels), self.count);
    }
}

/// What a node measures itself (shared by the RPC layer, the state
/// machine, the Raft service and the readiness task).
#[derive(Default)]
pub struct Observability {
    /// `(rpc, outcome)` -> duration histogram (its count is `mg_rpc_total`).
    rpc: Mutex<BTreeMap<(String, String), Histogram>>,
    apply: Mutex<Histogram>,
    /// The highest `leader_commit` any leader's `AppendEntries` (heartbeats
    /// included) told this node: what readiness compares the applied index
    /// with on a follower or learner.
    leader_commit: AtomicU64,
    leader_commit_changed: tokio::sync::Notify,
    /// The last leader contact: when a leader last reached this node (an
    /// accepted `AppendEntries`, heartbeats included, or `InstallSnapshot`)
    /// and the `leader_commit` the last accepted `AppendEntries` carried.
    /// The one record both readiness (D10: a follower or learner that has
    /// not heard from a leader for a while is partitioned and not ready)
    /// and a `LOCAL` read's `stale_possible` (D8, see
    /// [`crate::raft::node::RaftNode::read_meta`]) judge by.
    last_contact: Mutex<Option<(Instant, Option<u64>)>>,
    /// `(tool, outcome)` -> MCP `tools/call`s on `--mcp-listen`.
    mcp_calls: Mutex<BTreeMap<(String, String), u64>>,
}

impl Observability {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn observe_rpc(&self, rpc: &str, outcome: &str, secs: f64) {
        let mut m = self.rpc.lock().unwrap_or_else(PoisonError::into_inner);
        m.entry((rpc.to_string(), outcome.to_string()))
            .or_default()
            .observe(secs);
    }

    /// One MCP `tools/call` of `tool` (a known tool name or `unknown`)
    /// ended with `outcome` (`ok`, `error`, `rejected`, `timeout`,
    /// `refused`, `internal`).
    pub fn observe_mcp_call(&self, tool: &str, outcome: &str) {
        let mut m = self
            .mcp_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *m.entry((tool.to_string(), outcome.to_string()))
            .or_default() += 1;
    }

    /// MCP `tools/call`s so far, by `(tool, outcome)`.
    pub fn mcp_calls(&self) -> BTreeMap<(String, String), u64> {
        self.mcp_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn observe_apply(&self, secs: f64) {
        self.apply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(secs);
    }

    /// RPCs served since start, every method and outcome.
    pub fn rpc_total(&self) -> u64 {
        let m = self.rpc.lock().unwrap_or_else(PoisonError::into_inner);
        m.values().map(Histogram::count).sum()
    }

    /// Entries applied (timed) since start.
    pub fn apply_total(&self) -> u64 {
        self.apply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .count()
    }

    /// A leader said it has committed up to `index` (monotonic max).
    pub fn note_leader_commit(&self, index: u64) {
        if self.leader_commit.fetch_max(index, Ordering::SeqCst) < index {
            self.leader_commit_changed.notify_waiters();
        }
    }

    pub fn leader_commit(&self) -> u64 {
        self.leader_commit.load(Ordering::SeqCst)
    }

    /// This node accepted a leader's `AppendEntries` (heartbeats included)
    /// just now, carrying `leader_commit`: record the contact and note the
    /// commit index ([`note_leader_commit`](Self::note_leader_commit)).
    pub fn note_leader_contact(&self, leader_commit: Option<u64>) {
        *self
            .last_contact
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), leader_commit));
        if let Some(c) = leader_commit {
            self.note_leader_commit(c);
        }
    }

    /// A leader's `InstallSnapshot` arrived just now (it carries no commit
    /// index: the last one heard is kept).
    pub fn note_heard_from_leader(&self) {
        let mut last = self
            .last_contact
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let commit = last.and_then(|(_, c)| c);
        *last = Some((Instant::now(), commit));
    }

    /// When a leader last reached this node, and the commit index its last
    /// accepted `AppendEntries` carried (`None`: never).
    pub fn last_leader_contact(&self) -> Option<(Instant, Option<u64>)> {
        *self
            .last_contact
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Time since a leader last reached this node (`None`: never).
    pub fn since_heard_from_leader(&self) -> Option<Duration> {
        self.last_leader_contact().map(|(t, _)| t.elapsed())
    }

    /// Resolves the next time [`note_leader_commit`](Self::note_leader_commit)
    /// raises the value.
    pub async fn leader_commit_changed(&self) {
        self.leader_commit_changed.notified().await
    }
}

/// How long a follower or learner stays ready without hearing from a
/// leader: three maximum election timeouts. The leader sends
/// `AppendEntries` (heartbeats when idle) to every voter and learner each
/// heartbeat interval, far more often than that, so only a node the leader
/// cannot reach (a partition) goes this long.
pub fn leader_silence_limit(election_max_ms: u64) -> Duration {
    Duration::from_millis(election_max_ms.saturating_mul(3))
}

/// The inputs of [`is_ready`].
#[derive(Debug, Clone, Copy)]
pub struct Readiness {
    pub leader_known: bool,
    pub is_leader: bool,
    pub applied: u64,
    /// The leader's committed index, as its `AppendEntries` said.
    pub leader_commit: u64,
    /// Time since a leader last reached this node (`None`: never).
    pub since_heard: Option<Duration>,
}

/// Whether `memory-graph.ready` is `SERVING` (D10). The leader: always,
/// once it leads. A follower or learner: a leader is known, one reached
/// it within `silence_limit` ([`leader_silence_limit`]; a partitioned node
/// keeps its last known leader and would otherwise stay ready), and its
/// applied index is within `max_lag` entries of the leader's committed
/// index (a learner still catching up is not ready).
pub fn is_ready(r: Readiness, max_lag: u64, silence_limit: Duration) -> bool {
    if !r.leader_known {
        return false;
    }
    if r.is_leader {
        return true;
    }
    r.since_heard.is_some_and(|d| d <= silence_limit)
        && r.leader_commit.saturating_sub(r.applied) <= max_lag
}

/// Escape a label value (exposition format 0.0.4).
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn head(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn gauge(out: &mut String, name: &str, help: &str, v: u64) {
    head(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {v}");
}

/// The whole `/metrics` document of this node.
pub fn render(ctx: &Ctx) -> String {
    let m = ctx.raft.metrics();
    let obs = &ctx.raft.obs;
    let mut out = String::with_capacity(8 * 1024);
    let applied = m.last_applied.as_ref().map_or(0, |l| l.index);
    let last_log = m.last_log_index.unwrap_or(0);
    let committed = ctx
        .raft
        .log_store
        .committed_index()
        .unwrap_or(0)
        .max(applied);
    gauge(
        &mut out,
        "mg_raft_term",
        "Current Raft term.",
        m.current_term,
    );
    gauge(
        &mut out,
        "mg_raft_leader_id",
        "Node id of the known leader (0: none).",
        ctx.raft.leader().id.unwrap_or(0),
    );
    head(
        &mut out,
        "mg_raft_role",
        "gauge",
        "1 for this node's current Raft role.",
    );
    let current = crate::services::admin::role(m.state);
    for r in ["leader", "follower", "candidate", "learner", "shutdown"] {
        let _ = writeln!(
            out,
            "mg_raft_role{{role=\"{r}\"}} {}",
            u8::from(r == current)
        );
    }
    gauge(
        &mut out,
        "mg_raft_last_log_index",
        "Index of the last entry in this node's Raft log.",
        last_log,
    );
    gauge(
        &mut out,
        "mg_raft_committed_index",
        "Last log index this node knows to be committed.",
        committed,
    );
    gauge(
        &mut out,
        "mg_raft_applied_index",
        "Last log index applied to the store.",
        applied,
    );
    gauge(
        &mut out,
        "mg_raft_snapshot_index",
        "Log index of the current snapshot (0: none).",
        m.snapshot.map_or(0, |s| s.index),
    );
    gauge(
        &mut out,
        "mg_raft_purged_index",
        "Last purged log index (0: none).",
        m.purged.map_or(0, |p| p.index),
    );
    head(
        &mut out,
        "mg_raft_replication_lag",
        "gauge",
        "Leader only: entries each peer's matched index is behind the leader's last log index.",
    );
    if let Some(rep) = &m.replication {
        for (id, matched) in rep.iter().filter(|(id, _)| **id != ctx.info.node_id) {
            let lag = last_log.saturating_sub(matched.as_ref().map_or(0, |l| l.index));
            let _ = writeln!(out, "mg_raft_replication_lag{{peer=\"{id}\"}} {lag}");
        }
    }
    gauge(
        &mut out,
        "mg_store_bytes",
        "Size of the store file on disk.",
        std::fs::metadata(ctx.slot.path()).map_or(0, |m| m.len()),
    );
    gauge(
        &mut out,
        "mg_log_bytes",
        "Size of the Raft log file on disk.",
        ctx.raft.log_bytes(),
    );
    gauge(
        &mut out,
        "mg_snapshot_handles_open",
        "Open snapshot handles held for paging clients.",
        ctx.slot.snapshots().len() as u64,
    );
    {
        let rpc = obs.rpc.lock().unwrap_or_else(PoisonError::into_inner);
        head(
            &mut out,
            "mg_rpc_duration_seconds",
            "histogram",
            "gRPC call duration until the response headers, by method and outcome.",
        );
        for ((name, outcome), h) in rpc.iter() {
            let labels = format!("rpc=\"{}\",outcome=\"{}\"", label(name), label(outcome));
            h.render(&mut out, "mg_rpc_duration_seconds", &labels);
        }
        head(
            &mut out,
            "mg_rpc_total",
            "counter",
            "gRPC calls served, by method and outcome.",
        );
        for ((name, outcome), h) in rpc.iter() {
            let _ = writeln!(
                out,
                "mg_rpc_total{{rpc=\"{}\",outcome=\"{}\"}} {}",
                label(name),
                label(outcome),
                h.count()
            );
        }
    }
    head(
        &mut out,
        "mg_writes_forwarded_total",
        "counter",
        "Writes and membership changes this node forwarded to the leader.",
    );
    let _ = writeln!(
        out,
        "mg_writes_forwarded_total {}",
        ctx.fwd.forwarded_total()
    );
    head(
        &mut out,
        "mg_quorum_probes_total",
        "counter",
        "Leader liveness probes of silent voters while a write waited (quorum-loss check), by outcome.",
    );
    let (alive, dead) = ctx.raft.net_stats.probes();
    let _ = writeln!(out, "mg_quorum_probes_total{{outcome=\"alive\"}} {alive}");
    let _ = writeln!(out, "mg_quorum_probes_total{{outcome=\"dead\"}} {dead}");
    head(
        &mut out,
        "mg_mcp_tool_calls_total",
        "counter",
        "MCP tools/call requests on --mcp-listen, by tool and outcome (ok, error: an isError result, rejected: a JSON-RPC error, timeout: past the per-call deadline, refused: past --mcp-max-inflight (429), internal: the call itself failed).",
    );
    for ((tool, outcome), n) in obs.mcp_calls() {
        let _ = writeln!(
            out,
            "mg_mcp_tool_calls_total{{tool=\"{}\",outcome=\"{}\"}} {n}",
            label(&tool),
            label(&outcome)
        );
    }
    head(
        &mut out,
        "mg_apply_duration_seconds",
        "histogram",
        "Time to apply one committed log entry to the store.",
    );
    obs.apply
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .render(&mut out, "mg_apply_duration_seconds", "");
    head(
        &mut out,
        "mg_build_info",
        "gauge",
        "Always 1; the labels name the server version, protocol and store format.",
    );
    let _ = writeln!(
        out,
        "mg_build_info{{version=\"{}\",protocol=\"{}\",store_format=\"{}\"}} 1",
        label(crate::SERVER_VERSION),
        graph_proto::PROTOCOL_VERSION,
        graph_store::SCHEMA_VERSION
    );
    let b = ctx.backup.as_ref().map(|b| b.stats()).unwrap_or_default();
    gauge(
        &mut out,
        "mg_backup_last_success_timestamp",
        "Unix seconds of the last snapshot backup committed (0: none).",
        b.last_success_unix,
    );
    gauge(
        &mut out,
        "mg_backup_last_index",
        "Log index of the last snapshot backup committed (0: none).",
        b.last_index,
    );
    head(
        &mut out,
        "mg_backup_failures_total",
        "counter",
        "Snapshot backups that failed after every retry.",
    );
    let _ = writeln!(out, "mg_backup_failures_total {}", b.failures_total);
    head(
        &mut out,
        "mg_backup_bytes_total",
        "counter",
        "Bytes written by successful snapshot backups.",
    );
    let _ = writeln!(out, "mg_backup_bytes_total {}", b.bytes_total);
    render_read_stats(&mut out, &graph_store::read_stats::snapshot());
    out
}

fn counter(out: &mut String, name: &str, help: &str, v: u64) {
    head(out, name, "counter", help);
    let _ = writeln!(out, "{name} {v}");
}

/// A counter family with one sample per decode kind.
fn per_kind(out: &mut String, name: &str, help: &str, by_kind: [(&str, u64); 4]) {
    head(out, name, "counter", help);
    for (kind, v) in by_kind {
        let _ = writeln!(out, "{name}{{kind=\"{kind}\"}} {v}");
    }
}

/// The read-path counters (ADR 0008 phase 0). They are process-wide: every
/// store in this process adds to them. Nanosecond families stay at zero
/// unless `--read-timing` is on.
fn render_read_stats(out: &mut String, r: &graph_store::read_stats::ReadStats) {
    per_kind(
        out,
        "mg_read_decodes_total",
        "Decodes done by queries, by kind (dict: reverse-dictionary block scans; symbol: symbol sections; lazy: stream headers; full: whole streams).",
        [
            ("dict", r.dict_block_decodes),
            ("symbol", r.symbol_section_decodes),
            ("lazy", r.lazy_stream_decodes),
            ("full", r.full_stream_decodes),
        ],
    );
    per_kind(
        out,
        "mg_read_decode_bytes_total",
        "Encoded bytes read by query decodes, by kind.",
        [
            ("dict", r.dict_bytes),
            ("symbol", r.symbol_bytes),
            ("lazy", r.lazy_bytes),
            ("full", r.full_bytes),
        ],
    );
    // Symbol-section and lazy-header decodes share one timer in the store.
    per_kind(
        out,
        "mg_read_decode_nanoseconds_total",
        "Nanoseconds inside query decodes, by kind (--read-timing only; symbol-section time is reported under lazy).",
        [
            ("dict", r.dict_decode_nanos),
            ("symbol", 0),
            ("lazy", r.lazy_decode_nanos),
            ("full", r.full_decode_nanos),
        ],
    );
    counter(
        out,
        "mg_read_queries_total",
        "Store read calls (queries) served by this process.",
        r.queries,
    );
    counter(
        out,
        "mg_read_query_nanoseconds_total",
        "Wall nanoseconds inside store read calls (--read-timing only).",
        r.query_nanos,
    );
    counter(
        out,
        "mg_read_txns_total",
        "Read transactions opened by store read calls (snapshot reads reuse one).",
        r.read_txns,
    );
    counter(
        out,
        "mg_read_dict_strings_total",
        "Dictionary strings allocated by query-side term lookups.",
        r.dict_strings_decoded,
    );
}

// ---------------------------------------------------------------------------
// The per-RPC layer.

/// The `rpc` label of every path that is not a known method.
pub const UNKNOWN_RPC: &str = "unknown";

/// The standard health methods (tonic-health), besides
/// [`graph_proto::rpc_paths`].
const HEALTH_PATHS: [&str; 2] = [
    "/grpc.health.v1.Health/Check",
    "/grpc.health.v1.Health/Watch",
];

/// `/memory_graph.v1.Store/Search` -> `Store/Search`,
/// `/grpc.health.v1.Health/Check` -> `Health/Check`; any other path (a
/// client can send anything) is [`UNKNOWN_RPC`], so label cardinality stays
/// bounded by the methods this server defines.
pub fn rpc_name(path: &str) -> String {
    if !(graph_proto::rpc_paths().contains(path) || HEALTH_PATHS.contains(&path)) {
        return UNKNOWN_RPC.to_string();
    }
    let path = path.trim_start_matches('/');
    match path.split_once('/') {
        Some((svc, method)) => {
            let svc = svc.rsplit('.').next().unwrap_or(svc);
            format!("{svc}/{method}")
        }
        None => UNKNOWN_RPC.to_string(),
    }
}

/// The outcome label of a response: the gRPC status of a trailers-only
/// response (every error tonic answers before a body), else `ok`. An error
/// a stream reports in its trailers after the headers is not seen here.
fn outcome_of(headers: &http::HeaderMap) -> String {
    match headers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i32>().ok())
    {
        None | Some(0) => "ok".into(),
        Some(n) => snake(&format!("{:?}", tonic::Code::from_i32(n))),
    }
}

fn snake(camel: &str) -> String {
    let mut s = String::with_capacity(camel.len() + 4);
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

/// A tower layer for the tonic server: every call gets an `rpc` span
/// (method, peer, outcome, duration_ms), a `debug` event when it ends, and
/// an observation in `mg_rpc_duration_seconds` / `mg_rpc_total`.
#[derive(Clone)]
pub struct RpcLayer {
    obs: Arc<Observability>,
}

impl RpcLayer {
    pub fn new(obs: Arc<Observability>) -> Self {
        Self { obs }
    }
}

impl<S> tower_layer::Layer<S> for RpcLayer {
    type Service = RpcService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RpcService {
            inner,
            obs: Arc::clone(&self.obs),
        }
    }
}

#[derive(Clone)]
pub struct RpcService<S> {
    inner: S,
    obs: Arc<Observability>,
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, B, RB> tower_service::Service<http::Request<B>> for RpcService<S>
where
    S: tower_service::Service<http::Request<B>, Response = http::Response<RB>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = http::Response<RB>;
    type Error = S::Error;
    type Future = BoxFut<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        // The clone that was polled ready serves this call (tower's rule).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let rpc = rpc_name(req.uri().path());
        let peer = req
            .extensions()
            .get::<ConnInfo>()
            .and_then(|c| c.peer)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let obs = Arc::clone(&self.obs);
        let span = tracing::info_span!(
            "rpc",
            method = %rpc,
            peer = %peer,
            outcome = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let fut = {
            let span = span.clone();
            async move {
                let t = Instant::now();
                let r = inner.call(req).await;
                let secs = t.elapsed().as_secs_f64();
                let outcome = match &r {
                    Ok(resp) => outcome_of(resp.headers()),
                    Err(_) => "transport_error".into(),
                };
                span.record("outcome", outcome.as_str());
                span.record("duration_ms", secs * 1000.0);
                obs.observe_rpc(&rpc, &outcome, secs);
                tracing::debug!("rpc finished");
                r
            }
        };
        Box::pin(fut.instrument(span))
    }
}

// ---------------------------------------------------------------------------
// The `/metrics` HTTP/1.1 responder.

/// Longest request head accepted (a scraper sends a few hundred bytes).
const MAX_REQUEST: usize = 16 * 1024;
/// A connection that has not sent a full request head by then is dropped.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at once; one accepted beyond that is closed at once
/// (a flood of idle connections holds at most this many tasks, each for at
/// most [`READ_TIMEOUT`] plus the time to write the answer).
///
/// Starvation bound: every connection holds its slot for at most
/// `2 * READ_TIMEOUT` (10 s, the whole-connection timeout in
/// [`serve_metrics`]), so a flood of idle or never-reading clients can keep
/// a legitimate scrape out for at most that long per wave of slots, not
/// forever; a scraper that retries on its interval (15-60 s typically) gets
/// through as soon as a slot frees. The flood cannot touch the gRPC port,
/// which has its own listener. 64 is far above what any number of real
/// scrapers opens at once (one connection each per interval).
pub const MAX_CONNECTIONS: usize = 64;

/// Serve `GET /metrics` on `listener` until `shutdown`; every connection is
/// answered once and closed (`Connection: close`).
pub async fn serve_metrics(listener: TcpListener, ctx: Arc<Ctx>, shutdown: ShutdownHandle) {
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.wait() => return,
        };
        match accepted {
            Ok((stream, peer)) => {
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::debug!(%peer, "metrics: too many connections open; closing this one");
                    drop(stream);
                    continue;
                };
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move {
                    // Writing the answer is bounded too (a peer that
                    // never reads).
                    match tokio::time::timeout(READ_TIMEOUT * 2, answer(stream, &ctx)).await {
                        Ok(Err(e)) => tracing::debug!(error = %e, "metrics connection"),
                        Err(_) => tracing::debug!("metrics connection timed out"),
                        Ok(Ok(())) => {}
                    }
                    drop(permit);
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "metrics listener accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The parsed request line: `(method, path)` without the query string.
pub fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut parts = line.split_ascii_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return None;
    }
    let path = target.split(['?', '#']).next().unwrap_or(target);
    Some((method, path))
}

async fn answer(mut stream: TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let complete = tokio::time::timeout(READ_TIMEOUT, async {
        let mut chunk = [0u8; 1024];
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") {
                return Ok(true);
            }
            if buf.len() > MAX_REQUEST {
                return Ok(false);
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(false);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap_or(Ok::<bool, std::io::Error>(false))?;
    let head = String::from_utf8_lossy(&buf);
    let (status, ctype, body, head_only) = match complete.then(|| parse_request_line(&head)) {
        Some(Some((m @ ("GET" | "HEAD"), "/metrics"))) => {
            ("200 OK", CONTENT_TYPE, render(ctx), m == "HEAD")
        }
        Some(Some(("GET" | "HEAD", _))) => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found: metrics are at /metrics\n".to_string(),
            false,
        ),
        Some(Some(_)) => (
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "only GET /metrics\n".to_string(),
            false,
        ),
        _ => (
            "400 Bad Request",
            "text/plain; charset=utf-8",
            "bad request\n".to_string(),
            false,
        ),
    };
    let mut resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if status.starts_with("405") {
        resp.push_str("Allow: GET, HEAD\r\n");
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes()).await?;
    if !head_only {
        stream.write_all(body.as_bytes()).await?;
    }
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_names_drop_the_package() {
        assert_eq!(rpc_name("/memory_graph.v1.Store/Search"), "Store/Search");
        assert_eq!(rpc_name("/grpc.health.v1.Health/Check"), "Health/Check");
        assert_eq!(
            rpc_name("/memory_graph.v1.Raft/AppendEntries"),
            "Raft/AppendEntries"
        );
        assert_eq!(rpc_name("/memory_graph.v1.Admin/Metrics"), "Admin/Metrics");
        assert_eq!(rpc_name("/grpc.health.v1.Health/Watch"), "Health/Watch");
        for odd in [
            "/odd",
            "",
            "/",
            "/memory_graph.v1.Store/NoSuchMethod",
            "/memory_graph.v1.Nope/Search",
            "/x.y.Store/Search",
            "/memory_graph.v1.Store/Search/extra",
        ] {
            assert_eq!(rpc_name(odd), UNKNOWN_RPC, "{odd}");
        }
        // Random client-chosen paths never make new label values.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..500 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let p = format!("/memory_graph.v1.Store/{seed:x}");
            assert_eq!(rpc_name(&p), UNKNOWN_RPC, "{p}");
        }
        assert!(graph_proto::rpc_paths().len() > 30);
        assert!(graph_proto::rpc_paths().contains("/memory_graph.v1.Write/Index"));
    }

    /// A service answering an empty `200` to everything.
    #[derive(Clone)]
    struct Echo;

    impl tower_service::Service<http::Request<()>> for Echo {
        type Response = http::Response<()>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<http::Response<()>, std::convert::Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: http::Request<()>) -> Self::Future {
            std::future::ready(Ok(http::Response::new(())))
        }
    }

    #[tokio::test]
    async fn the_rpc_layer_labels_unknown_paths_unknown() {
        use tower_layer::Layer as _;
        use tower_service::Service as _;
        let obs = Observability::new();
        let mut svc = RpcLayer::new(Arc::clone(&obs)).layer(Echo);
        let paths = [
            "/memory_graph.v1.Store/Search".to_string(),
            "/memory_graph.v1.Store/f00dfeed".to_string(),
            "/evil/\"}\n".to_string(),
            format!("/{}", "a".repeat(4000)),
        ];
        for p in &paths {
            let uri: http::Uri = p.parse().unwrap_or_else(|_| "/bad".parse().unwrap());
            let req = http::Request::builder().uri(uri).body(()).unwrap();
            svc.call(req).await.unwrap();
        }
        let keys: Vec<(String, String)> = obs.rpc.lock().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                ("Store/Search".to_string(), "ok".to_string()),
                (UNKNOWN_RPC.to_string(), "ok".to_string())
            ],
            "only known methods are label values"
        );
        assert_eq!(obs.rpc_total(), paths.len() as u64);
    }

    #[test]
    fn outcome_names() {
        let mut h = http::HeaderMap::new();
        assert_eq!(outcome_of(&h), "ok");
        h.insert("grpc-status", "14".parse().unwrap());
        assert_eq!(outcome_of(&h), "unavailable");
        h.insert("grpc-status", "9".parse().unwrap());
        assert_eq!(outcome_of(&h), "failed_precondition");
    }

    #[test]
    fn histogram_is_cumulative_and_counts_everything() {
        let mut h = Histogram::default();
        for s in [0.0001, 0.003, 0.003, 20.0, f64::NAN] {
            h.observe(s);
        }
        let mut out = String::new();
        h.render(&mut out, "x", "a=\"b\"");
        assert!(out.contains("x_bucket{a=\"b\",le=\"0.0005\"} 2"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"0.005\"} 4"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"10\"} 4"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"+Inf\"} 5"), "{out}");
        assert!(out.contains("x_count{a=\"b\"} 5"), "{out}");
        let mut out = String::new();
        Histogram::default().render(&mut out, "y", "");
        assert!(
            out.contains("y_bucket{le=\"+Inf\"} 0\ny_sum 0\ny_count 0\n"),
            "{out}"
        );
    }

    #[test]
    fn readiness_needs_a_leader_a_recent_word_from_it_and_a_small_lag() {
        let limit = Duration::from_secs(6);
        let r = |leader_known, is_leader, applied, leader_commit, heard: Option<u64>| Readiness {
            leader_known,
            is_leader,
            applied,
            leader_commit,
            since_heard: heard.map(Duration::from_secs),
        };
        assert!(!is_ready(r(false, false, 10, 10, Some(0)), 1000, limit));
        assert!(
            is_ready(r(true, true, 0, 5000, None), 0, limit),
            "the leader is ready"
        );
        assert!(is_ready(r(true, false, 10, 1010, Some(1)), 1000, limit));
        assert!(
            !is_ready(r(true, false, 10, 1011, Some(1)), 1000, limit),
            "a catching-up learner"
        );
        assert!(
            is_ready(r(true, false, 20, 10, Some(0)), 0, limit),
            "applied ahead of what it heard"
        );
        assert!(
            is_ready(r(true, false, 10, 10, Some(6)), 1000, limit),
            "at the limit"
        );
        assert!(
            !is_ready(r(true, false, 10, 10, Some(7)), 1000, limit),
            "a partitioned follower or learner"
        );
        assert!(
            !is_ready(r(true, false, 10, 10, None), 1000, limit),
            "never heard from a leader"
        );
        assert_eq!(leader_silence_limit(2000), Duration::from_secs(6));
    }

    #[test]
    fn heard_from_leader_is_recorded() {
        let o = Observability::default();
        assert_eq!(o.since_heard_from_leader(), None);
        o.note_heard_from_leader();
        assert!(o.since_heard_from_leader().unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn request_lines() {
        assert_eq!(
            parse_request_line("GET /metrics?x=1 HTTP/1.1\r\nHost: a\r\n\r\n"),
            Some(("GET", "/metrics"))
        );
        assert_eq!(parse_request_line("GET /metrics"), None);
        assert_eq!(parse_request_line("GET /metrics HTTP/2 extra"), None);
        assert_eq!(parse_request_line(""), None);
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn leader_commit_is_a_monotonic_max() {
        let o = Observability::default();
        o.note_leader_commit(5);
        o.note_leader_commit(3);
        assert_eq!(o.leader_commit(), 5);
    }
}
