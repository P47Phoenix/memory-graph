# Server mode

A database file is opened by one process at a time. To share one between processes, machines or containers, serve it and point the other commands at the server:

```sh
memory-graph serve --db ./g --listen 127.0.0.1:7000     # prints: memory-graph serve: listening on 127.0.0.1:7000 (db ./g, node 1, 16 worker threads)
memory-graph --server 127.0.0.1:7000 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7000 search foo --language rust
export MEMORY_GRAPH_SERVER=127.0.0.1:7000                 # every command in this shell now uses the server
memory-graph describe
memory-graph health && echo up                            # exit 0 when serving, 1 when not
```

For a replicated cluster (`serve --data-dir`), see the [cluster guide](cluster.md). For logs, metrics and health probes, see [observability](observability.md).

## Same commands, same answers

Every command takes `--server` in place of `--db` and prints byte for byte what it prints on the file (tested on the vendored corpus). `index --server` reads and sends the files; the server parses and commits them (the progress view shows `send` and `replicate: acked by leader N (idx K)` stages, and `--stats` an `rpc` row). Reads run while an index writes.

Also over `--server`: `sysinfo` prints the server machine's report under `server <addr> node N (leader: M)`; `vacuum --compact` compacts the server's file; `health [--ready]` and `cluster status [--json]` / `cluster leader` report on the node.

## Choosing the target

`--db` and `--server` are exclusive; `MEMORY_GRAPH_SERVER` stands in for `--server` (the flag wins), and `--db` together with either is an error that names both. Neither means `./graph.redb`. `--read linearizable` (or `MEMORY_GRAPH_READ`) makes reads wait until they see every acknowledged write; the default `local` reads the node's store as it is (the same thing on a single node).

## Settings that belong to the server

`--cache-bytes` goes to `serve`; `--chunk-bytes` is refused with `--server` (the server cuts its log entries at 8 MiB itself); `--jobs` only sizes the client's reading threads (a warning says so); the disk guard runs on the server, which reports a full disk as an error.

## `serve` options and the LOCK file

- `--listen` defaults to `127.0.0.1:7000` (`0.0.0.0:7000` to accept other machines; port `0` picks a free port and the printed line names it).
- `--node-id` (default 1 with `--db`), `--cache-bytes` (default derived from available memory, sampled once when the store opens; see [storage](storage.md)), `--snapshot-max-age` (how long a paging client's frozen view may live, default `15m`).
- `--worker-threads N` (env `MEMORY_GRAPH_WORKER_THREADS`, config key `worker-threads`; 1 to 1024) sets the server's async worker threads; the `listening on` line reports the count. Precedence: the flag, then `MEMORY_GRAPH_WORKER_THREADS`, then the config file, then tokio's own `TOKIO_WORKER_THREADS` (checked the same way: 0, over 1024 or a non-number is an error, exit 2), then one per CPU the process may use. On a many-core host or a Docker Desktop VM, 2-4 workers serve a typical team with fewer threads, less memory and somewhat less idle CPU. Idle workers park, so this barely changes the idle wakeup rate: that is set by the Raft tick, which a node leading alone (any `--db` server) suspends ([ADR 0004](../adr/0004-client-server-and-replication.md)).
- With `--db` it writes `<db>.LOCK` (`{"pid", "listen", "started"}`) next to the file and removes it on a graceful stop: Ctrl-C, SIGTERM, `docker stop`; on Windows Ctrl-C or Ctrl-Break, which is what a supervisor sends a console process started in its own process group (e.g. Python's `send_signal(signal.CTRL_BREAK_EVENT)`). `taskkill /F` is a kill, not a graceful stop. The Raft log lives in `<db>.raft.redb`. `--data-dir` ([cluster guide](cluster.md)) keeps everything in one directory instead.

A served file opened directly waits up to 5 s for the lock (`MEMORY_GRAPH_LOCK_WAIT_MS` changes that), then says who holds it: `database ./g is locked by pid 4242 (memory-graph serve on 127.0.0.1:7000); use --server 127.0.0.1:7000 or stop it`. Once the server stops, the file opens directly again and answers exactly as the server did.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failure (and `health`: not serving; a read whose server is unreachable or whose connection was lost) |
| 3 | `cluster leader` found no leader |
| 4 | A write was not acknowledged within its deadline (`--write-deadline`, default 10 s of retries): no leader, the server unreachable, or the connection lost mid-write; or a `--read linearizable` read that found no leader within `--read-deadline` |
| 5 | The server speaks another protocol or store format version |
| 6 | A data directory (or a node named in a membership change) belongs to another cluster (`WrongCluster`) |
| 7 | `serve` refused its OpenTelemetry settings (an `https://` or malformed `--otlp-endpoint` / `otlp-endpoint`, a bad `--otlp-signals`) or could not build the exporters (`TELEMETRY_CONFIG`, [ADR 0009](../adr/0009-opentelemetry.md)). A problem in an `OTEL_*` environment variable is not fatal: it logs one error and `serve` runs with OpenTelemetry off |

## Retries and write deadlines

**An error does not prove a write failed.** A write that fails with a lost connection or exit code 4 may still have been applied (the server can commit it and die before answering). Rerunning it is safe: `index` skips unchanged files by fingerprint, `prune` and `vacuum` are idempotent, and `ingest` of the same extraction stores the same thing. A retried write reports what the retry did: a `prune` that landed before the connection was lost reports 0 removed on the retry, though the stored state is correct.

**Write deadline.** `--write-deadline <duration>` (or `MEMORY_GRAPH_WRITE_DEADLINE`; e.g. `500ms`, `10s`, `2m`; default `10s`) is how long a write keeps retrying through no leader or a lost connection before it fails with exit code 4. A membership change refused only for the moment (`cluster remove`'s quorum check right after a leader change, see the cluster guide) is retried within the same deadline, and then fails with that refusal (exit code 1).

## Upgrades: one binary version per cluster during writes

**Run one `memory-graph` version on every node of a cluster while it takes writes.** Upgrade between write bursts: stop writing, upgrade and restart the nodes, then write again. There is no guard for this; it is up to you (#212).

Why: every replica applies a write itself, running its own build's code on it. Two builds can store the same file differently. For example, the build before the span safety net (#203) rejects a file whose symbol spans are invalid, while a later build stores its tokens with no symbols and a span warning. Replicas on different builds then hold different data and give different answers. Nodes refuse Raft traffic, snapshots and joins from a peer whose extractor versions (or store format, tokenizer or decoder versions) differ, so a bump of those stops replication rather than diverging. A change in behaviour that keeps those versions, like the one above, is not caught.

## Not yet: TLS and authentication

TLS (issue #104, once a pure-Rust provider passes the no-C gate) and authentication (#105), see [ADR 0004](../adr/0004-client-server-and-replication.md); bind to loopback or a private network meanwhile.
