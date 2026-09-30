# Documentation index

Start here. Each document opens with a TL;DR, then details, then links to raw data.

## Suggested reading order for a newcomer
1. The [README](../README.md) quick start, then the [guides](#guides) for the parts you use.
2. [Glossary](glossary.md): plain meanings of every technical word. Skim it, and come back when a word is unclear.
3. The "In plain words" box at the top of [ADR 0001](adr/0001-storage.md) and [ADR 0002](adr/0002-parsing-and-crate-layout.md). [ADR 0003](adr/0003-data-model.md) and [ADR 0004](adr/0004-client-server-and-replication.md) also open with an "In plain words" section (no special vocabulary, then a table of the decisions made and the questions still open).
4. The [epic](epic-code-memory-graph.md), to see the goal and the planned stories.
5. The technical sections of the ADRs, then the [spikes](#spikes-evidence) for evidence.
6. [Learnings](learnings.md) as a quick list of key facts and pitfalls.

## Guides
How to use and run memory-graph (the [README](../README.md) has the quick start):
- [guide/indexing.md](guide/indexing.md): incremental indexing, pruning, paths, per-file failures, sizing (threads, memory, disk), progress and traces.
- [guide/querying.md](guide/querying.md): search grains, `--kind`, symbol patterns, paging, JSON output, MCP.
- [guide/server.md](guide/server.md): `memory-graph serve`, choosing `--db` or `--server`, the LOCK file, exit codes, retries and write deadlines.
- [guide/cluster.md](guide/cluster.md): a Raft cluster: bootstrap, tuning, TOML config, backups (with an S3 walkthrough), join/promote/remove, read modes.
- [guide/observability.md](guide/observability.md): logs, metrics (the stable metric names) and health probes.
- [guide/docker.md](guide/docker.md): the container image.
- [guide/languages.md](guide/languages.md): supported languages, extensions and tokenizer dialects.
- [guide/storage.md](guide/storage.md): the on-disk format, size, reclaiming space, the retired v1 format.
- [guide/development.md](guide/development.md): building, tests, CI gates, the test corpus, using it as a library.
- [mcp.md](mcp.md): connecting AI assistants over MCP.

## Glossary
- [glossary.md](glossary.md): every technical term in plain words, in alphabetical order.

## Product
- [Epic: Language-agnostic code memory graph](epic-code-memory-graph.md): the goal, how we measure success, and the list of planned stories (1-19, plus 20-25 for the client/server and cluster work added by ADR 0004, accepted 2026-09-28).

## Architecture decision records
| ADR | Status | One line |
|---|---|---|
| [0001 Storage engine](adr/0001-storage.md) | Accepted (provisional); JSON-node part proposed to be superseded by 0003 | Where the data lives: the redb database with our own graph on top, each node knowing its `parent`. |
| [0002 Parsing and crate layout](adr/0002-parsing-and-crate-layout.md) | Accepted | How the code is split into three crates, how language readers report spans, the fallback tokenizer for any language, and the pure-Rust check in CI. |
| [0003 Data model](adr/0003-data-model.md) | **Proposed** (not accepted; Q4 and Q5 decided by the user 2026-09-20) | A proposal to store tokens in a much smaller form (a dictionary, one stream per file, and count postings) instead of one record per token; also covers sharding, snapshots and migration. Opens with an "In plain words" section. Decided: an owning daemon for cross-process access, and shard by (org, repo) with the sharding build deferred. |
| [0004 Client/server and replication](adr/0004-client-server-and-replication.md) | **Accepted** (by the user, 2026-09-28) | Runs memory-graph as a real database: `memory-graph serve` reached over gRPC from any machine, hostable as a Raft cluster where every node serves reads, any node accepts writes, and a write is acknowledged only once a majority has it on disk. Supersedes the Unix-socket transport of ADR 0003 Q5. Opens with an "In plain words" section. |
| [0005 MCP access](adr/0005-mcp.md) | **Accepted** (by the user, 2026-09-29) | Read-only MCP tools over stdio (`memory-graph mcp`) and an opt-in loopback streamable HTTP endpoint (`serve --mcp-listen`), so AI assistants can query the graph. |
| [0006 Snapshots to object storage](adr/0006-snapshots-object-storage.md) | **Accepted** (by the user, 2026-09-29) | The leader copies each Raft snapshot to `file://` or S3-compatible storage (`--backup-url`), and `--restore` seeds a new cluster from one. |
| [0007 Source encodings](adr/0007-source-encodings.md) | **Proposed** (2026-09-30) | Index files in any encoding (UTF-16, Windows-1252, the ANSI code page, Shift_JIS, GBK, Big5, ...): each file is decoded to UTF-8, spans point into the decoded text, the encoding is recorded, and `--encoding` overrides detection. |

## Spikes (evidence)
| Spike | TL;DR | Raw data |
|---|---|---|
| [Parsing](spikes/parser.md) | Experiment on reading code: `syn` finds Rust symbols, the fallback tokenizer finds tokens. | none |
| [Storage](spikes/storage.md) | Experiment on the database: redb works, at about 690 bytes per token. | none |
| [Data model](spikes/data-model.md) | Experiment on size: the cost per token is the record around it, not the text; a stream model measured about 25x smaller. | [spikes/data-model/](../spikes/data-model/README.md) (code, README, logs) |
| [Q4/Q5 decision paper](spikes/q4-q5-decision-paper.md) | Decision record (not an ADR), decided by the user 2026-09-20: a daemon for cross-process access, shard by (org, repo) with the build deferred; keeps the options and evidence. | none |
| [v2 store checkpoint](spikes/v2-checkpoint.md) | ADR story 4 go/no-go input at 9.9 M tokens (not a decision): v2 is 24x smaller and 5.1x faster to ingest, no query worse than 1.51x v1; on real `syn` alone selective token search is 2.7x v1 and size is 39.95 B/token. Includes the fixed `search_symbols` regression (#22). | [spikes/data-model/logs/](../spikes/data-model/README.md) (`v2_checkpoint_*`), harness `crates/graph-store/examples/v2bench.rs` |
| [Daemon and locking](spikes/daemon-and-locking.md) | Experiment on cross-process access: a socket daemon adds under 1 ms per query even at 9.9 M tokens (5 ms trigger not tripped); today's lock is held for a whole index run (20 s per 2.5 M tokens) and retries starve under load. | [spikes/daemon/](../spikes/daemon/README.md) (code, README, logs) |
| [RPC overhead](spikes/rpc-overhead.md) | Measurement for epic story 20 (not a decision): the gRPC server adds 0.06 ms per empty call and 0.13-0.16 ms per query at p50 over the embedded store on the corpus (5 ms trigger not tripped); found and fixed a needless paging round trip for small unlimited searches. | harness `crates/graph-client/examples/rpc_bench.rs` |
| [Raft replication](spikes/raft-replication.md) | Measurement for epic story 21 (not a decision). Replicated ingest runs at 94% of embedded on one and on three loopback nodes (the 50% trigger is not tripped). A log entry is its payload plus 25 B. fsync-bound throughput is 664 / 392 small entries/s (1 / 3 nodes). The corpus snapshot builds in 0.15 s and installs in 0.17 s. Measuring found that `raft.redb` never shrank after a purge, fixed by compacting the log after a purge. Stage F (story 25) adds the run at 10 M tokens: 71-81% of embedded ingest, snapshot install in 1.2 s, RPC overhead p50 under 1.6 ms, no trigger tripped on an idle machine (one loaded run at 43%, #123). It also adds a 62-minute soak with 12 restarts: no acknowledged write lost, `raft.redb` under 4 MB. | ignored tests `measure_replication` and `measure_replication_at_scale` in `crates/graph-cli/tests/cluster_e2e.rs`; `scripts/cluster_soak.py` |

## Deployment
- [deploy/compose.md](deploy/compose.md): a three-node cluster with Docker Compose (`deploy/compose/cluster.yml`), and the CI check that runs it end to end.
- [deploy/kubernetes.md](deploy/kubernetes.md): a StatefulSet with a headless Service, node ids from the pod ordinal, gRPC readiness, a PodDisruptionBudget and the scale-down procedure (manifests in `deploy/kubernetes/`).
- [deploy/data-dir.md](deploy/data-dir.md): what a node's data directory holds, backup with `cluster snapshot --out` and restore with `serve --bootstrap --restore`.
- Logs, metrics and health probes: [guide/observability.md](guide/observability.md).

## Learnings
- [learnings.md](learnings.md): a short list of lasting facts, measured numbers, rules and review mistakes to avoid, each linking to the details.

The `spikes/` directory at the repository root holds spike code that is deliberately **not** built by the cargo workspace.
