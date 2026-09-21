> **Status: decided by the user 2026-09-20 (decision record). This paper is not an ADR.** Chosen: **Q5 = option (a), an owning daemon** (`memory-graph serve`, also the MCP server; the CLI talks over a versioned local socket through a `RemoteStore` implementing `Store`; with no daemon the CLI opens the file directly and retries with jittered back-off, default 5 s, with a message pointing at `serve`). **Q4 = option 1 with the build deferred**: shard by `(org, repo)`, one redb file per shard, split at a size threshold; the key and the id layout `tag | shard(10) | local(53)` are fixed now, and sharding stories 14-17 wait for the measured 100 M run (story 6) and the story 13 spike. The decisions are recorded in [ADR 0003](../adr/0003-data-model.md), which itself is still Proposed (not accepted). Not stated by the user yet: the Windows position, the wire encoding, and approval of spikes S1-S3. The text below is the original recommendation, kept as the evidence; its "awaiting decision" wording is superseded by this header. Written against `main` at 4ed7f41.

# Decision paper: ADR 0003 Q5 (cross-process access) and Q4 (shard granularity)

Status of the original review: advisory, read-only review of `docs/adr/0003-data-model.md` on main.

## Verified facts
- redb 2.6.3 `file_backend/unix.rs` L37: `flock(fd, LOCK_EX | LOCK_NB)`; `EWOULDBLOCK` maps to `DatabaseAlreadyOpen`. There is no shared/read mode and no wait. A read-only open still takes `LOCK_EX`, so a second process cannot read while a first holds the file. The lock is per open file description, so even a second `Database::open` in the same process fails.
- `graph-store` already maps this to `StoreError::Locked`. The CLI has no `serve` or MCP crate yet (crates: graph-cli, graph-core, graph-lang-rust, graph-store). So a daemon is new work, not a change to existing code.
- redb writes pages in place with a two-phase commit. A byte copy of a live file is not safe unless the writer is quiesced. I found no online backup or export API in 2.6.3 (savepoints are in-file, not file-level). `compact()` needs `&mut Database`, i.e. exclusive.
- A **hardlink is not a snapshot**: it shares the inode, the data and the flock. Only a copy or a reflink (btrfs/XFS, not ext4) gives a separate inode.
- redb's public `StorageBackend` trait allows a custom backend that skips flock. That would let a second process read the live file, but nothing makes those reads consistent (in-place page reuse, no cross-process txn ordering). **Rejected: unsafe, silently corrupt reads.**
- fjall is not in the local cargo registry, so nothing about it is verified here. Everything said about fjall below is from general knowledge and must be spiked.

## Requirements used as criteria
D1 >100M tokens and scale wide; D2 migration before 1.0; D3 point-in-time snapshots; D4 spikes in repo; pure Rust; language-agnostic; MCP (epic story 17) and agents are primary consumers; CLI ergonomics: `index` in one terminal while `search` runs in another.

---
## Q5: cross-process access

