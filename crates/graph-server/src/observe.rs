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
pub const METRIC_NAMES: [&str; 34] = [
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
    "mg_mcp_tool_calls_total",
    "mg_read_decodes_total",
    "mg_read_decode_bytes_total",
    "mg_read_decode_seconds_total",
    "mg_read_queries_total",
    "mg_read_query_seconds_total",
    "mg_read_txns_total",
    "mg_read_dict_strings_total",
    "mg_read_search_items_total",
    "mg_read_search_seconds_total",
    "mg_queries_total",
    "mg_query_exact_repeats_total",
];

/// The Prometheus text exposition format this module writes.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A fixed-bucket histogram ([`DURATION_BUCKETS`]); counts per bucket are
/// not cumulative here, [`Histogram::render`] accumulates.
#[derive(Debug, Clone, Default, PartialEq)]
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

    /// Observations per bucket of [DURATION_BUCKETS] (not cumulative;
    /// observations above the last bound are only in [count](Self::count)).
    pub fn bucket_counts(&self) -> &[u64; DURATION_BUCKETS.len()] {
        &self.counts
    }

    /// The sum of every observation, seconds.
    pub fn sum(&self) -> f64 {
        self.sum
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
    /// Read RPCs answered and their exact repeats (ADR 0008 phase 3 gate).
    pub repeats: crate::repeats::RepeatLog,
    /// Traced proposals in flight, for the leader's `apply` span links
    /// (ADR 0009 D5).
    pub apply_links: crate::telemetry::ApplyLinks,
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

// ---------------------------------------------------------------------------
// The metrics snapshot: every family `/metrics` exports, gathered once.

/// The kind of a metric family (its Prometheus `# TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Gauge,
    Counter,
    Histogram,
}

impl MetricKind {
    /// The `# TYPE` word.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Gauge => "gauge",
            MetricKind::Counter => "counter",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// One sample's value. Integer and float samples are kept apart so each
/// renders exactly as it always has (`7`, not `7.0`).
#[derive(Debug, Clone, PartialEq)]
pub enum SampleValue {
    Int(u64),
    Float(f64),
    Histogram(Histogram),
}

/// One sample of a family: its labels (in exposition order, values
/// unescaped) and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Vec<(&'static str, String)>,
    pub value: SampleValue,
}

/// One metric family: a [`METRIC_NAMES`] entry, its help text, kind and
/// samples (possibly none: a labelled family with nothing observed yet).
#[derive(Debug, Clone, PartialEq)]
pub struct MetricFamily {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: MetricKind,
    pub samples: Vec<Sample>,
}

impl MetricFamily {
    fn new(name: &'static str, kind: MetricKind, help: &'static str) -> Self {
        Self {
            name,
            help,
            kind,
            samples: Vec::new(),
        }
    }

    fn with_sample(mut self, labels: Vec<(&'static str, String)>, value: SampleValue) -> Self {
        self.samples.push(Sample { labels, value });
        self
    }

    fn push(&mut self, labels: Vec<(&'static str, String)>, value: SampleValue) {
        self.samples.push(Sample { labels, value });
    }
}

/// Every family a node exports at one moment, in [`METRIC_NAMES`] exposition
/// order: the one source both the Prometheus text ([`render`]) and, later,
/// OTLP instruments (ADR 0009) read, so the two can never disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsSnapshot {
    pub families: Vec<MetricFamily>,
}

impl MetricsSnapshot {
    /// The family called `name`, if exported.
    pub fn family(&self, name: &str) -> Option<&MetricFamily> {
        self.families.iter().find(|f| f.name == name)
    }

    /// The Prometheus text exposition (format 0.0.4) of this snapshot.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        for family in &self.families {
            render_family(&mut out, family);
        }
        out
    }
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

