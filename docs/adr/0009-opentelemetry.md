# ADR 0009: OpenTelemetry (OTLP traces, metrics and logs)

**Status:** Proposed on 2026-10-06. The owner decides. The owner made the scope decisions below on 2026-10-06 (signals, transport, default). Builds on [ADR 0004](0004-client-server-and-replication.md) D10 (observability), which gets a dated note pointing here. Epic amendment, proposed: stories 50-54 in the [epic](../epic-code-memory-graph.md). They are not counted in the epic totals until this ADR is accepted.

## In plain words

1. Today `serve` has `tracing` logs (text or JSON), a hand-written Prometheus `/metrics` endpoint and health checks. There is no distributed tracing and no OpenTelemetry (OTLP) export.
2. This ADR adds all three OpenTelemetry signals (traces, metrics and logs). They are exported over OTLP/gRPC without TLS to an OpenTelemetry Collector running next to the node. The collector handles TLS and auth onward.
3. It is **off by default**. Nothing related to OpenTelemetry runs until an endpoint is set, by a flag, a config key or the standard `OTEL_*` environment variables.
4. Metrics come from one shared snapshot, so Prometheus and OTLP always report the same numbers. The Prometheus output stays byte-identical.
5. A collector that is down or slow never blocks or fails an RPC, a Raft step or an index batch. Telemetry is dropped and counted instead.

## Context

References are to `origin/main` on 2026-10-06.

- **Logs.** `crates/graph-cli/src/logging.rs::init` (~36-50, called from `main.rs` ~1176) installs a plain `tracing_subscriber::fmt().try_init()`. It is not a layered `Registry`, so another layer cannot be added.
- **Metrics.** `render()` (`crates/graph-server/src/observe.rs` ~310) writes Prometheus text straight from about ten sources: openraft metrics, the log store, forward counts, quorum probes, MCP, backup, repeats and read stats. There is no snapshot type in between. `METRIC_NAMES` lists the 32 families (the contract), and `DURATION_BUCKETS` holds the histogram bounds (0.5 ms to 10 s).
- **Spans that exist.** `RpcLayer` opens an `rpc` span per RPC (`observe.rs` ~777-802, installed in `server.rs` ~908). The state machine opens an `apply` span per Raft apply (`raft/state_machine.rs` ~166).
- **Spans that are missing.** MCP calls, index batches and `graph-client` have none. `#[instrument]` is used nowhere.
- **Interceptors where a trace context can travel:**
  - `RpcService::call` (`observe.rs` ~770), on the server side;
  - `ForwardHeaders` (`forward.rs` ~102-118), from a follower to the leader;
  - `RaftHeaders` (`raft/network.rs` ~373-381), from the leader to its peers;
  - `SendVersion` (`graph-client/src/conn.rs` ~248-260), from the client.
- **Config.** Every serve flag gets a matching TOML key automatically (`serve_config.rs`). Keys that look like secrets are refused.
- **Dependencies.** The workspace pins tonic `=0.14.6` and prost `=0.14.4`. The pure-Rust gate (`scripts/check-no-c-deps.py`) and `test_gate.py` forbid `ring` and `aws-lc-sys`.

## Decision

### D1. Owner decisions (2026-10-06)

- **Signals:** traces, metrics and logs, all three.
- **Transport:** OTLP over gRPC, with no TLS, to a local or sidecar OpenTelemetry Collector. The collector handles TLS and auth onward. TLS in the process waits for #104.
- **Default:** off, and opt-in. With it off, no OpenTelemetry layer, provider or exporter task exists.

### D2. Crates, versions and features

Exact pins in `[workspace.dependencies]`, as the workspace does for tonic and prost.

| Crate | Pin | Features |
|---|---|---|
| `opentelemetry` | `=0.33.0` | default |
| `opentelemetry_sdk` | `=0.33.0` | `rt-tokio`, plus `trace`, `metrics`, `logs` as needed; review the default features before pinning and turn off any that are not needed |
| `opentelemetry-otlp` | `=0.33.0` | `default-features = false`; `grpc-tonic`, `trace`, `metrics`, `logs` |
| `tracing-opentelemetry` | `=0.34.0` | default |
| `opentelemetry-appender-tracing` | `=0.33.0` | default |
| `opentelemetry-proto` | `=0.33.0`, dev-dependency only | `gen-tonic` (with the trace, metrics and logs services), for the fake collector in tests |

- **Avoid:** the `tls`, `tls-*`, `reqwest-rustls`, `reqwest-*` and `zstd-*` features of `opentelemetry-otlp`, and `with-schemars` on `opentelemetry-proto`. They pull in `ring`, `aws-lc`, `reqwest`, a C zstd, or code nobody uses.
- **Checks in story 50, before pinning:**
  - `tracing-opentelemetry 0.34.0` really targets `opentelemetry 0.33`. If not, take the release that does and record it here with a dated note.
  - `opentelemetry-otlp` resolves onto the workspace's tonic `=0.14.6` and prost `=0.14.4`. `cargo tree -d` must show no second tonic or prost.
  - `cargo tree -i ring` and `cargo tree -i aws-lc-sys` print nothing, and `check-no-c-deps.py` and `test_gate.py` pass.
