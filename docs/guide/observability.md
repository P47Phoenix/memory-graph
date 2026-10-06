# Observability

Logs, Prometheus metrics and health probes for `memory-graph serve` (a `--db` server or a [cluster](cluster.md) node).

```sh
memory-graph serve --data-dir ./n1 --listen 127.0.0.1:7001 --metrics-listen 127.0.0.1:9101 --log-format json
curl -s 127.0.0.1:9101/metrics                                   # Prometheus text format 0.0.4
memory-graph --server 127.0.0.1:7001 health --ready && echo ready  # exit 0 ready, 1 not (or unreachable)
MEMORY_GRAPH_LOG=debug memory-graph serve ...                     # RPC and apply spans
```

## Logs

Logs go to stderr. Stdout carries only the `listening on` start line, and `metrics on` before it; under `--log-format json` these are JSON objects too, with `event` (`metrics`/`listening`) and `addr`, and their `message` keeps the text form, so a container runtime that merges stdout into the log stream sees only JSON.

- `--log-format text` (default) or `json`: one object per line with `timestamp`, `level`, `target`, `message`, the event's fields beside it, and `span` (the innermost span and its fields).
- `--log-level <filter>` (or `MEMORY_GRAPH_LOG`; default `info`) takes `tracing` EnvFilter syntax, e.g. `info,graph_server=debug`.
- At `debug`, every gRPC call logs `rpc finished` inside an `rpc` span (`method`, `peer`, `outcome`, `duration_ms`) and every applied log entry `applied` inside an `apply` span (`index`, `kind`, `files`, `duration_ms`).

## Metrics

`--metrics-listen HOST:PORT` serves `GET /metrics` (plain HTTP/1.1, one request per connection, at most 64 connections at once and 5 s to send the request; anything else is 404/405). `Admin.Metrics` returns the same text over gRPC. Names are stable:

| Metric | Type | Meaning |
|---|---|---|
| `mg_raft_term` | gauge | Current Raft term |
| `mg_raft_leader_id` | gauge | Known leader's node id (0: none) |
| `mg_raft_role{role}` | gauge | 1 for the current role (`leader`, `follower`, `candidate`, `learner`, `shutdown`) |
| `mg_raft_last_log_index`, `mg_raft_committed_index`, `mg_raft_applied_index` | gauge | Last log entry, last known committed, last applied to the store |
| `mg_raft_snapshot_index`, `mg_raft_purged_index` | gauge | Current snapshot's index, last purged index (0: none) |
| `mg_raft_replication_lag{peer}` | gauge | Leader only: entries each peer is behind the leader's last log index |
| `mg_store_bytes`, `mg_log_bytes` | gauge | Store file and Raft log file sizes |
| `mg_snapshot_handles_open` | gauge | Open snapshot handles held for paging clients |
| `mg_rpc_duration_seconds{rpc,outcome}` | histogram | gRPC call duration until the response headers (buckets 0.5 ms to 10 s); `rpc` like `Store/Search` (`unknown` for any path that is not one of the server's methods, so a client cannot create label values), `outcome` `ok` or the gRPC code (`unavailable`, `failed_precondition`, ...) |
| `mg_rpc_total{rpc,outcome}` | counter | gRPC calls served |
| `mg_writes_forwarded_total` | counter | Writes and membership changes forwarded to the leader |
| `mg_quorum_probes_total{outcome}` | counter | Leader only: health probes of voters that went silent while a write waited (`--quorum-loss-timeout`); `outcome` `alive` (answered `SERVING`) or `dead` |
| `mg_apply_duration_seconds` | histogram | Time to apply one committed log entry |
| `mg_build_info{version,protocol,store_format}` | gauge | Always 1 |
| `mg_backup_last_success_timestamp`, `mg_backup_last_index` | gauge | Unix time and log index of the last snapshot backup committed (`--backup-url`; 0: none) |
| `mg_backup_failures_total`, `mg_backup_bytes_total` | counter | Backups that failed after every retry; bytes written by successful ones |
| `mg_mcp_tool_calls_total{tool,outcome}` | counter | MCP tool calls served by `serve --mcp-listen` ([docs/mcp.md](../mcp.md)) |
| `mg_read_decodes_total{kind}` | counter | Decodes done by queries; `kind` `dict` (reverse-dictionary block scans), `symbol` (symbol sections), `lazy` (stream headers) or `full` (whole streams) |
| `mg_read_decode_bytes_total{kind}` | counter | Encoded bytes behind those decodes: dict blocks scanned, symbol sections, for `lazy` the whole encoded size of each stream whose header was decoded (symbol bytes lie within it), for `full` the whole stream. Do not sum across kinds |
| `mg_read_decode_seconds_total{kind}` | counter | Seconds inside those decodes (`--read-timing` only, else 0) |
| `mg_read_queries_total`, `mg_read_query_seconds_total` | counter | Store read calls (including rejected or expired ones), and their wall time including the read-transaction open (`--read-timing` only) |
| `mg_read_txns_total`, `mg_read_dict_strings_total` | counter | Read transactions opened by store reads (snapshot reads reuse one); dictionary strings allocated by term lookups |

The `mg_read_*` families (read cache phase 0, [ADR 0008](../adr/0008-read-cache.md)) count the store's read path. The share of query time spent decoding is the sum over kinds of `mg_read_decode_seconds_total` divided by `mg_read_query_seconds_total`; query time includes opening the read transaction. Counts are always on; the seconds families need `serve --read-timing` (config key `read-timing = true`, default off), because reading the clock around every decode is not free on every platform. The counters and the timing switch are **process-wide**: several servers sharing one process (as in tests) report the same totals, and once `--read-timing` turns timing on it stays on for the life of the process.

`cluster status --json` reports the same Raft and store numbers, `writes_forwarded_total`, `rpcs_total` and `entries_applied_total`; on the leader, `replication` lists each peer's `matched_index`, `lag` and `last_error` (the last failed Raft RPC to it, kept while RPCs to it keep failing or while it lags behind the leader; empty once it answers and has caught up).

Contract note on `outcome`: it is read from the response headers, so an error a streaming call (`Descendants`, `FileTokens`, snapshot download) reports in its trailers after its first message counts as `ok`; alert on stream failures from the client side.

## Health

gRPC `grpc.health.v1`:

- The default service (`""`) is `SERVING` once the store is open.
- `memory-graph.ready` is `SERVING` only while a leader is known, a leader's `AppendEntries` (heartbeats included; learners get them too) reached this node within three maximum election timeouts, and it has applied to within `--ready-max-lag` entries (default 1000) of the leader's commit index. A learner still catching up, or one cut off from the leader, is not ready; an idle, caught-up one is.
- Both go `NOT_SERVING` as soon as a shutdown starts.

`memory-graph health [--ready] --server <addr>` exits 0 when serving, 1 otherwise. It is the probe for the shell-less image: the Docker `HEALTHCHECK` runs `health`, Compose and Kubernetes use `--ready`.

## Deployment

[docs/deploy/compose.md](../deploy/compose.md), [docs/deploy/kubernetes.md](../deploy/kubernetes.md) and [docs/deploy/data-dir.md](../deploy/data-dir.md); see also the [cluster guide](cluster.md#deployment).