fn render_family(out: &mut String, family: &MetricFamily) {
    head(out, family.name, family.kind.as_str(), family.help);
    for sample in &family.samples {
        let labels = sample
            .labels
            .iter()
            .map(|(key, value)| format!("{key}=\"{}\"", label(value)))
            .collect::<Vec<_>>()
            .join(",");
        match &sample.value {
            SampleValue::Int(v) => sample_line(out, family.name, &labels, v),
            SampleValue::Float(v) => sample_line(out, family.name, &labels, v),
            SampleValue::Histogram(h) => h.render(out, family.name, &labels),
        }
    }
}

fn sample_line(out: &mut String, name: &str, labels: &str, v: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name} {v}");
    } else {
        let _ = writeln!(out, "{name}{{{labels}}} {v}");
    }
}

fn gauge(name: &'static str, help: &'static str, v: u64) -> MetricFamily {
    MetricFamily::new(name, MetricKind::Gauge, help).with_sample(Vec::new(), SampleValue::Int(v))
}

fn counter(name: &'static str, help: &'static str, v: SampleValue) -> MetricFamily {
    MetricFamily::new(name, MetricKind::Counter, help).with_sample(Vec::new(), v)
}

/// The whole `/metrics` document of this node.
pub fn render(ctx: &Ctx) -> String {
    snapshot(ctx).to_prometheus()
}

/// Every family this node exports, read now.
pub fn snapshot(ctx: &Ctx) -> MetricsSnapshot {
    snapshot_with(ctx, &graph_store::read_stats::snapshot())
}

/// [`snapshot`] with the process-wide read counters given (tests pin them).
fn snapshot_with(ctx: &Ctx, read_stats: &graph_store::read_stats::ReadStats) -> MetricsSnapshot {
    let mut families = raft_families(ctx);
    families.extend(storage_families(ctx));
    families.extend(rpc_families(&ctx.raft.obs));
    families.extend(cluster_families(ctx));
    families.extend(apply_and_build_families(&ctx.raft.obs));
    families.extend(backup_families(ctx));
    families.extend(read_stats_families(read_stats));
    families.extend(repeats_families(&ctx.raft.obs.repeats));
    MetricsSnapshot { families }
}

/// `mg_raft_*`: term, leader, role, the log indices and replication lag.
fn raft_families(ctx: &Ctx) -> Vec<MetricFamily> {
    let m = ctx.raft.metrics();
    let applied = m.last_applied.as_ref().map_or(0, |l| l.index);
    let last_log = m.last_log_index.unwrap_or(0);
    let committed = ctx
        .raft
        .log_store
        .committed_index()
        .unwrap_or(0)
        .max(applied);
    let current = crate::services::admin::role(m.state);
    let mut role = MetricFamily::new(
        "mg_raft_role",
        MetricKind::Gauge,
        "1 for this node's current Raft role.",
    );
    for r in ["leader", "follower", "candidate", "learner", "shutdown"] {
        role.push(
            vec![("role", r.to_string())],
            SampleValue::Int(u64::from(r == current)),
        );
    }
    let mut lag = MetricFamily::new(
        "mg_raft_replication_lag",
        MetricKind::Gauge,
        "Leader only: entries each peer's matched index is behind the leader's last log index.",
    );
    if let Some(rep) = &m.replication {
        for (id, matched) in rep.iter().filter(|(id, _)| **id != ctx.info.node_id) {
            let behind = last_log.saturating_sub(matched.as_ref().map_or(0, |l| l.index));
            lag.push(vec![("peer", id.to_string())], SampleValue::Int(behind));
        }
    }
    vec![
        gauge("mg_raft_term", "Current Raft term.", m.current_term),
        gauge(
            "mg_raft_leader_id",
            "Node id of the known leader (0: none).",
            ctx.raft.leader().id.unwrap_or(0),
        ),
        role,
        gauge(
            "mg_raft_last_log_index",
            "Index of the last entry in this node's Raft log.",
            last_log,
        ),
        gauge(
            "mg_raft_committed_index",
            "Last log index this node knows to be committed.",
            committed,
        ),
        gauge(
            "mg_raft_applied_index",
            "Last log index applied to the store.",
            applied,
        ),
        gauge(
            "mg_raft_snapshot_index",
            "Log index of the current snapshot (0: none).",
            m.snapshot.map_or(0, |s| s.index),
        ),
        gauge(
            "mg_raft_purged_index",
            "Last purged log index (0: none).",
            m.purged.map_or(0, |p| p.index),
        ),
        lag,
    ]
}