- The OTLP messages come as generated code inside `opentelemetry-proto`, so no protoc and no xtask change is needed.
- Adding these dependencies needs the owner's approval, given in story 50's PR.

### D3. Configuration

| Flag (serve) | Config key | Environment | Default |
|---|---|---|---|
| `--otlp-endpoint <url>` | `otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset, which means off |
| `--otlp-signals traces,metrics,logs` | `otlp-signals` | none | all three, once an endpoint is set |
| `--otel-service-name <name>` | `otel-service-name` | `OTEL_SERVICE_NAME` | `memory-graph` |
| `--otlp-metrics-interval <duration>` | `otlp-metrics-interval` | none | `60s` |
| none | none | `OTEL_EXPORTER_OTLP_HEADERS` | none |
| none | none | `OTEL_RESOURCE_ATTRIBUTES` | none |
| none | none | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | parent-based, always on |

- **Enabling.** An endpoint from any source turns OpenTelemetry on. `--otlp-signals` only narrows which signals are sent.
- **Precedence:** the flag, then the config key, then the environment.
- **Headers come from the environment only** (`OTEL_EXPORTER_OTLP_HEADERS`). There is no flag or key, because auth headers are secrets and the serve config refuses keys that look like secrets. They are never logged.
- An endpoint that is not `http://` is refused at startup with a clear error, because TLS is out of scope (D10).
- The scope is `serve`. Other CLI commands do not export.

### D4. Resource attributes

| Attribute | Value |
|---|---|
| `service.name` | `memory-graph`, or `--otel-service-name` / `OTEL_SERVICE_NAME` |
| `service.version` | the crate version, as in `mg_build_info{version}` |
| `service.instance.id` | the Raft node id |
| `memory_graph.cluster` | the cluster id |
| `host.name` | the host name, reusing the existing hostname logic (the StatefulSet pod name on Kubernetes) |

`OTEL_RESOURCE_ATTRIBUTES` adds more. Where it names one of the keys above, the value here wins, so the node identity cannot be spoofed by the environment.

### D5. Traces

- **Semantic conventions** on the rpc span: `rpc.system=grpc`, `rpc.service`, `rpc.method`, `rpc.grpc.status_code` and `server.address`.
- **Span tree:**
  - one `rpc` span per RPC (the existing `RpcLayer` span, with the attributes above);
  - a forward hop span carrying `mg.forwarded_by`, as a child of the follower's rpc span; the leader's rpc span is its child;
  - Raft append (leader to followers) and apply (the existing `apply` span), plus snapshot install;
  - MCP `tools/call` spans;
  - index batches (`index_dir` / store `index_batch`);
  - `RemoteStore` calls in `graph-client`.
- **Propagation:** W3C `traceparent` and `tracestate` in gRPC metadata, with the `TraceContextPropagator`.
  - **Extract** in `RpcService::call` (`observe.rs` ~770).
  - **Inject** in the client's `SendVersion` (`graph-client/src/conn.rs` ~248-260), in `ForwardHeaders` (`forward.rs` ~102-118) for forwards to the leader, and in `RaftHeaders` (`raft/network.rs` ~373-381) for Raft from the leader to followers.
  - A small tonic metadata carrier (injector and extractor) does the work; no HTTP crate is needed.
- **Sampler:** parent-based, always on by default. `OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG` override it.
- Raft heartbeats get no span of their own, so an idle cluster does not flood the collector.

### D6. Metrics

- **One shared `MetricsSnapshot`.** One struct is collected from all the sources `render()` reads today. It feeds both:
  - Prometheus, where `render()` formats the snapshot and the output stays **byte-identical**, so the existing contract tests stay authoritative;
  - OTLP observable instruments, whose callbacks read the same snapshot.
- **Reader:** a periodic reader at `--otlp-metrics-interval` (default 60 s).
- **Histograms** use the same `DURATION_BUCKETS` as explicit bucket bounds.
- **Naming:** OTel style, mapped 1:1 from `METRIC_NAMES`. Every Prometheus family has exactly one OTLP twin, and a contract test checks both directions. A family added to `METRIC_NAMES` without a row here fails that test.

Every attribute keeps its Prometheus label name. Counters are monotonic sums with cumulative temporality.

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
| `mg_rpc_duration_seconds` | `memory_graph.rpc.duration` | histogram | `s` | `rpc`, `outcome` |
| `mg_rpc_total` | `memory_graph.rpc.calls` | counter | `{call}` | `rpc`, `outcome` |
| `mg_writes_forwarded_total` | `memory_graph.writes.forwarded` | counter | `{write}` | none |
| `mg_quorum_probes_total` | `memory_graph.quorum.probes` | counter | `{probe}` | `outcome` |
| `mg_apply_duration_seconds` | `memory_graph.raft.apply.duration` | histogram | `s` | none |
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

