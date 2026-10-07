# ADR 0009: OpenTelemetry (OTLP traces, metrics and logs)

**Status:** Proposed on 2026-10-06, and revised the same day after dev and QA review. The owner decides. The owner made the scope decisions in D1 on 2026-10-06 (signals, transport, default). Builds on [ADR 0004](0004-client-server-and-replication.md) D10 (observability), which gets a dated note pointing here. Epic amendment, proposed: stories [50](../epic-code-memory-graph.md#story-50), [51](../epic-code-memory-graph.md#story-51), [52](../epic-code-memory-graph.md#story-52), [53](../epic-code-memory-graph.md#story-53) and [54](../epic-code-memory-graph.md#story-54). They are not counted in the epic totals until this ADR is accepted.

## In plain words

1. Today `serve` has `tracing` logs (text or JSON), a hand-written Prometheus `/metrics` endpoint and health checks. There is no distributed tracing and no OpenTelemetry (OTLP) export.
2. This ADR adds all three OpenTelemetry signals (traces, metrics and logs). They are exported over OTLP/gRPC without TLS to an OpenTelemetry Collector running next to the node. The collector handles TLS and auth onward.
3. It is **off by default**. Nothing related to OpenTelemetry runs until an endpoint is set, by a flag, a config key or the standard `OTEL_*` environment variables. When it is off, the logs are exactly as they are today.
4. Most metrics come from one shared snapshot, so Prometheus and OTLP report the same numbers. The two duration histograms are recorded at the same call sites as their Prometheus versions. The Prometheus output stays byte-identical until story 52 adds two families.
5. A collector that is down or slow never blocks or fails an RPC, a Raft step or an index batch. Telemetry is dropped and counted instead.
6. Exported spans and logs carry operational fields only (names, outcomes, ids, counts, sizes and durations), never query text or source.

## Context

References are to `origin/main` on 2026-10-06.

- **Logs.** `crates/graph-cli/src/logging.rs::init` (~35, called from `main.rs` ~1176) installs a plain `tracing_subscriber::fmt().try_init()`. It is not a layered `Registry`, so another layer cannot be added.
- **Metrics:**
  - `render()` (`crates/graph-server/src/observe.rs` ~310) writes Prometheus text straight from about ten sources: openraft metrics, the log store, forward counts, quorum probes, MCP, backup, repeats and read stats. There is no snapshot type in between.
  - `METRIC_NAMES` lists the 32 families (the contract), and `DURATION_BUCKETS` holds the histogram bounds (0.5 ms to 10 s).
  - `observe_rpc` (~159) records each RPC's duration from `RpcService::call` (~765-796).
- **Spans that exist.** `RpcLayer` opens an `rpc` span per RPC (`observe.rs` ~777-802, installed in `server.rs` ~908). The state machine opens an `apply` span per Raft apply (`raft/state_machine.rs` ~166).
- **Spans that are missing.** MCP calls, index batches and `graph-client` have none. `#[instrument]` is used nowhere.
- **Interceptors where a trace context can travel:**
  - `RpcService::call` (`observe.rs` ~765), on the server side;
  - `ForwardHeaders` (`forward.rs` ~102-118), from a follower to the leader;
  - `RaftHeaders` (`raft/network.rs` ~373-381), from the leader to its peers;
  - the client's `SendVersion` interceptor. It is defined in `graph-proto/src/version.rs` (~20) and attached to the Store, Write and Admin clients in `graph-client/src/conn.rs` (~245-260).
- **Client facade.** `RemoteStore` is synchronous: it runs each call on its own small tokio runtime, so the caller's current span is not automatically the span the future runs in.
- **Config.** Every serve flag gets a matching TOML key automatically (`serve_config.rs`). Keys that look like secrets are refused.
- **Exit codes** (`graph-cli/src/target.rs`): 3 no leader, 4 write deadline, 5 protocol or format, 6 `WrongCluster`.
- **Dependencies.** The workspace pins tonic `=0.14.6` and prost `=0.14.4`. The pure-Rust gate (`scripts/check-no-c-deps.py`) and `test_gate.py` forbid `ring` and `aws-lc-sys`.

## Decision

### D1. Owner decisions (2026-10-06)

- **Signals:** traces, metrics and logs, all three.
- **Transport:** OTLP over gRPC, with no TLS, to a local or sidecar OpenTelemetry Collector. The collector handles TLS and auth onward. TLS in the process waits for #104.
- **Default:** off, and opt-in. With it off, no OpenTelemetry layer, provider or exporter task exists, and log output is unchanged.

### D2. Crates, versions and features

Exact pins in `[workspace.dependencies]`, as the workspace does for tonic and prost.

| Crate | Pin | Features |
|---|---|---|
| `opentelemetry` | `=0.33.0` | default |
| `opentelemetry_sdk` | `=0.33.0` | `rt-tokio`, plus `trace`, `metrics`, `logs` as needed; review the default features before pinning and turn off any that are not needed |
| `opentelemetry-otlp` | `=0.33.0` | `default-features = false`; `grpc-tonic`, `trace`, `metrics`, `logs` |
| `tracing-opentelemetry` | `=0.34.0` | default |
| `opentelemetry-appender-tracing` | `=0.33.0` | default |
| `opentelemetry-proto` | `=0.33.0`, dev-dependency only (the fake collector) | `gen-tonic`, with the trace, metrics and logs services |

- **Avoid:** the `tls`, `tls-*`, `reqwest-rustls`, `reqwest-*` and `zstd-*` features of `opentelemetry-otlp`, and `with-schemars` on `opentelemetry-proto`. They pull in `ring`, `aws-lc`, `reqwest`, a C zstd, or code nobody uses.
- **Checks in story 50, before pinning:**
  - `tracing-opentelemetry 0.34.0` really targets `opentelemetry 0.33`. If not, take the release that does and record it here with a dated note.
  - `opentelemetry-otlp` resolves onto the workspace's tonic `=0.14.6` and prost `=0.14.4`. `cargo tree -d` must show no second tonic or prost.
  - `cargo tree -i ring` and `cargo tree -i aws-lc-sys` print nothing, and `check-no-c-deps.py` (all six targets), `test_gate.py` and the docker build pass.
- The OTLP messages come as generated code inside `opentelemetry-proto`, so no protoc and no xtask change is needed.
- Adding these dependencies needs the owner's approval, given in story 50's PR.

### D3. Configuration

| Flag (serve) | Config key | Environment | Default |
|---|---|---|---|
| `--otlp-endpoint <url>` | `otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset, which means off |
| `--otlp-signals traces,metrics,logs` | `otlp-signals` | none | all three, once an endpoint is set |
| `--otel-service-name <name>` | `otel-service-name` | `OTEL_SERVICE_NAME` | `memory-graph` |
| `--otlp-metrics-interval <duration>` | `otlp-metrics-interval` | none | `60s` |
| none | none | `OTEL_EXPORTER_OTLP_TIMEOUT` | the SDK default (10 s) |
| none | none | `OTEL_EXPORTER_OTLP_HEADERS` | none |
| none | none | `OTEL_RESOURCE_ATTRIBUTES` | none |
| none | none | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | parent-based, always on |
| none | none | `OTEL_SDK_DISABLED=true` | false |

- **Enabling.** An endpoint from any source turns OpenTelemetry on. `--otlp-signals` only narrows which signals are sent.
- **Precedence:** the flag, then the config key, then the environment.
- **`OTEL_SDK_DISABLED=true`** turns everything off, whatever else is set. This follows the OpenTelemetry spec.
- **The endpoint is always passed explicitly** to each exporter builder, so the SDK never reads endpoint variables on its own.
- **Headers come from the environment only** (`OTEL_EXPORTER_OTLP_HEADERS`). There is no flag or key, because auth headers are secrets and the serve config refuses keys that look like secrets. Header values are never logged or exported.
- **Settings this ADR does not support:**
  - `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT` (per-signal endpoints);
  - `OTEL_EXPORTER_OTLP_PROTOCOL` set to anything other than `grpc`;
  - an `https://` endpoint.

  How they are handled depends on where they come from:
  - **From a flag or config key** (only `https://` can come from there): `serve` refuses to start. It exits with the new code **7, `TELEMETRY_CONFIG`**, in `target.rs`, and a message naming the setting and pointing to #104.
  - **From the environment:** `serve` logs one error naming the variable and starts with OTLP disabled. Platforms often inject `OTEL_*` variables, and an injected variable must not crash-loop `serve`.
- **An endpoint that is valid but unreachable** at startup is not an error. `serve` starts and serves, and the exports fail and are counted (D8).
- The scope is `serve`. Other CLI commands do not export.

### D4. Resource attributes

| Attribute | Value |
|---|---|
| `service.name` | `--otel-service-name`, then the config key, then `OTEL_SERVICE_NAME`, then `service.name` in `OTEL_RESOURCE_ATTRIBUTES`, then `memory-graph` |
| `service.version` | the crate version, as in `mg_build_info{version}` |
| `service.instance.id` | the Raft node id |
| `memory_graph.cluster` | the cluster id |
| `host.name` | the host name, reusing the existing hostname logic (the StatefulSet pod name on Kubernetes) |

`OTEL_RESOURCE_ATTRIBUTES` can add attributes. It does not override `service.instance.id`, `memory_graph.cluster` or `service.version`. These come from the node's own state, which is authoritative, and a stale or templated environment value would attach one node's telemetry to another node. `service.name` is a deployment label, so it follows the precedence in the table.

### D5. Traces

- **Semantic conventions** on the rpc span: `rpc.system=grpc`, `rpc.service`, `rpc.method`, `rpc.grpc.status_code` and `server.address`.
- **Custom attributes** use the `memory_graph.*` namespace (for example `memory_graph.forwarded_by`, `memory_graph.peer_id`). This is deliberate: it keeps them apart from semantic-convention keys, including future ones.
- **Trace scope.** This adds no wire or Raft-log payload change, which fixes what one trace can cover.
  - **One trace** covers: client call, then follower `rpc`, then `forward` hop, then leader `rpc`, then the leader's `apply` of that write. The leader proposes the entry and waits for it to apply inside the request's task. The apply runs in the state machine task, so the `apply` span is joined to the request through a **span link**, carrying the log index, rather than as a child, unless story 51 finds the apply on the request's own task, where it is a child. Story 51 records which it is.
  - **Raft replication** (append to followers) and **follower applies** are not traced per request. A log entry carries no trace context, and adding one would change the Raft log payload, which is out of scope here.
- **Span tree and names** (the test checks these by span id):

  | Span | Parent |
  |---|---|
  | `client` (`RemoteStore` call, `graph-client`) | the caller's span, or root |
  | `rpc` (follower) | `client`, from the extracted `traceparent` |
  | `forward` (follower, attribute `memory_graph.forwarded_by`) | `rpc` (follower) |
  | `rpc` (leader) | `forward`, from the injected `traceparent` |
  | `apply` (leader) | linked to `rpc` (leader), or its child (see above) |
  | `mcp.tools_call` | the MCP request's span, or root |
  | `index_batch` | the `Index` rpc span (`serve`), or root (embedded) |
  | `install_snapshot` (one span on each side, wrapping the whole stream) | root, or the sending node's span through `RaftHeaders` |

- **Raft traffic is not exported.** A `tracing` span's target is fixed where the macro is called, and today `RpcLayer` opens one `rpc` span for every service (`observe.rs` ~777).
  - **Separate target.** `RpcService::call` chooses the macro call by request path. For `/memory_graph.v1.Raft/*` it opens the span with a separate `info_span!(target: "memory_graph::raft_rpc", "rpc", ...)` call, and for everything else it uses the existing call.
  - **Per-layer filter.** The OpenTelemetry layer has its own filter (`Layer::with_filter`) that rejects the `memory_graph::raft_rpc` target. The fmt layer does not have that filter, so these spans still appear in local logs at the usual levels. An idle cluster does not flood the collector.
  - **Context is extracted anyway.** The receiving side of `InstallSnapshot` still extracts the propagated `traceparent` in `RpcService::call`, even though its `rpc` span is filtered out of export. Its `install_snapshot` span (default target, so exported) takes the extracted context as its parent.
  - **Children of a filtered Raft rpc.** For the OpenTelemetry layer a filtered span does not exist, so a child span opened under it (for example a follower's `apply` under AppendEntries) becomes a **root** in its own trace. It is not dropped. Follower `apply` spans are therefore exported as roots, and this matches the D5 trace scope ("follower applies are their own roots"). The Raft rpc that carried them is not exported.
  - `InstallSnapshot` gets exactly one span on each side (above) and no per-chunk spans.
- **Propagation:** W3C `traceparent` and `tracestate` in gRPC metadata, with the `TraceContextPropagator`.
  - **Extract** in `RpcService::call` (`observe.rs` ~765).
  - **Inject** in `SendVersion` (client), `ForwardHeaders` (forward to the leader) and `RaftHeaders` (only for `install_snapshot`).
  - A small tonic metadata carrier (injector and extractor) does the work; no HTTP crate is needed.
  - **The client's sync facade** must run each call's future instrumented with the caller's current span (`future.instrument(Span::current())`, captured before entering its runtime). Otherwise the interceptor sees no active context and injects nothing.
- **Sampler:** parent-based, always on by default. `OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG` override it.
- **Overhead:** a manual `rpc_bench` comparison with OTLP on and off (`cargo run --release -p graph-client --example rpc_bench`) is recorded in `docs/spikes/rpc-overhead.md`. It is not a CI assertion.

### D6. Metrics

- **Snapshot families.** All families except the two duration histograms come from one `MetricsSnapshot`:
  - Prometheus: `render()` formats the snapshot, and the output stays byte-identical, so the existing contract tests stay authoritative.
  - OTLP: observable instruments whose callbacks read the snapshot.
  - **The snapshot is built once per collection** (once per reader tick) and shared by every observable callback. Callbacks are cheap and non-blocking: they copy fields out of the cached snapshot and never open a redb transaction or take a lock that serving holds for long.
- **Histograms.** The OpenTelemetry Rust SDK has no observable histogram. So `mg_rpc_duration_seconds` and `mg_apply_duration_seconds` are exported through **synchronous `Histogram` instruments** built with `with_boundaries(DURATION_BUCKETS)`. They are recorded at the same call sites as `observe_rpc` and the apply timer. So the "OTLP equals a scrape" check is **exact for snapshot families** (with a frozen snapshot) and **approximate for the two histograms**.
- **Reader:** a periodic reader at `--otlp-metrics-interval` (default 60 s).
- **Source of truth in code.** The mapping is a const table in `telemetry.rs`. A test parses the table below from this file and compares it with the const table, so the doc and the code cannot drift. A second test checks the const table against `METRIC_NAMES` in both directions.
- **Label sets.** Every attribute keeps its Prometheus label name, except where the semantic convention renames it (the rpc histogram). Counters are monotonic sums with cumulative temporality. Gauges with label sets match Prometheus exactly:
  - `memory_graph.raft.role` reports one data point per role, with value 1 for the current role and 0 for the others, as `mg_raft_role{role}` does;
  - `memory_graph.build.info` reports one point with value 1 and the three attributes.
- **The one exception to 1:1:** `mg_rpc_total` has no OTLP twin. The `rpc.server.call.duration` histogram's count carries the same number, by the same attributes. The contract test lists this exception by name.

| Prometheus name | OTel name | Instrument | Unit | Attributes |
|---|---|---|---|---|
| `mg_raft_term` | `memory_graph.raft.term` | gauge | `{term}` | none |
| `mg_raft_leader_id` | `memory_graph.raft.leader_id` | gauge | `1` | none |
| `mg_raft_role` | `memory_graph.raft.role` | gauge | `1` | `role` |
| `mg_raft_last_log_index` | `memory_graph.raft.last_log_index` | gauge | `{entry}` | none |
| `mg_raft_committed_index` | `memory_graph.raft.committed_index` | gauge | `{entry}` | none |
| `mg_raft_applied_index` | `memory_graph.raft.applied_index` | gauge | `{entry}` | none |
| `mg_raft_snapshot_index` | `memory_graph.raft.snapshot_index` | gauge | `{entry}` | none |
| `mg_raft_purged_index` | `memory_graph.raft.purged_index` | gauge | `{entry}` | none |
| `mg_raft_replication_lag` | `memory_graph.raft.replication_lag` | gauge | `{entry}` | `peer` |
| `mg_store_bytes` | `memory_graph.store.size` | up-down counter | `By` | none |
| `mg_log_bytes` | `memory_graph.log.size` | up-down counter | `By` | none |
| `mg_snapshot_handles_open` | `memory_graph.snapshot_handles.open` | up-down counter | `{handle}` | none |
| `mg_rpc_duration_seconds` | `rpc.server.call.duration` | histogram (synchronous) | `s` | `rpc.system`, `rpc.service`, `rpc.method`, `rpc.grpc.status_code` |
| `mg_rpc_total` | none (the exception: the histogram's count) | none | none | none |
| `mg_writes_forwarded_total` | `memory_graph.writes.forwarded` | counter | `{write}` | none |
| `mg_quorum_probes_total` | `memory_graph.quorum.probes` | counter | `{probe}` | `outcome` |
| `mg_apply_duration_seconds` | `memory_graph.raft.apply.duration` | histogram (synchronous) | `s` | none |
| `mg_build_info` | `memory_graph.build.info` | gauge (always 1) | `1` | `version`, `protocol`, `store_format` |
| `mg_backup_last_success_timestamp` | `memory_graph.backup.last_success.time` | gauge | `s` | none |
| `mg_backup_last_index` | `memory_graph.backup.last_index` | gauge | `{entry}` | none |
| `mg_backup_failures_total` | `memory_graph.backup.failures` | counter | `{failure}` | none |
| `mg_backup_bytes_total` | `memory_graph.backup.written` | counter | `By` | none |
| `mg_mcp_tool_calls_total` | `memory_graph.mcp.tool_calls` | counter | `{call}` | `tool`, `outcome` |
| `mg_read_decodes_total` | `memory_graph.read.decodes` | counter | `{decode}` | `kind` |
| `mg_read_decode_bytes_total` | `memory_graph.read.decode.size` | counter | `By` | `kind` |
| `mg_read_decode_seconds_total` | `memory_graph.read.decode.time` | counter | `s` | `kind` |
| `mg_read_queries_total` | `memory_graph.read.queries` | counter | `{query}` | none |
| `mg_read_query_seconds_total` | `memory_graph.read.query.time` | counter | `s` | none |
| `mg_read_txns_total` | `memory_graph.read.transactions` | counter | `{transaction}` | none |
| `mg_read_dict_strings_total` | `memory_graph.read.dict_strings` | counter | `{string}` | none |
| `mg_queries_total` | `memory_graph.queries` | counter | `{query}` | `rpc` |
| `mg_query_exact_repeats_total` | `memory_graph.queries.exact_repeats` | counter | `{query}` | `rpc` |
| `mg_otel_export_failures_total` (new, D8) | `memory_graph.otel.export.failures` | counter | `{export}` | `signal` |
| `mg_otel_dropped_total` (new, D8) | `memory_graph.otel.dropped` | counter | `{item}` | `signal` |

Sizes that can shrink (`mg_store_bytes`, `mg_log_bytes`, open handles) are up-down counters, following the OTel guidance for additive values. Values that are not additive (indexes, the term, the leader id, timestamps) are gauges. The two new families join `METRIC_NAMES`, which then has 34 entries; 33 of them have an OTLP twin.

- **Cardinality.** Every attribute has a bounded value set:

  | Attribute | Bound |
  |---|---|
  | `rpc` / `rpc.service` + `rpc.method` | the server's fixed methods, plus `unknown` |
  | `outcome`, `rpc.grpc.status_code` | the gRPC codes |
  | `role` | 5 values |
  | `peer` | the cluster members |
  | `tool` | the fixed MCP tools |
  | `kind` | 4 values |
  | `signal` | 3 values |

  The SDK's default limit of 2,000 streams per instrument stays as it is. Overflow lands in the SDK's `otel.metric.overflow=true` stream, and a test checks that no instrument reaches it under the e2e workload.

### D7. Logs

- `opentelemetry-appender-tracing` exports `tracing` events as OTLP log records, behind `--otlp-signals logs`. Each record carries the trace and span ids of the active span, and the severity is mapped from the `tracing` level.
- The filter is the existing `--log-level` / `MEMORY_GRAPH_LOG`, so OTLP gets the same events as stdout.
- **JSON logs gain `trace_id` and `span_id` only when the traces signal is enabled**, and only on events inside a span with a valid context. They never carry zero ids, and events outside a span carry no id fields.
  - Formats: `trace_id` is 32 lowercase hex characters and `span_id` is 16.
  - For one event, the ids in stdout JSON, in the OTLP record and on the span are the same.
  - With OpenTelemetry off, or with traces not among `--otlp-signals`, the JSON format is unchanged (D1).
  - Text logs are unchanged.
- The CLI's `eprintln!` paths stay as they are.
- `logging::init` becomes a `Registry` with the fmt layer (text or JSON, as today), plus the `tracing_opentelemetry` layer and the appender layer only when they are enabled.
- **The exporter never exports its own errors.** The OpenTelemetry crates' own targets (`opentelemetry*`) are filtered out of the appender layer.

### D8. Failure behaviour

- **Bounded queues.** Traces and logs use the SDK batch processors with bounded queues. Metrics use the periodic reader. When a queue is full, new items are dropped. Nothing waits for room.
- **Serving never blocks.** A collector that is down, slow or stalled must never block or fail an RPC, a Raft step or an index batch. Exports run on their own tasks, with the `OTEL_EXPORTER_OTLP_TIMEOUT` timeout. A collector that comes back is used again with no restart.
- **Counting.** Each exporter is wrapped in a thin decorator that counts results:
  - `mg_otel_export_failures_total{signal}`: export calls that failed or timed out, with `signal` `traces`, `metrics` or `logs`;
  - `mg_otel_dropped_total{signal}`: items (spans, log records or metric data points) carried by those failed or timed-out exports.

  **Known gap:** SDK 0.33 does not expose its batch processors' queue-full drops, so they are not counted. That gap goes to a follow-up issue, filed with story 50. The options there are a small custom bounded processor, or an SDK release that exposes the count.

  Both counters sit on `/metrics`, because the collector is the thing that has failed. They stay 0 when OpenTelemetry is off.
- **Export errors are logged** at most once per interval per signal, so a dead collector does not flood the log.
- **Shutdown.** `TelemetryGuard::shutdown()` is async, and `serve` calls it on the way out, before the tokio runtime drops. It flushes traces, metrics and logs and shuts the providers down within a bounded timeout (5 s by default), whether or not the collector answers. `Drop` is only a non-blocking fallback: it signals shutdown and does not wait.
- **Test seams** (story 50):
  - the batch schedule delay, the queue size and the export timeout can be set in tests;
  - `telemetry::init` returns `None` when disabled, and `telemetry::is_active()` reports it, so "off" is checked structurally rather than by waiting for nothing to arrive.

### D9. Privacy

Span attributes and OTLP log fields come from an allow-list: rpc and method names, outcomes and status codes, node and peer ids, counts, sizes, durations and log indexes.

- **Never exported:**
  - query or search text;
  - MCP tool arguments;
  - source snippets or token text;
  - header or metadata values (including `OTEL_EXPORTER_OTLP_HEADERS`).
- **File paths:** repo and file paths are not exported in this ADR. Paths can name customers or projects, and nothing here needs them.
- **Log records:** `opentelemetry-appender-tracing` has no field allow-list of its own. A small custom `LogProcessor` sits in front of the batch processor. In `emit` it strips every attribute that is not on the allow-list before the record is queued for export. The record keeps its message. Stripped fields stay in local logs, as today.
- **Messages must not interpolate values that are not allow-listed.** Neither log messages nor span names may do it: no `info!("search {q}")`. Put the value in a field instead, where it is either allow-listed or stripped. The processor cannot inspect a formatted message, so the sentinel test is what enforces this rule.
- **The test:** stories 51 and 53 send a sentinel query string and check that it appears in no exported attribute, field or body.

### D10. Telemetry module

`crates/graph-server/src/telemetry.rs` holds:

- `TelemetryConfig`, built with the D3 precedence;
- `init(config) -> Option<TelemetryGuard>`, which builds only the providers that are enabled;
- the resource (D4);
- the metadata carrier (D5);
- the metric mapping table (D6);
- the exporter decorators (D8).

`graph_server::testing::FakeCollector` is an in-process tonic server on 127.0.0.1:0 built on `opentelemetry-proto` `gen-tonic`. It records traces, metrics and logs, and can be told to stall, to refuse, or to stop and restart. It is shared by stories 51-54. Tests always use port 0, never 4317, and wait on collector counts with a deadline rather than fixed sleeps.

`graph-client` only propagates the context (inject), and has no exporter. A client process exports nothing, but its context is passed on, so the server's spans join the caller's trace if the caller has one.

**Multi-node trace tests** use `cluster_e2e`: three real `serve` processes export to one in-test `FakeCollector`, whose port is passed with `--otlp-endpoint`. `ClusterTestbed` shares one global subscriber and provider within its process, so it cannot give each node its own resource.

### D11. Out of scope

- TLS to the collector (#104). Run a collector next to the node and let it handle TLS.
- OTLP over HTTP (`http/protobuf`, `http/json`) and per-signal endpoints.
- Exemplars.
- Profiles.
- Per-request tracing of Raft replication and follower applies: that needs trace context in the log payload.
- Counting queue-full drops (the D8 gap, which has a follow-up issue).
- Exporting from CLI commands other than `serve`.

## Open questions (owner)

1. Accept this ADR (Proposed to Accepted).
2. Approve the new dependencies in D2 (in story 50's PR).
3. Exit code 7 (`TELEMETRY_CONFIG`) for an `https://` endpoint given by flag or key. The alternative is the generic code 1.
4. Unsupported `OTEL_*` settings from the environment log an error and disable OTLP, rather than failing startup. Is that the right trade-off, versus failing fast?
5. Should repo and file paths ever be exported (D9)? Today they are not.
6. Whether the queue-full drop gap (D8) must be closed before 1.0, or can wait for the SDK.
7. Whether story 54's otel compose profile gets a CI smoke step, or stays a manual demo. Story 54 proposes a CI step if it costs under 2 minutes of runtime.

## Alternatives considered

| Alternative | Why not |
|---|---|
| A hand-rolled OTLP exporter: vendor the OTLP `.proto` files, generate them with the xtask, and send with the workspace's tonic | It avoids the SDK, but then batching, retries, temporality, the samplers, `OTEL_*` parsing and the W3C propagator would all have to be written and kept up to date by hand. The SDK resolves onto the same tonic and prost and passes the pure-Rust gate, so it is cheaper and more correct. |
| OTLP over HTTP (`http/protobuf`) | It needs an HTTP client (`reqwest` or `hyper-util`), and with TLS it pulls in `rustls`, which the TLS features bring with `ring` or `aws-lc`. gRPC reuses the tonic already in the binary. Most collectors accept both protocols, on 4317 and 4318. |
| Prometheus scraping only (keep `/metrics` and let the collector scrape it) | It covers metrics only: no traces, no correlated logs and no cross-node trace for a forwarded request, which is the main gap. The scrape still works and stays the contract, so operators who want only metrics lose nothing. |

## Consequences

- **Positive:**
  - One trace follows a request from the client, to the follower, to the leader, and into the leader's apply.
  - Logs correlate with traces, in OTLP and in the JSON logs when traces are on.
  - OTLP metrics have no second source of truth: snapshot families read one snapshot, and the histograms record at the same call sites.
- **Negative:**
  - Five new direct dependencies plus their transitive tree, pinned exactly, so every upgrade is a deliberate bump of the whole OpenTelemetry set.
  - A larger binary, even with OpenTelemetry off.
  - Two naming schemes to keep in step. The const table and its tests are what keep them in step.
  - Replication and follower applies are not in the request's trace.
  - Queue-full drops are not counted yet.
  - With traces on, the parent-based always-on default sends every span. Busy clusters should set `OTEL_TRACES_SAMPLER`.
- **Neutral:**
  - The Prometheus `/metrics` output and its tests are unchanged, apart from the two new families, which story 52 adds.
  - No on-disk format change, no wire protocol change (W3C headers are ordinary metadata), no Raft log change, and no `PROTOCOL_VERSION` bump.
- **Delivery:**
  - Story 50 (groundwork) can merge before acceptance, because it changes no behaviour.
  - Stories 51-54 merge only after the owner accepts this ADR.
  - Story 52 depends only on 50, and can run in parallel with 51.