/// File sizes and open snapshot handles.
fn storage_families(ctx: &Ctx) -> Vec<MetricFamily> {
    vec![
        gauge(
            "mg_store_bytes",
            "Size of the store file on disk.",
            std::fs::metadata(ctx.slot.path()).map_or(0, |m| m.len()),
        ),
        gauge(
            "mg_log_bytes",
            "Size of the Raft log file on disk.",
            ctx.raft.log_bytes(),
        ),
        gauge(
            "mg_snapshot_handles_open",
            "Open snapshot handles held for paging clients.",
            ctx.slot.snapshots().len() as u64,
        ),
    ]
}

/// `mg_rpc_duration_seconds` and `mg_rpc_total`, read under one lock so the
/// two always agree.
fn rpc_families(obs: &Observability) -> Vec<MetricFamily> {
    let mut duration = MetricFamily::new(
        "mg_rpc_duration_seconds",
        MetricKind::Histogram,
        "gRPC call duration until the response headers, by method and outcome.",
    );
    let mut total = MetricFamily::new(
        "mg_rpc_total",
        MetricKind::Counter,
        "gRPC calls served, by method and outcome.",
    );
    let rpc = obs.rpc.lock().unwrap_or_else(PoisonError::into_inner);
    for ((name, outcome), h) in rpc.iter() {
        let labels = vec![("rpc", name.clone()), ("outcome", outcome.clone())];
        total.push(labels.clone(), SampleValue::Int(h.count()));
        duration.push(labels, SampleValue::Histogram(h.clone()));
    }
    vec![duration, total]
}

/// Forwarded writes, quorum probes and MCP tool calls.
fn cluster_families(ctx: &Ctx) -> Vec<MetricFamily> {
    let (alive, dead) = ctx.raft.net_stats.probes();
    let mut probes = MetricFamily::new(
        "mg_quorum_probes_total",
        MetricKind::Counter,
        "Leader liveness probes of silent voters while a write waited (quorum-loss check), by outcome.",
    );
    probes.push(vec![("outcome", "alive".into())], SampleValue::Int(alive));
    probes.push(vec![("outcome", "dead".into())], SampleValue::Int(dead));
    let mut mcp = MetricFamily::new(
        "mg_mcp_tool_calls_total",
        MetricKind::Counter,
        "MCP tools/call requests on --mcp-listen, by tool and outcome (ok, error: an isError result, rejected: a JSON-RPC error, timeout: past the per-call deadline, refused: past --mcp-max-inflight (429), internal: the call itself failed).",
    );
    for ((tool, outcome), n) in ctx.raft.obs.mcp_calls() {
        mcp.push(
            vec![("tool", tool), ("outcome", outcome)],
            SampleValue::Int(n),
        );
    }
    vec![
        counter(
            "mg_writes_forwarded_total",
            "Writes and membership changes this node forwarded to the leader.",
            SampleValue::Int(ctx.fwd.forwarded_total()),
        ),
        probes,
        mcp,
    ]
}

/// `mg_apply_duration_seconds` and `mg_build_info`.
fn apply_and_build_families(obs: &Observability) -> Vec<MetricFamily> {
    let apply = obs
        .apply
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    vec![
        MetricFamily::new(
            "mg_apply_duration_seconds",
            MetricKind::Histogram,
            "Time to apply one committed log entry to the store.",
        )
        .with_sample(Vec::new(), SampleValue::Histogram(apply)),
        MetricFamily::new(
            "mg_build_info",
            MetricKind::Gauge,
            "Always 1; the labels name the server version, protocol and store format.",
        )
        .with_sample(
            vec![
                ("version", crate::SERVER_VERSION.to_string()),
                ("protocol", graph_proto::PROTOCOL_VERSION.to_string()),
                ("store_format", graph_store::SCHEMA_VERSION.to_string()),
            ],
            SampleValue::Int(1),
        ),
    ]
}

