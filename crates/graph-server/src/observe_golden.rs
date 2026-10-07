//! Golden test for the MetricsSnapshot refactor (ADR 0009 O1): the
//! Prometheus renderer as it stood before the refactor, kept verbatim, must
//! produce byte-identical text to [super::render] for the same node state.
//! Delete this module once O3 lands and the contract tests cover OTLP too.
use crate::services::Ctx;
use std::fmt::Write as _;
use std::sync::PoisonError;
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

/// The renderer as it was before `MetricsSnapshot` (verbatim apart from\n/// the pinned read counters): the golden reference.
pub(super) fn render(ctx: &Ctx, read_stats: &graph_store::read_stats::ReadStats) -> String {
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
    render_read_stats(&mut out, read_stats);
    render_repeats(&mut out, &obs.repeats);
    out
}

/// `mg_queries_total{rpc}` and `mg_query_exact_repeats_total{rpc}`: one
/// sample per read RPC (a fixed set), zeros included.
fn render_repeats(out: &mut String, log: &crate::repeats::RepeatLog) {
    let counts = log.counts();
    head(
        out,
        "mg_queries_total",
        "counter",
        "Read RPCs answered by this node, by method.",
    );
    for (rpc, c) in counts {
        let _ = writeln!(
            out,
            "mg_queries_total{{rpc=\"{}\"}} {}",
            rpc.as_str(),
            c.queries
        );
    }
    head(
        out,
        "mg_query_exact_repeats_total",
        "counter",
        "Read RPCs that repeated an identical request (any read view) answered within 60 s at the same Raft applied index, by method; an approximate lower bound (see docs/guide/observability.md).",
    );
    for (rpc, c) in counts {
        let _ = writeln!(
            out,
            "mg_query_exact_repeats_total{{rpc=\"{}\"}} {}",
            rpc.as_str(),
            c.repeats
        );
    }
}

fn counter(out: &mut String, name: &str, help: &str, v: impl std::fmt::Display) {
    head(out, name, "counter", help);
    let _ = writeln!(out, "{name} {v}");
}

/// A counter family with one sample per decode kind.
fn per_kind<V: std::fmt::Display>(
    out: &mut String,
    name: &str,
    help: &str,
    by_kind: [(&str, V); 4],
) {
    head(out, name, "counter", help);
    for (kind, v) in by_kind {
        let _ = writeln!(out, "{name}{{kind=\"{kind}\"}} {v}");
    }
}

