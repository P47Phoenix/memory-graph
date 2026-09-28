# Measurement: Raft replication cost (log size, fsync throughput, ingest, snapshots)

Status: stage B evidence (epic story 21, [ADR 0004](../adr/0004-client-server-and-replication.md) D5-D7 and its revisit trigger "replicated ingest below 50% of embedded"). Date 2026-09-28. Not a decision.

**TL;DR (plain language).**
- **Replicated ingest runs at 94% of embedded speed, on one node and on three loopback nodes alike. The ADR trigger (below 50%) is not tripped.** [M]
- **A log entry is its payload plus 25 bytes of framing, and the payload is about the source itself (1.02x).** The corpus is 19 write entries (the client cuts batches at 8 MiB); framing is 0.03% of the log. [M]
- **`raft.redb` is bigger than what it holds before a snapshot: 4.06x the source after one corpus pass, 2.18x after three.** redb rounds large values up to page-order allocations, and an empty redb file is already 1.59 MB. [M]
- **After a snapshot purges the log, the file used to stay at its high-water mark (redb reuses freed pages but never returns them). The log store now compacts after a purge: 11.0 MB went down to 2.66 MB (0.53x of the 5.05 MB indexed), and that is the size gate.** [M]
- **fsync-bound throughput: 664 small entries/s and 108 MB/s of 1 MiB entries on one node; 392 entries/s and 65 MB/s on three nodes.** Three nodes add a network round trip and a follower fsync per entry, and cost 40% of small-entry throughput. Batched ingest does not notice, because the entries are 8 MiB. [M]
- **A snapshot of the 17 MB corpus store builds in 0.15 s. A new learner receives and installs it and is serving in 0.17 s.** [M]

**Labels.** [M] measured on this machine, [E] estimated (method stated). Machine: AMD Ryzen 9 7950X (16 cores, 32 threads), 63 GB RAM, NVMe SSDs (Crucial T700, WD SN850X), Windows 11 Pro (10.0.26200), rustc 1.98.1, `--release`, openraft 0.9, tonic 0.14, redb 2.6.3. Other programs were running; warm page cache. Every node is a separate `memory-graph serve --data-dir` process on `127.0.0.1` (loopback, no real network), with its data directory on the same disk.

## Method

- Harness: the ignored test `measure_replication` in `crates/graph-cli/tests/cluster_e2e.rs`, driving the real release binary exactly as a user would:

  ```sh
  cargo test --release -p graph-cli --test cluster_e2e measure_replication -- --ignored --nocapture
  ```

- Data: the vendored corpus (`testdata/corpus`, nine repos, 1,684,464 source bytes). Each repo is indexed as one `memory-graph index --org corpus --repo <name>` run, a new process each time, so every timing includes nine process starts.
- **Embedded ingest:** `--db <new file>`, best of 3.
- **Replicated ingest:** the same nine runs with `--server <leader>`. One node: `serve --data-dir --bootstrap`. Three nodes: node 1 `--bootstrap`, nodes 2 and 3 `--wait-for-membership`, joined with `cluster add-learner` / `cluster promote` before the clock starts. The clock stops when the leader has acknowledged the last write, which means a majority has fsynced it.
- **Log contents:** after a one-node ingest, the node is stopped and `raft.redb` is read directly (the `raft_log` table): entries, encoded bytes, and 25 bytes of framing per entry (`kind | term | node | index`, see `raft/log_store.rs`).
- **fsync throughput:** `RemoteStore::index_bytes` in a loop, one call = one log entry (a `Write.IndexFile` proposal). 300 calls with a 1 KiB single-token file, then 20 calls with a 1 MiB single-token file. Single tokens keep the store's per-token work small, so the loop measures the log (proposal, replication, fsync, apply) rather than the tokenizer.
- **Snapshot build:** `cluster snapshot --json` against the one-node server after the corpus ingest (built on the server; no download).
- **Snapshot install:** with `--log-keep-entries 0`, the snapshot above purges the whole log. A fresh `--wait-for-membership` node is then added with `cluster add-learner`. It can only catch up by an `InstallSnapshot`. The time runs from the add until its `applied_index` (from `cluster status`) reaches the leader's. (Measured in stage B. Since stage C, `--wait-for-membership` is retired and `measure_replication` starts the node with `--join <leader> --standby` instead, timing from the start of the joining process.)
- **Size gate:** `raft_log_after_snapshot_and_purge_stays_within_bounds_of_source` in `crates/graph-cli/tests/size_gate.rs` (debug build; the sizes do not depend on the build).

## Results

### Ingest [M]

| run | seconds | share of embedded speed |
|---|---:|---:|
| embedded (`--db`, best of 3) | 1.319 | 100% |
| one node (`--data-dir --bootstrap`, `--server`) | 1.399 | 94% |
| three nodes, via the leader | 1.398 | 94% |

The server parses and commits on the leader as the embedded store would. The 6% gap is the client reading and sending files, plus the log entry's own fsync. Three nodes cost no more than one here because an ingest is 19 entries, so a follower's round trip and fsync per entry amount to milliseconds in total. [M]

### The log [M]