/// The snapshot backup uploader's state (zeros without `--backup-url`).
fn backup_families(ctx: &Ctx) -> Vec<MetricFamily> {
    let b = ctx.backup.as_ref().map(|b| b.stats()).unwrap_or_default();
    vec![
        gauge(
            "mg_backup_last_success_timestamp",
            "Unix seconds of the last snapshot backup committed (0: none).",
            b.last_success_unix,
        ),
        gauge(
            "mg_backup_last_index",
            "Log index of the last snapshot backup committed (0: none).",
            b.last_index,
        ),
        counter(
            "mg_backup_failures_total",
            "Snapshot backups that failed after every retry.",
            SampleValue::Int(b.failures_total),
        ),
        counter(
            "mg_backup_bytes_total",
            "Bytes written by successful snapshot backups.",
            SampleValue::Int(b.bytes_total),
        ),
    ]
}

/// `mg_queries_total{rpc}` and `mg_query_exact_repeats_total{rpc}`: one
/// sample per read RPC (a fixed set), zeros included.
fn repeats_families(log: &crate::repeats::RepeatLog) -> Vec<MetricFamily> {
    let counts = log.counts();
    let mut queries = MetricFamily::new(
        "mg_queries_total",
        MetricKind::Counter,
        "Read RPCs answered by this node, by method.",
    );
    let mut repeats = MetricFamily::new(
        "mg_query_exact_repeats_total",
        MetricKind::Counter,
        "Read RPCs that repeated an identical request (any read view) answered within 60 s at the same Raft applied index, by method; an approximate lower bound (see docs/guide/observability.md).",
    );
    for (rpc, c) in counts {
        let labels = vec![("rpc", rpc.as_str().to_string())];
        queries.push(labels.clone(), SampleValue::Int(c.queries));
        repeats.push(labels, SampleValue::Int(c.repeats));
    }
    vec![queries, repeats]
}

/// A counter family with one sample per `kind` label value.
fn per_kind<const N: usize>(
    name: &'static str,
    help: &'static str,
    by_kind: [(&str, SampleValue); N],
) -> MetricFamily {
    let mut family = MetricFamily::new(name, MetricKind::Counter, help);
    for (kind, v) in by_kind {
        family.push(vec![("kind", kind.to_string())], v);
    }
    family
}

/// Nanoseconds as float seconds (the Prometheus base unit).
fn secs(nanos: u64) -> SampleValue {
    SampleValue::Float(nanos as f64 / 1e9)
}

