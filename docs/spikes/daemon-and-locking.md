# Spike: daemon round trip, lock behaviour, copy cost

Status: evidence for the Q5 cross-process access decision in the [Q4/Q5 decision paper](q4-q5-decision-paper.md) (option a, an owning daemon; option b, retry/back-off as the no-daemon fallback; option d, snapshot copies). Date 2026-09-20. Spikes S1, S2 and S4 from the paper; S3 (fjall) was skipped, see the end.

**TL;DR (plain language).**
- **The daemon does not cost noticeable time. The 5 ms trigger did not trip.** Asking a daemon over a Unix socket instead of calling the store directly adds about 4 microseconds for an empty request and 0.1 to 0.4 ms for a normal 100-row answer (the large-answer rate of about 4 microseconds per KB is an estimate that holds only for big answers), and the size of the database does not change that. Measured directly on a 9.9 M-token database (not extrapolated): worst case 1.4 ms at the median for a 2,000-row answer, about 0.3 ms for typical 100-row answers. A compact binary format halves that again but is not needed. [M]
- **Today a second process is locked out completely, and for the whole life of the first process.** The database is locked from the moment a command opens it until the command exits. A warm search holds it for 6 to 85 ms (2 s for the first run on a cold 9.9 M file); an index run holds it for the entire run (20 s for 2.5 M tokens here; 77 s at 10 M is [M, from the data-model spike log], not re-run). During that time every other command, including read-only `describe` and `search`, fails at once with "database is locked by another process". [M]
- **Retry/back-off works for short holds but not for indexing, and it looks unfair under concurrency (inferred from the starvation seen, not tested against redb's lock call).** A retrying reader gets in within 5 to 250 ms of the lock being released (tunable), so retry is a fine answer to a 7 ms search hold. It cannot help against a 20 s to 77 s index run unless the wait is that long, and with 8 readers hammering the same file some of them waited 1 to 3 s for a 5 ms job while the median stayed at 5 ms (starvation). A daemon removes both problems: readers never touch the lock. [M]
- **Copying (option d) is cheap on this machine only because it is btrfs.** A reflink copy of a 135 MB file takes about 10 ms, 1 GB about 0.6 s and 6.4 GB about 1.6 s; a full copy is about 0.2 s per GB when it sits in RAM (only the 1 GB file shows this) and 3 s for the first cold GB; the 6.45 GB full copy was run once (4.4 s). Reflink is not available on ext4 (the paper's assumption for the target). [M on btrfs, E for other filesystems]
- **Conclusion: keep the daemon (a).** The latency evidence gives no reason to reconsider it, and the lock evidence is the argument for it. Two things surfaced that the daemon design must handle: `Store` is not `Send`/`Sync` today, and the error message does not tell the user how to fix the lock.

Raw logs and the throwaway code are in [spikes/daemon/](../../spikes/daemon/README.md). Nothing in `crates/` changed.

**Labels.** [M] measured on this machine, [E] estimated (method stated). Machine: Intel Core Ultra 9 275HX (24 threads), 32 GB RAM, NVMe, btrfs (`/var/home`), Linux 7.2, rustc 1.94.1, redb 2.6.3, `--release`. Other jobs were running; warm page cache unless stated. Code under test: the spike binaries were built against `origin/main` at the time (graph-store as of PR #8), but the current `main` is 5bedd03, which includes PR #12 (catalog, `schema_version` 2, in-place v1 to v2 upgrade on open). Numbers below were not re-run on 5bedd03 except the tokenizer repro in section 2.

## 1. S1: socket round trip versus in-process

### Method
- A throwaway server (`spikes/daemon/src/main.rs`) opens the real `graph_store::Store` and answers requests over a `UnixListener`; a client sends one request at a time and decodes the full response into typed rows. Frames are a 4-byte little-endian length plus payload.
- Two encodings, both carrying identical rows (grain, org, repo, file, language, symbol, kind, six span numbers, count): **JSON** (`serde_json`, typed both ways) and a **compact binary** hand-written codec (varints, length-prefixed strings; no extra crates). The rows are the same fields as `graph_store::Hit`/`SymbolHit`, converted the same way in both variants.
- Data sets: the corpus at 1x (241,638 tokens, indexed with `memory-graph index`), a 1 M-token and a 9.9 M-token database (the model-A files from the [data-model spike](data-model.md); their layout predates PR #12 and is not the current `main` layout, see the caveats). The 10 M set is 6.4 GB.
- Operations: `ping` (empty), rare token search (`Backtrace`, 22 rows), common token search (`self`, limit 100 and limit 2000), symbol-grain search (`self`, limit 100), `symbols` exact (`new`) and prefix (`f*`, limit 100), `describe`. 1,000 iterations per operation at 1x and 1 M, 100 at 10 M (15 for `describe`).
- **Overhead is computed against the same store**: the server records the time inside `handle()` (the store call plus row conversion, no socket, no codec) and the overhead is client round-trip p50 minus server `handle` p50. I first compared against a separate in-process copy of the file and got differences of ±4 ms at 10 M that were only noise between two physical copies (different page-cache state), so the table below uses the same-store method. The in-process column in the raw logs is still printed for reference.
- The server handles one connection at a time because `Store` is not `Send`/`Sync` (see section 4). Concurrent clients were not measured.

### Results (p50 milliseconds; overhead = round trip minus server handle)
9.9 M-token database, measured directly [M]:

| Operation | Rows | Store call (p50) | JSON round trip p50 / p95 | JSON overhead | Binary round trip p50 / p95 | Binary overhead | Payload JSON / binary |
|---|---|---|---|---|---|---|---|
| ping (empty) | 0 | 0.0001 | 0.0033 / 0.0037 | 0.003 | 0.0036 / 0.0038 | 0.004 | 6 B / 1 B |
| search, rare token | 22 | 0.043 | 0.059 / 0.071 | 0.016 | 0.061 / 0.068 | 0.008 | 3.9 KB / 1.7 KB |
| search, common token, limit 100 | 100 | 32.1 | 32.4 / 35.1 | 0.27 | 31.6 / 32.5 | 0.23 | 17.7 KB / 8.0 KB |
| search, symbol grain, limit 100 | 100 | 229.3 | 229.7 / 242.1 | 0.37 | 228.8 / 240.7 | 0.22 | 17.4 KB / 7.9 KB |
| search, common token, limit 2000 | 2000 | 28.0 to 30.9 | 29.4 / 31.2 | 1.4 (noisy, see below) | 31.0 / 37.7 | see below | 351 KB / 155 KB |
| symbols, exact `new` | 100 | 1.17 | 1.37 / 1.61 | 0.21 | 1.25 / 1.52 | 0.14 | 16.8 KB / 7.4 KB |
| symbols, prefix `f*`, limit 100 | 100 | 4.23 | 4.46 / 4.83 | 0.22 | 4.47 / 4.73 | 0.17 | 17.8 KB / 8.3 KB |
| describe | (1 repo set) | 1.40 | 1.80 / 1.92 | 0.40 | 1.43 / 1.57 | 0.06 | 164 KB / 142 KB (binary sends the JSON text) |

The 2,000-row line varies by run because the store call itself varies by about ±3 ms between the two server copies at 10 M; the cleaner measurement is the 1 M set below.

1 M-token database [M] (overhead p50, JSON / binary): rare search 0.016 / 0.008 ms; common search limit 100 0.12 / 0.07 ms; symbol grain limit 100 0.30 / 0.25 ms; common search limit 2000 (960 rows, 167 KB JSON / 73 KB binary) **0.69 / 0.35 ms**; symbols prefix 0.06 / 0.03 ms; describe about 0.03 / 0.02 ms. 1x corpus [M]: all overheads are 0.004 to 0.13 ms JSON, 0.004 to 0.065 ms binary (largest: 240 rows, 42 KB JSON).

Store call latency itself, for scale [M]: common-token search limit 100 is 0.47 ms at 1x, 1.8 ms at 1 M and about 30 ms at 9.9 M; symbol-grain search is 1.0 ms, 16 ms and 230 ms. The store, not the socket, is the cost.

### Decision trigger and extrapolation
- **Trigger: more than 5 ms p50 over in-process at 10 M tokens. Not tripped.** The largest measured p50 overhead at 9.9 M tokens is 0.4 ms for normal answers and about 1.4 ms for a 2,000-row (350 KB) answer in JSON. [M]
- **Why it will not trip at larger sizes [E].** The overhead depends on the size of the answer, not on the database: 0.003 ms fixed plus about 4 microseconds per KB of JSON (0.69 ms for 167 KB at 1 M; 1.4 ms for 351 KB at 10 M, consistent) and about 2 microseconds per KB of binary. That rate fits only large answers: small answers cost more per KB because of the fixed part (about 15 microseconds per KB for the 17.7 KB, 0.27 ms answer at 9.9 M). A 5 ms overhead therefore needs roughly a 1.2 MB JSON (about 3,400 rows) or 2.4 MB binary answer by the large-answer rate; the per-KB rate is an estimate, not a measured limit. With a `limit` (the CLI has `--limit`) the answer stays small at any database size, so 100 M tokens (the paper's scale-wide step) would show the same overhead; only the store call grows.
- **Unbounded answers are the risk, not the socket [E].** A `search` for a very common token without a limit returns rows in proportion to the corpus (at 10 M, hundreds of thousands of rows, tens of MB); that would cost tens to hundreds of ms of encoding. The paper's cursor/paging design already covers this; the protocol should default to a limit.
- **JSON versus binary.** Binary is about 55% smaller and halves the encoding overhead, but at every size measured the difference is under 0.4 ms. JSON is enough for the first protocol version; a binary encoding can be a later negotiated option. [M for the numbers, judgement for the recommendation]
- **What the daemon also saves.** A one-shot CLI call pays open and close of the database: 1.4 to 2.4 ms open and 3 to 7 ms close at every size here (`search-once` in `logs/s2_lock.txt`), plus a cold page cache for a file that was not touched recently (first search on a fresh copy: 1.8 s at 9.9 M versus 0.075 s warm). A resident daemon skips these, so for a repeated CLI call the daemon path is likely **faster** than opening the file, about 5 to 9 ms at 10 M before process start. [M for the components, E for the sum]

### Caveats
- One client, sequential requests, warm cache, same host, `std` blocking I/O. No concurrency, no TLS/auth, no MCP layer, no async runtime overhead (a `tokio`-based daemon adds a little). No daemon start-up or auto-spawn cost.
- The 1 M and 9.9 M files were produced by the data-model spike prototype and were **upgraded on first open**, 0.9 s at 1 M and 12.9 s at 9.9 M, all while holding the lock. That cost was measured against the code of the time; on current `main` (5bedd03, with PR #12) the same open also performs the in-place v1 to v2 upgrade (catalog, `schema_version` 2), so the 12.9 s may be the v1 upgrade plus the symbol rebuild, not the symbol rebuild alone; which part dominates was not separated. It is a one-time cost of old-format files, not counted in the tables, but it is an example of a long lock hold at open (section 2).
- Row conversion is in both paths; the store's own `Hit` is not serialisable in a round trip (`Hit` derives `Serialize` only), so a real protocol needs `Deserialize` on the result types.

## 2. S2: two-process behaviour today

### What a second process sees [M]
While one process holds the file, a second `Store::open` fails immediately, not after waiting:
- Library: `StoreError::Locked(path)` after 0.025 ms; display text `database is locked by another process: <path>`.
- CLI `search` on a locked file: exit code 1 after 2.7 ms, message `Error: opening database ... Caused by: database is locked by another process: <path>`. `index-file` prints the same line (exit 1, 1.5 ms).
- The message does not say who holds it, for how long, or what to do. The paper's (b) wanted "a message naming `serve`"; that is not there yet.
- Reads are blocked too. redb 2 has no read-only open, so even `describe` or `search` cannot run next to any other process, not just next to a writer.

### How long the lock is held [M]
Source: section 1 of `logs/s2_lock.txt` (5 runs per size; run 1 is the first run on a fresh copy, runs 2 to 5 are warm) unless another source is named. The lock is taken in `Store::open` and released when the `Store` is dropped, so it is held for the whole life of the command.

| Command | Lock held |
|---|---|
| `search` (limit 100), corpus 1x | 5.8 to 7.4 ms warm (open 1.3 to 2.4, search 1.8 to 2.0, close 2.7 to 3.1); 57.5 ms first run |
| same, 1 M-token file | 11.8 to 12.5 ms warm; 186 ms first run on a fresh copy |
| same, 9.9 M-token file | 79 to 85 ms warm (search 71 to 72); 2.0 s first run on a fresh copy (cold cache) |
| `index`, the 8-repo corpus (241 k tokens) | 8 processes, 1.76 s wall in total (fresh), 3.07 s with `--reindex` (from `logs/s2_index_1x.txt`, not re-checked here) |
| `index`, 2,150 files / 2.55 M tokens / 46 k symbols (one crate set from the Cargo registry) | **19.7 s** (`logs/s2_lock_index_rerun.txt`, section 3), one process (about 130 k tokens/s, in line with the 129 k tokens/s of the data-model spike) |
| `index` at 9.9 M tokens | 76.8 s [M, from the data-model spike ingest log; not re-run] |
| first open of a legacy 9.9 M file | 12.9 s (one-time; symbol index rebuild and, on current `main`, possibly also the v1 to v2 upgrade; not separated; not in the committed logs) |

The 19.7 s run is 2.55 M tokens (about 130 k tokens/s); "about 77 s at 10 M" is the measured figure from the data-model spike, not this spike. Extrapolating the index hold to 100 M tokens at 130 k tokens/s: about 13 minutes per full run [E], less for incremental runs (unchanged files are skipped by fingerprint, which stops being a per-file write).

### Retry with back-off [M]
Method: a holder process opens the store and sleeps for D seconds; a second process retries `Store::open` on `Locked` and we record when it succeeds relative to the holder's release (overshoot). Full jitter (sleep a random amount up to the current delay, doubling to a cap) versus a fixed poll:

| Holder | Policy | Attempts | Overshoot after release (median; range) |
|---|---|---|---|
| 0.25 s | fixed 100 ms | 4 | 107 ms; 105 to 108 |
| 0.25 s | jitter 5 ms doubling to 250 ms | 8 | 86 ms; 15 to 183 |
| 0.25 s | jitter 20 ms doubling to 1 s | 6 | 197 ms; 18 to 576 |
| 1 s | fixed 100 ms | 11 | 53 ms; 52 to 54 |
| 1 s | jitter 5 to 250 ms | 14 to 16 | 93 ms; 32 to 221 |
| 1 s | jitter 20 ms to 1 s | 8 to 10 | 413 ms; 20 to 701 |
| 5 s | fixed 100 ms | 51 | 58 ms; 56 to 59 |
| 5 s | jitter 5 to 250 ms | 41 to 51 | 101 ms; 41 to 203 |
| 5 s | jitter 20 ms to 1 s | 14 to 17 | 428 ms; 28 to 757 |
| 30 s | fixed 100 ms | 301 | 97 ms |
| 30 s | jitter 5 to 250 ms | 232 to 241 | 132 ms; 40 to 223 |
| 30 s | jitter 20 ms to 1 s | 63 to 70 | 160 ms; 86 to 233 |

- Every policy succeeds; the worst wait after the release is roughly the cap (about 250 ms for a 250 ms cap), and the cost is attempts, each about 20 microseconds (a failed open). Retry adds nothing measurable to a 7 ms search hold.
- **Against a real index run it works only if the budget is longer than the run.** A reader started 1 s into the 19.7 s index (rerun log; jitter 5 to 250 ms) made 159 attempts and got in 5.3 ms after the indexer exited [M]. With the paper's 5 s default budget it would have failed with `Locked`. A 2.5 M-token index already exceeds it by 4x, and a 10 M-token index by 15x [E from the 77 s hold].

### Contention among short readers [M]
8 reader processes each doing 100 rounds of open, search (limit 100), close on the 1x file, retrying with jitter (2 ms doubling to 100 ms), no writer:

- Median 4.7 to 4.9 ms and p95 5.6 to 6.6 ms per round for every worker (a round is about 4 ms of actual work), but the worst rounds were 0.47 to 3.05 s, with up to 71 attempts. The lock is probably not fair (inferred from the starvation seen here; not tested against redb's lock call itself), and a jittered retry can starve a process for seconds against 7 ms holders.
- The same 8 readers while an indexer was running (`--reindex` of the 2.55 M-token set into a copy): every worker's worst round was 22 to 31 s, p95 3.1 to 8.9 s, up to 626 attempts. Total wall was 47.3 s (rerun log) against 19.7 s for an indexer alone; the first run, with an 8.5 s indexer, gave 15.1 s total wall and worst rounds of 7.8 to 11.1 s (the indexer's own timing was not captured in this run; the extra time is probably the readers serialising on the lock after the indexer released it, and their CPU use, but that was not isolated).
- A retry loop with a jitter cap therefore appears to hide a lock that is a queue with no fairness (inferred, as above). For a single agent plus a human this is fine; for several agents plus MCP clients it is not.

### A finding on the side
Indexing a set of Cargo-registry crates failed in five of the crates (`bstr`, `cargo-deny`, `cargo_metadata`, `cargo-zigbuild`, `codespan-reporting`) with `invalid span: bytes ... partially overlap an enclosing symbol ...`. The cause is a **tokenizer bug in `graph-core`** (`crates/graph-core/src/tokenizer.rs`), not the Rust extractor: the tokenizer treats every `"` as a plain string delimiter and has no raw-string handling (`r#"..."#`, `br#"..."#`). Minimal repro, re-run on `main` 5bedd03:

```
$ printf 'const A: &str = r#"a"b"#;\n' > a.rs
$ memory-graph --db g.redb index-file --org o --repo r a.rs
Error: invalid span: bytes 22..26 partially overlap an enclosing symbol ending at 25
```

The symbol span from `syn` is right (it ends at 25); the tokenizer ends the string token early at the inner `"` and produces a token that straddles the symbol boundary. The store's rejection is valid. The remaining problem is that one bad file aborts the whole batch (nothing from that batch is stored), which is what aborted the first long-index attempt; the crates were removed from the test input. Both the tokenizer fix and per-file error isolation are outside this spike's scope. Logs: `logs/s2_lock.txt` (first run, section 3 shows the failure as an early exit at 8.5 s) and `logs/s2_lock_index_rerun.txt`.

## 3. S4: reflink and copy time [M, btrfs]
Filesystem: btrfs on NVMe (`/var/home`), so `cp --reflink=auto` clones. Files: a real 135 MB redb (left over from earlier spike work), a real 1.08 GB redb (model E of the data-model spike), a real 6.45 GB redb (model A, 9.9 M tokens) and a synthetic 1 GiB file of random bytes. 3 runs each; `cp` wall time; page cache warm (cold-cache copies were not measured because dropping caches needs root).

| File | `cp --reflink=auto` / `always` | `cp --reflink=never` (full copy) |
|---|---|---|
| 135 MB redb (2,092 extents) | 10 to 17 ms | 21 to 23 ms warm (321 ms first run) |
| 1.08 GB redb (140,826 extents) | 0.55 to 0.84 s | 0.2 s warm (3.1 s first run); then `sync` 0.45 s |
| 6.45 GB redb (362,395 extents) | 1.59 to 1.99 s | 4.4 s |
| synthetic 1 GiB, one extent | **1.2 to 1.8 ms** | 0.21 to 0.22 s, plus 0.45 s to flush to disk (`sync`) |

- The reflink cost is not O(bytes) but O(extents): 1 GiB in one extent clones in 1.5 ms, while the 1.08 GB redb (heavily fragmented by copy-on-write from many small commits) needs 0.6 s, and the 6.45 GB one 1.6 s. A redb that has been written for a long time on btrfs will be fragmented like these, so a checkpoint by reflink is about 0.5 s per GB in the realistic case, and the writer must be quiesced for that time [E: the spike did not stop a writer].
- A full copy ran at about 5 GB/s from the page cache in the 1 GiB synthetic file (the only file behind the "GB/s from RAM" figure) and about 0.3 to 1 GB/s when the source was cold or the destination had to be flushed; the 6.45 GB full copy was run once (4.4 s). Disk cost doubles unless reflinked, and reflinked copies stay shared only until the writer rewrites those pages.
- ext4 has no reflink (it needs XFS, btrfs or bcachefs); there, option (d) is a full copy: about 0.2 to 0.5 s per GB warm, 3 s or more for a cold GB [E], every checkpoint, under the writer lock. For the paper's 20 GB shards that is tens of seconds of a paused writer per shard.

## 4. What this means for the decision

1. **Option (a), the daemon, is confirmed on latency.** Overhead over in-process is well under 1 ms for normal answers and 0.003 ms for an empty request, at 1x, 1 M and 9.9 M tokens. The trigger (more than 5 ms p50 at 10 M) did not trip, and the reason (overhead follows the size of the answer, not the database) means it will not trip at 100 M either as long as answers are limited. Length-prefixed JSON is sufficient for protocol v1.
2. **The lock evidence is the case for it.** Today any second process, even a read, is refused for the whole life of the first; short holds (6 to 85 ms warm) are fine with retry, but index holds (20 s per 2.5 M tokens; about 77 s per 10 M [M, from the data-model spike log]) are not, and retry among several readers starves. The daemon holds the lock once and serves all readers from snapshots.
3. **Option (b) as the no-daemon fallback is acceptable only for the single-user case.** The 5 s budget in the paper covers searches; it will not cover a long index. The error message should name `serve` and, if possible, the holder (a lock file with pid and command would need a sidecar because redb's lock has no owner information).
4. **Option (d) is viable on btrfs only.** About 0.5 s per GB in the realistic fragmented case on this machine; on ext4 a full copy and a paused writer make it a poor default. It stays a fallback for read-only clients on a checkpoint.
5. **Design items for the daemon story that the spike found:**
   - `graph_store::Store` is not `Send`/`Sync` (`Registry` holds `Box<dyn Extractor>`), so a threaded server needs the trait objects to be `Send + Sync` or a dedicated store thread. This is a change in `graph-store`, not just a new crate.
   - Result types (`Hit`, `SymbolHit`, `RepoInfo`) are `Serialize` only; the protocol needs `Deserialize`.
   - Default `limit` on every request; unbounded answers are the only way the overhead becomes visible.
   - `describe` at 9.9 M tokens returns 164 KB of JSON and takes 1.4 ms in the store here (this store has the catalog); it is fine over the socket.
   - Opening a legacy file does one-time work while holding the lock (12.9 s at 9.9 M: the symbol index rebuild and, on current `main`, possibly the v1 to v2 upgrade): the daemon must do this at start-up (the ADR's `repair on open`), before it accepts clients, and clients need a "starting" state rather than a timeout.
   - Cold-cache first queries (1.8 s at 9.9 M versus 75 ms warm) are the daemon's main advantage over one-shot CLI calls; consider a warm-up on start.

## 5. What was not done
- **S3 (fjall) skipped.** `fjall` is not in the offline crate cache, so it was not trivially available; not attempted.
- Concurrent clients against the daemon, an async runtime, Windows named pipes, auth and protocol versioning were not measured.
- Cold-cache copy times, an ext4 measurement and a writer paused during a checkpoint were not measured.
- The 10 M search numbers use 100 iterations per operation (versus 1,000 at 1x and 1 M) because a single symbol-grain search takes 0.23 s.
- 9.9 M is measured, not 10.0 M; 100 M is extrapolated only for the overhead model above.

## Reproduce
See [spikes/daemon/README.md](../../spikes/daemon/README.md).