| quantity | value |
|---|---:|
| entries after one corpus pass | 21 (19 writes, a blank entry and the bootstrap membership) |
| encoded bytes | 1,720,142 |
| framing | 525 B (25 B/entry) |
| payload | 1,719,617 B (1.02x source) |
| `raft.redb` after one pass (1 pass) | 6,836,224 B (4.06x source) |
| `raft.redb` after three passes (3 passes, size gate) | 11,030,528 B (2.18x of 5,053,392 B) |
| after `cluster snapshot` + purge, before compaction (3 passes) | 11,030,528 B (unchanged: 2.18x) |
| after `cluster snapshot` + purge + compaction (3 passes) | 2,658,304 B (0.53x) |
| empty `raft.redb` (just created) | 1,589,248 B |

The payload is the prost `LogCommand`: the file bytes, their path, language and fingerprint. It is replicated as bytes and never as JSON (D5). The file overhead before a snapshot is redb's rather than the log's. Each ~90 KB entry takes a power-of-two run of pages, and a small file is dominated by its 1.6 MB floor.

**Compaction after a purge (added in this change).** Without it, a snapshot purges the log's rows but the file keeps its size. redb reuses the pages for later entries but never truncates. So between two snapshots the file held the largest log it ever had: up to `--snapshot-log-bytes` (default 1 GiB) plus the kept entries. `RedbLogStore::purge` now calls `compact_if_sparse`, which works in four steps:

1. It releases the purge's pages with two empty commits. redb frees pages only on a later commit, and `compact` does the same itself.
2. It reads the live size from redb's own stats (`allocated_pages x page_size`).
3. It runs `Database::compact` if the file is at least twice that size plus 1 MiB, and has grown by 1 MiB since the last compaction.
4. It takes the write side of a lock that every log transaction holds the read side of, so no transaction of ours is open, as `compact` requires.

We chose redb's `compact` over rebuilding the file, for these reasons:

- **Durability is intact.** `compact` commits with two-phase commits, so an interrupted compaction leaves a valid file with the purge already done. Rebuilding would need a second file, a rename that has to be atomic, and a directory fsync on every platform. That is more code, and more crash windows for the vote and the purge point, which live in the same file.
- **Its cost is bounded.** The pause is bounded by what the purge left: `--log-keep-entries` entries, default 1000. Appends wait for it, and it runs once per snapshot. Here it takes milliseconds.
- **Rebuilding cannot do better.** A fresh redb file is already 1.59 MB, and the compacted file ends near that floor plus one growth region.

This also found a policy bug: `purge_batch_size` was 64, so openraft purged only once 64 entries could go. A whole corpus index is 19 entries of up to 8 MiB, so after a snapshot nothing was purged at all. Up to 64 x 8 MiB of log could stay. It is now 1. A purge is one range delete plus, at most, one compaction.

**Why the gate uses three passes.** The spec's gate is `raft.redb <= 1.5x source` after an index, a snapshot and a purge. With one corpus pass (1.68 MB) no redb file can meet it: an empty one is 1.59 MB (0.94x), and the compacted log measured 1.58x, made of that floor plus one region redb grows into on the next write. Three passes index 5.05 MB, so the ratio measures the log rather than redb's constant, and the gate passes at 0.53x. On a real tree (hundreds of MB) the floor is negligible. [M]

### fsync throughput [M]

| cluster | 1 KiB entries/s | 1 MiB entries/s | 1 MiB MB/s |
|---|---:|---:|---:|
| one node | 664 | 103.1 | 108.1 |
| three nodes (loopback) | 392 | 62.3 | 65.3 |

Every entry costs, in order: the leader's log fsync, a follower's log fsync (three nodes: the first of two followers), and the leader's apply transaction with its own fsync, before the acknowledgement. The small-entry rate is fsync latency: about 1.5 ms per entry on one node and 2.6 ms on three. The client sends one entry per call, so nothing is pipelined. `index` batches files into 8 MiB entries, so an ingest pays this per 8 MiB, not per file. [M]

A real network adds its round trip per entry. At 0.5 ms it would cost three nodes about 15% more on small entries [E], and nothing measurable on 8 MiB ingest batches [E].

### Snapshots [M]

| operation | time | size |
|---|---:|---:|
| build (`cluster snapshot`, corpus store) | 0.148 s | 17,379,328 B |
| transfer + install on a new learner, until it applied the leader's index | 0.167 s | same |

The build is `V2Store::export_snapshot` from one read transaction, plus the SHA-256 and the `.meta`. The install streams 1 MiB chunks over the `Raft` service, verifies the SHA-256 and size, and swaps the store file under the slot's write lock. Reads on the learner answer `UNAVAILABLE` during that window (D8). `cluster add-learner` returned after 0.021 s, before the install finished: openraft's blocking add waits for the change and the replication stream, not for a snapshot install. The CLI's help text says so, and the e2e test waits on `applied_index` instead. [M]

## Verdict

- ADR 0004 revisit trigger ("replicated ingest below 50% of embedded"): **not tripped**. The measurement is 94% on one node and on three loopback nodes. [M]
- The log does not accumulate on disk after snapshots, now that a purge compacts. The size gate pins it at <= 1.5x the indexed source (measured 0.53x). [M]
- Not measured here: a real network, a large tree (the corpus is 1.7 MB), and a follower on a slower disk than the leader. The small-entry numbers are the floor that a real network adds to. [E]