/// The read-path counters (ADR 0008 phase 0). They are process-wide: every
/// store in this process adds to them, so several servers sharing one
/// process report the same totals. The seconds families stay at zero unless
/// read timing is on (`--read-timing`).
fn read_stats_families(r: &graph_store::read_stats::ReadStats) -> Vec<MetricFamily> {
    use SampleValue::Int;
    vec![
        per_kind(
            "mg_read_decodes_total",
            "Decodes done by queries, by kind (dict: reverse-dictionary block scans; symbol: symbol sections; lazy: stream headers; full: whole streams).",
            [
                ("dict", Int(r.dict_block_decodes)),
                ("symbol", Int(r.symbol_section_decodes)),
                ("lazy", Int(r.lazy_stream_decodes)),
                ("full", Int(r.full_stream_decodes)),
            ],
        ),
        per_kind(
            "mg_read_decode_bytes_total",
            "Encoded bytes behind query decodes, by kind: dict blocks scanned, symbol sections, the whole encoded size of streams whose header was decoded lazily (symbol bytes lie within it), whole streams decoded. Do not sum across kinds.",
            [
                ("dict", Int(r.dict_bytes)),
                ("symbol", Int(r.symbol_bytes)),
                ("lazy", Int(r.lazy_bytes)),
                ("full", Int(r.full_bytes)),
            ],
        ),
        per_kind(
            "mg_read_decode_seconds_total",
            "Seconds inside query decodes, by kind (--read-timing only).",
            [
                ("dict", secs(r.dict_decode_nanos)),
                ("symbol", secs(r.symbol_decode_nanos)),
                ("lazy", secs(r.lazy_decode_nanos)),
                ("full", secs(r.full_decode_nanos)),
            ],
        ),
        counter(
            "mg_read_queries_total",
            "Store read calls (queries) in this process, including rejected or expired ones.",
            Int(r.queries),
        ),
        counter(
            "mg_read_query_seconds_total",
            "Wall seconds inside store read calls, including opening the read transaction (--read-timing only).",
            secs(r.query_nanos),
        ),
        counter(
            "mg_read_txns_total",
            "Read transactions opened by store read calls (snapshot reads reuse one).",
            Int(r.read_txns),
        ),
        counter(
            "mg_read_dict_strings_total",
            "Dictionary strings allocated by query-side term lookups.",
            Int(r.dict_strings_decoded),
        ),
        per_kind(
            "mg_read_search_items_total",
            "Search work, by kind (postings: candidate files' postings scanned; walk_files: files walked; walk_tokens: token records read through posting ordinals).",
            [
                ("postings", Int(r.search_postings)),
                ("walk_files", Int(r.search_walk_files)),
                ("walk_tokens", Int(r.search_walk_tokens)),
            ],
        ),
        per_kind(
            "mg_read_search_seconds_total",
            "Seconds in search's phases, by kind (posting: the posting scan; ctx: resolving, filtering and sorting candidate files; walk: the per-file walk) (--read-timing only).",
            [
                ("posting", secs(r.search_posting_nanos)),
                ("ctx", secs(r.search_ctx_nanos)),
                ("walk", secs(r.search_walk_nanos)),
            ],
        ),
    ]
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

/// The gRPC status code of a trailers-only response (every error tonic
/// answers before a body), else 0 (`OK`; an error a stream reports in its
/// trailers after the headers is not seen here).
fn status_code_of(headers: &http::HeaderMap) -> i64 {
    headers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// `rpc.service` and `rpc.method` (OpenTelemetry semantic conventions) of
/// a known path, `/memory_graph.v1.Store/Search` -> (`memory_graph.v1.Store`,
/// `Search`); `unknown` for both otherwise (bounded, like [`rpc_name`]).
pub fn rpc_parts(path: &str) -> (String, String) {
    if rpc_name(path) == UNKNOWN_RPC {
        return (UNKNOWN_RPC.into(), UNKNOWN_RPC.into());
    }
    match path.trim_start_matches('/').split_once('/') {
        Some((svc, method)) => (svc.to_string(), method.to_string()),
        None => (UNKNOWN_RPC.into(), UNKNOWN_RPC.into()),
    }
}

/// The remote trace context an incoming request carried (`traceparent`),
/// in the request's extensions for handlers that open a span of their own
/// whose parent must be the caller's even though their `rpc` span is not
/// exported (the Raft service's `install_snapshot`, ADR 0009 D5).
#[derive(Clone, Debug)]
pub struct RemoteParent(pub opentelemetry::Context);

/// The host of an advertised `host:port` (`server.address`).
fn host_of(addr: &str) -> String {
    match addr.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        _ => addr.to_string(),
    }
}

/// A tower layer for the tonic server: every call gets an `rpc` span
/// (method, peer, outcome, duration_ms, and the OpenTelemetry RPC
/// attributes), a `debug` event when it ends, and an observation in
/// `mg_rpc_duration_seconds` / `mg_rpc_total`. A W3C `traceparent` on the
/// request makes the span its child (ADR 0009 D5).
#[derive(Clone)]
pub struct RpcLayer {
    obs: Arc<Observability>,
    server_address: Arc<str>,
}

impl RpcLayer {
    pub fn new(obs: Arc<Observability>) -> Self {
        Self {
            obs,
            server_address: Arc::from(""),
        }
    }

    /// `server.address` on every span: this node's advertised host.
    pub fn with_server_address(mut self, advertise: &str) -> Self {
        self.server_address = Arc::from(host_of(advertise));
        self
    }
}

impl<S> tower_layer::Layer<S> for RpcLayer {
    type Service = RpcService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RpcService {
            inner,
            obs: Arc::clone(&self.obs),
            server_address: Arc::clone(&self.server_address),
        }
    }
}