Sizes that can shrink (`mg_store_bytes`, `mg_log_bytes`, open handles) are up-down counters, following the OTel guidance for additive values. Values that are not additive (indexes, the term, the leader id, timestamps) are gauges. The two new families join `METRIC_NAMES`, which then has 34 entries.

### D7. Logs

- `opentelemetry-appender-tracing` exports `tracing` events as OTLP log records, behind `--otlp-signals logs`. Each record carries the trace and span ids of the active span, and the severity is mapped from the `tracing` level.
- The filter is the existing `--log-level` / `MEMORY_GRAPH_LOG`, so OTLP gets the same events as stdout.
- **JSON logs gain `trace_id` and `span_id`** whenever a span with a valid context is active, **even with OTLP off**. Nothing else in the JSON log format changes. Text logs are unchanged.
- The CLI's `eprintln!` paths stay as they are.
- `logging::init` becomes a `Registry` with the fmt layer (text or JSON, as today), plus the `tracing_opentelemetry` layer and the appender layer only when they are enabled.

### D8. Failure behaviour

- **Batched, with bounded queues.** Traces and logs use batch processors with bounded queues, and metrics use the periodic reader. When a queue is full, new items are dropped and counted. Nothing waits for room.
- **Serving never blocks.** A collector that is down, slow or refusing must never block or fail an RPC, a Raft step or an index batch. Exports run on their own tasks, with timeouts.
- **Shutdown flushes** all providers with a bounded timeout (`TelemetryGuard::drop`), then exits whether or not the flush finished.
- **New Prometheus counters,** each with an OTLP twin (D6):
  - `mg_otel_export_failures_total{signal}`: export calls that failed, with `signal` `traces`, `metrics` or `logs`;
  - `mg_otel_dropped_total{signal}`: spans, log records or metric batches dropped because a queue was full or an export failed.

  They sit on `/metrics` because the collector is the thing that has failed. They stay 0 when OpenTelemetry is off.
- Export errors are logged at most once per interval per signal, so a dead collector does not flood the log. The log exporter never exports its own error events.

### D9. Telemetry module

`crates/graph-server/src/telemetry.rs` holds the `TelemetryConfig` (built with the D3 precedence), `init(config) -> TelemetryGuard` (it builds only the providers that are enabled), the resource (D4) and the metadata carrier (D5). `graph-client` gets only the propagation (inject), with no exporter. A client process exports nothing, but its context is passed on so the server's spans join the caller's trace if the caller has one.

### D10. Out of scope

- TLS to the collector (#104). Run a collector next to the node and let it handle TLS.
- OTLP over HTTP (`http/protobuf`, `http/json`).
- Exemplars.
- Profiles.
- Exporting from CLI commands other than `serve`.

## Alternatives considered

| Alternative | Why not |
|---|---|
| A hand-rolled OTLP exporter: vendor the OTLP `.proto` files, generate them with the xtask, and send with the workspace's tonic | It avoids the SDK, but then batching, retries, temporality, the samplers, `OTEL_*` parsing and the W3C propagator would all have to be written and kept up to date by hand. The SDK resolves onto the same tonic and prost and passes the pure-Rust gate, so it is cheaper and more correct. |
| OTLP over HTTP (`http/protobuf`) | It needs an HTTP client (`reqwest` or `hyper-util`), and with TLS it pulls in `rustls`, which the TLS features bring with `ring` or `aws-lc`. gRPC reuses the tonic already in the binary. Most collectors accept both protocols on 4317 and 4318. |
| Prometheus scraping only (keep `/metrics` and let the collector scrape it) | It covers metrics only: no traces, no correlated logs and no cross-node trace for a forwarded request, which is the main gap. The scrape still works and stays the contract, so operators who want only metrics lose nothing. |

## Consequences

- **Positive:**
  - One trace follows a request from the client, to the follower, to the leader, and through Raft to the followers.
  - Logs correlate with traces, in OTLP and in the JSON logs.
  - OTLP metrics come with no second source of truth, because both outputs read one snapshot.
- **Negative:**
  - Five new direct dependencies plus their transitive tree, pinned exactly, so every upgrade is a deliberate bump of the whole OpenTelemetry set.
  - A larger binary, even with OpenTelemetry off.
  - Two naming schemes to keep in step. The contract test is what keeps them 1:1.
  - With traces on, the parent-based always-on default sends every span. Busy clusters should set `OTEL_TRACES_SAMPLER`.
- **Neutral:**
  - The Prometheus `/metrics` output and its tests are unchanged, apart from the two new families.
  - No on-disk format change, no wire protocol change (W3C headers are ordinary metadata), and no `PROTOCOL_VERSION` bump.
- **Delivery:** stories 50-54. Story 50 (groundwork) can merge before acceptance, because it changes no behaviour. Stories 51-54 merge only after the owner accepts this ADR.