/// Nanoseconds as float seconds (the Prometheus base unit).
fn secs(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

/// The read-path counters (ADR 0008 phase 0). They are process-wide: every
/// store in this process adds to them, so several servers sharing one
/// process report the same totals. The seconds families stay at zero unless
/// read timing is on (`--read-timing`).
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
        "Encoded bytes behind query decodes, by kind: dict blocks scanned, symbol sections, the whole encoded size of streams whose header was decoded lazily (symbol bytes lie within it), whole streams decoded. Do not sum across kinds.",
        [
            ("dict", r.dict_bytes),
            ("symbol", r.symbol_bytes),
            ("lazy", r.lazy_bytes),
            ("full", r.full_bytes),
        ],
    );
    per_kind(
        out,
        "mg_read_decode_seconds_total",
        "Seconds inside query decodes, by kind (--read-timing only).",
        [
            ("dict", secs(r.dict_decode_nanos)),
            ("symbol", secs(r.symbol_decode_nanos)),
            ("lazy", secs(r.lazy_decode_nanos)),
            ("full", secs(r.full_decode_nanos)),
        ],
    );
    counter(
        out,
        "mg_read_queries_total",
        "Store read calls (queries) in this process, including rejected or expired ones.",
        r.queries,
    );
    counter(
        out,
        "mg_read_query_seconds_total",
        "Wall seconds inside store read calls, including opening the read transaction (--read-timing only).",
        secs(r.query_nanos),
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

/// Fixed, non-zero read counters: the process-wide ones move under
/// parallel tests, so both renderers get the same pinned values.
fn pinned_read_stats() -> graph_store::read_stats::ReadStats {
    let mut r = graph_store::read_stats::ReadStats::default();
    r.dict_block_decodes = 3;
    r.symbol_section_decodes = 5;
    r.lazy_stream_decodes = 7;
    r.full_stream_decodes = 11;
    r.dict_bytes = 13;
    r.symbol_bytes = 17;
    r.lazy_bytes = 19;
    r.full_bytes = 23;
    r.dict_decode_nanos = 1;
    r.symbol_decode_nanos = 250_000_000;
    r.lazy_decode_nanos = 1_000_000_000;
    r.full_decode_nanos = 1_234_567_891;
    r.queries = 29;
    r.query_nanos = 31;
    r.read_txns = 37;
    r.dict_strings_decoded = 41;
    r
}

/// Record label values that need escaping, a NaN, an overflow-bucket
/// observation and MCP calls on `ctx`'s node.
fn exercise(ctx: &Ctx) {
    let obs = &ctx.raft.obs;
    obs.observe_rpc("search", "ok", 0.003);
    obs.observe_rpc("search", "ok", f64::NAN);
    obs.observe_rpc("we\"ird\\rpc\n", "invalid_argument", 42.0);
    obs.observe_mcp_call("search", "ok");
    obs.observe_mcp_call("to\"ol", "error");
    obs.observe_apply(0.0007);
    obs.observe_apply(3.0);
}

/// The node may still be settling (an election, the first log entries):
/// compare only once two legacy renders around the new one agree, so the
/// node state is the same for all three. Returns the agreed text.
fn assert_golden(ctx: &Ctx) -> String {
    let read_stats = pinned_read_stats();
    for _ in 0..50 {
        let before = render(ctx, &read_stats);
        let after = super::snapshot_with(ctx, &read_stats).to_prometheus();
        let again = render(ctx, &read_stats);
        if before == again {
            assert_eq!(
                after, before,
                "MetricsSnapshot output differs from the legacy renderer"
            );
            return before;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("node state never settled for two consecutive renders");
}

#[test]
fn snapshot_renders_byte_identical_to_the_legacy_renderer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = crate::testing::TestServer::start(&dir.path().join("golden.redb"), Vec::new());
    let ctx = &server.running().expect("server is running").ctx;
    exercise(ctx);
    let text = assert_golden(ctx);
    assert!(
        text.contains("we\\\"ird\\\\rpc\\n"),
        "label escaping exercised"
    );
}

/// On a cluster leader, so the peer-labelled families (replication lag)
/// are compared too.
#[test]
fn snapshot_renders_peer_families_like_the_legacy_renderer() {
    let mut cluster = crate::testing::ClusterTestbed::new(2, Vec::new());
    cluster.form();
    let leader = cluster.wait_leader(crate::testing::CLUSTER_WAIT);
    let ctx = &cluster
        .node(leader)
        .running()
        .expect("leader is running")
        .ctx;
    exercise(ctx);
    let text = assert_golden(ctx);
    assert!(
        text.contains("mg_raft_replication_lag{peer=\""),
        "a peer sample was compared:\n{text}"
    );
}

#[test]
fn snapshot_exports_exactly_the_metric_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = crate::testing::TestServer::start(&dir.path().join("names.redb"), Vec::new());
    let ctx = &server.running().expect("server is running").ctx;
    let snapshot = super::snapshot(ctx);
    let mut exported: Vec<&str> = snapshot.families.iter().map(|f| f.name).collect();
    let mut contract = super::METRIC_NAMES.to_vec();
    exported.sort_unstable();
    contract.sort_unstable();
    assert_eq!(exported, contract);
    assert!(snapshot.family("mg_raft_term").is_some());
    assert!(snapshot.family("mg_nope").is_none());
}