#[derive(Clone)]
pub struct RpcService<S> {
    inner: S,
    obs: Arc<Observability>,
    server_address: Arc<str>,
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

/// The `rpc` span; `$target` must be a constant (a span's target is fixed
/// where the macro is called), hence one call per target. The
/// OpenTelemetry fields start empty and are recorded only with traces on,
/// so local logs are unchanged when OpenTelemetry is off (ADR 0009 D1).
macro_rules! rpc_span {
    ($target:expr, $rpc:expr, $peer:expr) => {
        tracing::info_span!(
            target: $target,
            "rpc",
            method = %$rpc,
            peer = %$peer,
            outcome = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            otel.kind = tracing::field::Empty,
            rpc.system = tracing::field::Empty,
            rpc.service = tracing::field::Empty,
            rpc.method = tracing::field::Empty,
            rpc.grpc.status_code = tracing::field::Empty,
            server.address = tracing::field::Empty,
        )
    };
}

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

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        // The clone that was polled ready serves this call (tower's rule).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let rpc = rpc_name(req.uri().path());
        let (svc, method) = rpc_parts(req.uri().path());
        let peer = req
            .extensions()
            .get::<ConnInfo>()
            .and_then(|c| c.peer)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let obs = Arc::clone(&self.obs);
        // The Raft service's spans get a target of their own, which the
        // OpenTelemetry layer filters out (heartbeats and AppendEntries
        // would flood the collector); local logs see them as before.
        let span = if svc == "memory_graph.v1.Raft" {
            rpc_span!(crate::telemetry::RAFT_RPC_TARGET, rpc, peer)
        } else {
            rpc_span!(module_path!(), rpc, peer)
        };
        let traced = crate::telemetry::traces_on();
        if traced {
            span.record("otel.kind", "server");
            span.record("rpc.system", "grpc");
            span.record("rpc.service", svc.as_str());
            span.record("rpc.method", method.as_str());
            span.record("server.address", &*self.server_address);
            let parent = graph_proto::trace_context::extract(req.headers());
            {
                use opentelemetry::trace::TraceContextExt;
                use tracing_opentelemetry::OpenTelemetrySpanExt;
                if parent.span().span_context().is_valid() {
                    // Fails only for a span the layer does not see (Raft).
                    let _ = span.set_parent(parent.clone());
                }
            }
            req.extensions_mut().insert(RemoteParent(parent));
        }
        let fut = {
            let span = span.clone();
            async move {
                let t = Instant::now();
                let r = inner.call(req).await;
                let secs = t.elapsed().as_secs_f64();
                let (outcome, code) = match &r {
                    Ok(resp) => (outcome_of(resp.headers()), status_code_of(resp.headers())),
                    // UNKNOWN: the transport failed before any status.
                    Err(_) => ("transport_error".into(), 2),
                };
                span.record("outcome", outcome.as_str());
                if traced {
                    span.record("rpc.grpc.status_code", code);
                }
                span.record("duration_ms", secs * 1000.0);
                obs.observe_rpc(&rpc, &outcome, secs);
                tracing::debug!("rpc finished");
                r
            }
        };
        let scoped = crate::telemetry::RPC_SPAN.scope(span.clone(), fut);
        Box::pin(scoped.instrument(span))
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
#[path = "observe_golden.rs"]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;