### Options
| # | Option | Meets D3 across processes | CLI ergonomics | Cost (days) | Risk |
|---|---|---|---|---|---|
| a | Owning daemon (embedded in `memory-graph serve`, also the MCP server); CLI and agents are clients over a local Unix socket | Yes, all reads use in-process redb snapshots | Good if the CLI auto-spawns or auto-detects the daemon; a fallback is needed | 6-9 | Medium |
| b | CLI retry/back-off on `Locked` | No. Availability only between writes | Bad while indexing: a large index run holds the file for minutes, so `search` times out | 1-2 | Low, but does not solve it |
| c | Per-shard files, writers on other shards | Only for shards not being written; the repo being indexed is unreadable | Poor at the default of one shard | in Q4 | Does not solve the default case |
| d | Snapshot-file replicas: the writer (or `checkpoint` command) produces a reflink or copy of each shard at a quiesced point and publishes it with a manifest; readers open the copy | Yes, but staleness is by design (reads lag the last checkpoint) | Good; needs no daemon; `search` sees the last checkpoint | 5-7 (plus copy I/O) | Medium-high: copy cost O(db size) on ext4, 20 GB per shard, disk 2x, and the copy must be taken under the writer lock so the writer pauses |
| e | Writer-side handoff: `index` acquires the lock only for short per-chunk commits and releases it between chunks (open/close per chunk), readers retry | No snapshot across chunks; readers block only at commit instants | Acceptable for small runs; reopen cost per chunk (cache warm-up) hurts throughput | 3-4 | Medium: livelock and fairness (no queue on `LOCK_NB`), needs an advisory lockfile with a waiter queue |
| f | Custom `StorageBackend` without flock | No | n/a | 3 | Unsafe, rejected |
| g | Switch engine (e.g. fjall, an LSM) for multi-process | Unknown; LSM engines are typically also single-process (fjall's own lock), so no gain is expected | n/a | 15+ and reopens ADR 0001/0003 | High. Pure-Rust status must be re-checked (gate keys on `links`/C build scripts) |

Notes the ADR missed: (d) and (e) above, plus the fact that (g) is unlikely to help: LSM embedded stores generally hold a process lock too. This is unverified until the spike below.

### Interaction with the story table
Story 10 (snapshots, single shard, 3 d) is in-process by design, so (a) leaves it unchanged. (a) adds a new story: "serve + client protocol", about 6-9 d, which is not in the current 45-47 d core. (a) also unblocks `verify` (the ADR notes it must go through the owner) and `vacuum`, and it makes `repair` a startup step of the owner.

### Recommendation Q5: (a) daemon, with (b) as the interim
Build one owning process, embedded in `memory-graph serve`, which is also the MCP server (story 17). The CLI works in two modes:
1. Daemon reachable on the per-index socket (`<db>.sock`, or a path in the db directory): send the request (search, symbols, describe, index) as length-prefixed JSON or MessagePack. Snapshots are held in the daemon; multi-call paging uses a cursor handle with a server-side snapshot id and the Q6 max-age.
2. No daemon: open the file directly (today's behaviour), with (b) retry/back-off (default 5 s, jittered) and a message naming `serve` as the fix. Single-user, single-terminal use stays zero-config.

Why: it is the only option that satisfies D3 across processes without staleness or copy cost, it is the topology MCP requires anyway (an MCP server is long-lived and owns the store), and it matches the ADR's own hints (`repair` on daemon open, `verify` via daemon). Pure Rust: a std `UnixListener` (or `interprocess`/`tokio`; `tokio` is already likely once MCP exists, check the gate) has no C deps.

Costs and risks:
- CLI must not become a second implementation of the query API: the client should call the same `Store` trait through a remote adapter (`RemoteStore: Store`). That keeps the differential oracle valid.
- Latency: one local socket round trip is expected in the 0.05-0.3 ms range, small next to a search over 10 M+ tokens; confirm in the spike.
- Windows: named pipes are needed for parity (redb also has a Windows backend); the project is currently Linux-first, so a decision is needed on Windows support.
- Two daemons on one db is prevented by the redb lock itself (good).

**Irreversible:** the wire protocol becomes a public surface once agents depend on it (version it from day one, a `protocol_version` in the handshake); and "CLI talks to the owner" becomes the operational model in the docs. The redb file format is not affected.

**What would change my mind:**
- Spike A (1 d): measure socket round trip and `search` overhead vs in-process at 10 M tokens; if p50 overhead is more than 5 ms, reconsider.
- Spike B (1 d): fjall/other engine multi-process behaviour. If an LSM engine supports genuinely concurrent readers across processes with a writer (verified, not assumed), and its pure-Rust status and 100 M size are confirmed, (g) becomes worth an ADR. Otherwise dead.
- If users are only ever single-terminal and MCP is dropped, (b) alone is enough and (a) can be deferred, but story 17 makes that unlikely.

---
## Q4: shard granularity and partition key

### Options
| # | Option | D1 scale | Ops complexity | Cost (days) | Risk |
|---|---|---|---|---|---|
| 1 | ADR default: shard = one redb file, key `(org, repo)`, one shard until a size threshold (e.g. 200 M tokens or 20 GB), then split | Good; repo never spans shards | Low until the first split; split is a new, tested operation (story 16) | 27 (stories 13-17, as in the ADR) | Medium: a single very large repo (monorepo above threshold) cannot be split |
| 2 | Shard per repo (one file per repo) | Good for many repos; ok per-repo | Many files and open handles (1,024 cap in the id layout); every query is a fan-out; manifest churn | ~25 | Medium: fan-out cost and O(repos) read txns per snapshot (ADR: a snapshot holds a txn on every shard) |
| 3 | Hash of `(org, repo)` into N fixed shards (N set at creation) | Predictable | Resharding = full rewrite; no locality (org queries always fan out) | ~22 | High: N fixed is hard to change |
| 4 | Shard by content/language | Poor: dictionary and postings are per-file and repos would span shards, breaking ADR invariants (roll-up, atomic file replace) | High | n/a | Rejected |
| 5 | Defer: ship single shard (stories 0-12) and build sharding only when a measured limit is hit | Only if 100M fits one file; D1 asks for scale wide, but not necessarily now | Lowest | 0 now, 27 later | Medium: the id layout and store trait must carry the shard bits from day one (already the plan), so deferral costs little |
| 6 | Intra-repo split (by directory) for monorepos | Handles the monorepo case | Very high: breaks "repo never spans shards" | 10+ | High; not needed for D1 |

### Recommendation Q4: option 1, with build deferred (option 5 timing)
Keep the ADR default: unit = one redb file, partition key `(org, repo)`, default one shard, split at a configured size. Do not build stories 14-17 until story 13 (2 d spike, real distinct-text ratios) and a measured 100 M run (story 6) show the single-file ceiling. Fix now, because they are hard to change later: the id layout (`tag | shard(10) | local(53)`, format byte allows widening) and the store trait's shard-agnostic API.

Rejected: option 2 as the default (snapshot cost scales with the number of shards, and many tiny repos are the common case), 3 (fixed N), 4, 6.

**Irreversible:** the partition key `(org, repo)` and the id layout are baked into persisted token ids and into agents holding ids (Q1 says ids are unstable across re-index, which limits the blast radius). Choose the key once; the split threshold and shard count ceiling (Q7) are tunable.

**What would change my mind:**
- Story 13 shows a single repo above the threshold in the target corpus (e.g. a Linux-kernel-sized repo at 200 M+ tokens on its own): then option 6 or a raised threshold is needed.
- The measured 100 M run fits one file at acceptable size and latency: then defer 14-17 entirely (saves 25 d) and revisit at 300 M.
- The 100 M run shows that per-file lock contention (one writer per shard) throttles ingest: then more, smaller shards (option 2 for big orgs) become attractive.

---
## Q4 / Q5 interactions
1. **Daemon owns shards.** With (a), the daemon holds every shard's exclusive lock, so the manifest snapshot protocol (eager read txn per shard, epoch validation, bounded retry) runs inside one process. The cross-process "commit-then-publish" window is then only an intra-process race, and crash recovery is the daemon's startup `repair`. This makes the ADR's story 14 simpler (fewer retries and no `SnapshotUnavailable` for live writers). Without a daemon, story 14 is what makes cross-process reads unsafe.
2. **Option (c) needs Q4.** Per-shard files only help if multiple shards exist; at the default of one shard it gives nothing. Q5 must not be answered with (c).
3. **Manifest location.** Put the manifest beside the shard files (same directory, `manifest.json` with atomic rename), owned and written only by the daemon. CLI in fallback mode (no daemon) must read it read-only. With daemon mode, clients never touch it. It is not in the repo (D4 concerns spikes, not runtime data).
4. **Replicas (d) and sharding multiply cost:** a copy-based checkpoint copies all shards; with 20 GB shards, that is the reason (d) is not recommended.
5. **Routing across processes:** if a future scale-wide step means shards in multiple processes (a shard per daemon), Q5's protocol would need to become a shard-to-shard protocol. The epic lists distributed storage as out of scope, so I recommend the protocol be per-database, not per-shard, and revisited only then.

## Suggested sequencing and spikes (all under `spikes/`, per D4)
| Spike | Days | Question answered |
|---|---|---|
| S1: socket round-trip and search overhead vs in-process | 1 | Is (a) acceptable latency-wise? |
| S2: two-process test with today's `graph-store` (index in one, `search` in another) confirming `Locked` at any point and measuring how long `index` holds the lock | 0.5 | How bad is (b) today? |
| S3: fjall (or other pure-Rust engine) multi-process behaviour and a 10 M size check | 1-2 | Is (g) worth an ADR? |
| S4: reflink availability and copy time of a 2 GB redb file on the target filesystem | 0.5 | Is (d) viable for anyone? |
| S5 = ADR story 13 | 2 | Q4/Q7 evidence |

Total new spike effort: about 3-4 d beyond story 13. Net delta to the ADR estimate: Q5(a) adds about 6-9 d (core becomes about 52-56 d); Q4 as recommended adds nothing until story 13's evidence arrives.

## Decisions requested of the human architects

**Decided 2026-09-20: Q4 shard by (org, repo) with build deferred; Q5 daemon. Still open: Windows/named pipes, wire encoding, spike approval.**

1. Q5: approve (a) daemon in `memory-graph serve` with (b) as the no-daemon fallback; approve a versioned local-socket protocol; state Windows position.
2. Q4: approve `(org, repo)` partition key and the id layout; agree to defer stories 14-17 until story 6 + 13 evidence.
3. Approve spikes S1-S3 before story 10 is scheduled.