    /// A frozen snapshot renders to exactly this text (peers, labels,
    /// escaping, int vs float, an empty family, a histogram).
    #[test]
    fn a_frozen_snapshot_renders_exactly() {
        let mut h = Histogram::default();
        h.observe(0.002);
        h.observe(20.0);
        let mut lag = MetricFamily::new("mg_raft_replication_lag", MetricKind::Gauge, "Lag.");
        lag.push(vec![("peer", "2".into())], SampleValue::Int(0));
        lag.push(vec![("peer", "3".into())], SampleValue::Int(17));
        let snapshot = MetricsSnapshot {
            families: vec![
                gauge("mg_raft_term", "Term.", 4),
                lag,
                MetricFamily::new("mg_rpc_total", MetricKind::Counter, "Calls."),
                counter(
                    "mg_read_query_seconds_total",
                    "Secs.",
                    SampleValue::Float(1.5),
                ),
                MetricFamily::new("mg_apply_duration_seconds", MetricKind::Histogram, "Apply.")
                    .with_sample(vec![("q", "a\"b".into())], SampleValue::Histogram(h)),
            ],
        };
        let buckets: String = DURATION_BUCKETS
            .iter()
            .map(|b| {
                let n = u8::from(*b >= 0.0025);
                format!("mg_apply_duration_seconds_bucket{{q=\"a\\\"b\",le=\"{b}\"}} {n}\n")
            })
            .collect();
        let expected = format!(
            "# HELP mg_raft_term Term.\n# TYPE mg_raft_term gauge\nmg_raft_term 4\n\
             # HELP mg_raft_replication_lag Lag.\n# TYPE mg_raft_replication_lag gauge\n\
             mg_raft_replication_lag{{peer=\"2\"}} 0\nmg_raft_replication_lag{{peer=\"3\"}} 17\n\
             # HELP mg_rpc_total Calls.\n# TYPE mg_rpc_total counter\n\
             # HELP mg_read_query_seconds_total Secs.\n# TYPE mg_read_query_seconds_total counter\n\
             mg_read_query_seconds_total 1.5\n\
             # HELP mg_apply_duration_seconds Apply.\n# TYPE mg_apply_duration_seconds histogram\n\
             {buckets}mg_apply_duration_seconds_bucket{{q=\"a\\\"b\",le=\"+Inf\"}} 2\n\
             mg_apply_duration_seconds_sum{{q=\"a\\\"b\"}} 20.002\n\
             mg_apply_duration_seconds_count{{q=\"a\\\"b\"}} 2\n"
        );
        assert_eq!(snapshot.to_prometheus(), expected);
    }

    #[test]
    fn read_stats_render_as_counters_in_seconds() {
        // `ReadStats` is non-exhaustive: no struct literal outside its crate.
        let mut r = graph_store::read_stats::ReadStats::default();
        r.symbol_decode_nanos = 1_500_000_000;
        r.query_nanos = 2_000_000_000;
        r.queries = 7;
        let out = MetricsSnapshot {
            families: read_stats_families(&r),
        }
        .to_prometheus();
        let families: Vec<&str> = out
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .collect();
        assert_eq!(families.len(), 9, "{out}");
        for f in families {
            let (name, kind) = f.split_once(' ').expect("TYPE name kind");
            assert_eq!(kind, "counter", "{name}");
            assert!(METRIC_NAMES.contains(&name), "{name} not in METRIC_NAMES");
        }
        assert!(
            out.contains(
                "mg_read_decode_seconds_total{kind=\"symbol\"} 1.5
"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "mg_read_query_seconds_total 2
"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "mg_read_queries_total 7
"
            ),
            "{out}"
        );
    }

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
