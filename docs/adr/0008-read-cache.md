# ADR 0008: Read cache pool

**Status:** Accepted by the owner on 2026-10-05 (proposed and revised after dev and QA review the same day). Phases 0 and 1 are delivered (#230, #235). After phase 1 the decode share is below the 25% gate on the vendored corpus, so phase 2 (stories 47-48) is not started; it stays gated on the large-index re-run tracked in #233. Builds on [ADR 0003](0003-data-model.md) (the v2 store) and [ADR 0004](0004-client-server-and-replication.md) (`serve`, Raft). Epic amendment: stories 45-49 in the [epic](../epic-code-memory-graph.md), added with the owner's approval.

## In plain words

1. Today the only thing a query reuses from the last query is redb's page cache. Everything decoded from those pages (dictionary blocks, symbol sections, org/repo/file nodes) is thrown away at the end of each call and decoded again by the next.
2. This ADR first measures how much time that re-decoding costs (phase 0), then takes two cheap wins (phase 1), and only if decode passes an exact threshold adds an in-process cache of decoded objects (phase 2).
3. The cache must never return something a reader's MVCC snapshot would not see, and readers must never wait for a writer. Every committed write bumps a seqlock generation and records, per key, the generation that last changed it. An entry is tagged with the snapshot generation of the reader that built it, and a reader may use it only if the key has not changed since either snapshot and both are above a floor. Anything uncertain bypasses to redb.
4. It is pure Rust, in process, memory only, sized from the hardware, and `--read-cache-bytes 0` turns it off. There is no on-disk format change.
5. An external cache such as Redis is rejected: it is slower than a local decode, cannot follow MVCC snapshots or Raft applied indexes, and adds an ops dependency to an embedded single-file database.

## Context

References are to `origin/main` on 2026-10-05.

- **The only cross-query cache is redb's page cache** (`--cache-bytes`; `V2Store::open_with_cache_bytes`, `crates/graph-store/src/v2.rs` ~2608). When no size is given the code leaves redb's 1 GiB default, but [ADR 0003](0003-data-model.md) (line 242) says it is set explicitly to 256 MiB. The two disagree.
- **Per-query memos die with each call.** `R.texts`, `R.ents` and the files maps live in the per-call reader and are dropped when it returns.
- **Hot spots:**
  1. `dict_rev_lookup` (v2.rs ~251-277) decodes a whole reverse block, up to `DICT_BLOCK_MAX_BYTES` (64 KiB) of strings, to read one id. Hot terms are re-decoded on every query.
  2. `decode_lazy` (`codec.rs:398`) and `symbols()` (`codec.rs` ~483) decode the full symbol section of each candidate file per query. Owner and sibling resolution multiplies this.
  3. Each call opens a new read transaction, about 13 tables, and re-decodes the org, repo and file nodes it touches.
  4. A snapshot install or a compact reopens the store, which empties the page cache.
- **Ids.** File and content ids come from the `next_id` meta counter (v2.rs ~3819-3931) and only grow, so an id is never reused for different content within one store. Reverse dictionary blocks are append-only: `dict_rev_append` (v2.rs ~310) only extends the last block.
- **No cold or concurrent numbers.** `docs/spikes/v2-checkpoint.md` (line 143) measures warm single-reader queries only.

## Decision

Four phases. Each later phase is gated on the one before.

### Phase 0: measure

- **Counters and timers** for dictionary-block, symbol-section and node decodes (count, bytes, nanoseconds), plus per-query wall time, exported on the metrics endpoint (story 24).
- **A read benchmark**, `#[ignore]` and run by hand like `measure_replication`, with an agent-like mix (`search`, `symbols`, grain roll-ups, `describe`) over the vendored corpus. It runs:
  - **cold**: a freshly opened store in a new process, so the redb page cache is empty (the OS file cache is not flushed; the doc states this);
  - **warm**: the same query set repeated after one discarded pass;
  - **concurrent**: 8, 16 and 32 reader threads on the warm store.
- **Decode share**, per reader: the sum of that reader's decode timers divided by the sum of its query wall time. The reported value is the median across readers.
- **Gate (exact):** go on to phase 2 only if decode share is >= 25% at warm single-reader, **or** >= 25% at any of 8, 16 or 32 readers. The gate is re-evaluated after phase 1 with the same benchmark, because phase 1 removes part of the dictionary decode.
- **Who decides:** the numbers and the go/no-go are recorded in a spike doc (`docs/spikes/read-cache.md`), and the owner signs off there before story 47 starts.
- **No timing is a CI assertion** anywhere in this ADR. Benchmarks are manual and compare cache on and off within one run. CI gates only on counters and on answers.

### Phase 1: cheap wins

- **Find an id inside an encoded dict block without decoding it.** Scan the varint lengths, skip strings that are not the target, and allocate only the hit. A counter (`dict_strings_decoded`) proves one string is decoded per lookup.
- **Page-cache sizing.** A pure function `derive_cache_bytes(avail_bytes) -> u64` returns 25% of available memory, clamped to [64 MiB, 4 GiB]. `--cache-bytes` overrides it. ADR 0003 gets a dated note ("2026-10-xx: superseded by ADR 0008 phase 1; the default is now derived") rather than a silent edit.

### Phase 2: a decoded-object cache

#### Contents and scope

- Values: decoded dictionary blocks; symbol sections together with stream headers and checkpoints; file context. Values are `Arc<T>` with `T: 'static + Send + Sync`, so by type they cannot borrow from, or hold, a `ReadTransaction`.
- Keys: `DictBlock(block_no)`, `Symbols(file_id)`, `FileCtx(file_id)`, `Repo(repo_id)`, `Org(org_id)`. File context is stored as a file entry that refers to separate repo and org entries, so a repo or org update invalidates one key, not every file under it.
- One cache per `V2Store`, memory only. The slot swap on a snapshot install or a compact drops the old store and its cache. A crash loses the cache and nothing else, so crash and re-run are unaffected.

#### Generations

State, beside the cache:

- `gen: AtomicU64`, a seqlock word. It is even when no commit is in progress and odd while one is; each successful commit moves it on by 2.
- `last_mod: Map<Key, u64>`: the generation that last changed each non-dictionary key. An absent record means "unchanged since before F".
- `F: AtomicU64`: the floor generation. Nothing below it is trusted.

**Safety property and optimisation.** The use-time check below is the safety property; everything else (the insert-time check, pruning) is an optimisation or a memory bound. A reader that is unsure of anything reads redb, so uncertainty only ever costs an uncached read.

**Writer protocol (seqlock).** Writes are already serialised by redb's single write transaction. Every write goes through a `RecordingWriteTxn` wrapper whose only commit method takes the touched-key set (`commit(self, touched: TouchedKeys)`), so a write cannot commit without declaring what it touched; this is type-enforced. Commit does:

1. `gen` += 1 (now odd: commit in progress);
2. commit the redb transaction (including the fsync);
3. on success, set `last_mod[k] = gen + 1` for each touched key, then `gen` += 1 (even, the new generation); on failure, raise `F` to `gen + 1` and `gen` += 1, which can only over-invalidate.

An aborted transaction that never reaches commit touches nothing. A failure between the redb commit and step 3 (a failpoint covers this) leaves `gen` odd only until the error path runs, which raises `F`.

**Reader protocol (wait-free).** A reader reads `g1 = gen`, calls `begin_read`, reads `g2 = gen`, registers in the reader table (below), and re-reads `g3 = gen`. If `g1` is odd or `g1 != g2` or `g2 != g3`, the reader bypasses the cache for its whole lifetime (it still reads redb normally). Otherwise `G = g1`.

- **Memory ordering.** We choose explicit Acquire/Release (not SeqCst) because the only cross-thread edges are the ones listed, and they are cheap on every shipped target:
  - the writer's increment to odd is Release and happens-before the redb commit is published; the increment to even is Release, after `commit()` returns;
  - the reader's `g1` load is Acquire and is not reordered after `begin_read`; its `g2` (and `g3`) loads are Acquire and are not reordered before `begin_read`.
  - **redb assumption:** `commit` and `begin_read` synchronise through redb's transaction tracker (a mutex and atomics). That is an implementation detail, not a documented guarantee, so it is pinned by a `loom` model of the protocol and by a stress test that checks each reader's snapshot (its own `next_id` and Raft marker) against its `G`. If either fails on a redb upgrade, the reader path falls back to SeqCst plus a fence.
- **One `G` per read transaction:** a store call that opens more than one read transaction (for example snapshot paging) does a separate seqlock check and registration for each. A reader that starts mid-commit bypasses for its whole lifetime, so the benchmark reports the bypass rate under a concurrent indexer.
- **Why a seqlock, not a mutex or try-lock:** redb readers never wait for a writer today, and a lock held across a commit (with its fsync, and every chunk of `commit_each_counted`) would queue readers behind indexing. A try-lock-else-bypass would also be wait-free but would bypass for the whole commit too while adding a lock word to the hot path; the seqlock costs three atomic loads and bypasses only readers that actually overlap a commit. Correctness: if `gen` was even and unchanged across `begin_read`, no commit finished or started during it, so the snapshot is exactly generation `G`.
- **Acceptance (manual benchmark, not CI):** reader p99 under a concurrent indexer with the cache on is no worse than with the cache off.
- **With `--read-cache-bytes 0`** readers never touch `gen`, `F`, the reader table or the cache. Writers still maintain `gen` and `last_mod` (a map insert per touched key), so the protocol does not depend on configuration.

**Entry tag.** An entry is tagged with `Ge`, the snapshot generation of the reader that decoded it, not the time it was inserted.

- **Use (safety):** a reader at `G` may use an entry for key `K` iff, reading `F_now` at lookup time, `G >= F_now && Ge >= F_now && last_mod(K) <= min(G, Ge)`, where an absent record counts as below `F_now`. Otherwise it reads redb. `F` is re-read on every lookup, so a floor raised mid-reader (vacuum, a cap prune) makes that reader bypass from then on.
- **Insert (optimisation):** skipped if `last_mod(K) > Ge` or `Ge < F_now`, which saves inserting an entry the use check would always reject (for example a slow old reader inserting after a newer write).
- This is correct in both directions: a newer reader never sees an entry built before a change it can see, and an older reader never sees an entry built from a change it cannot see.

**Dictionary blocks are exempt from `last_mod`.** Reverse blocks are append-only (`dict_rev_append` only extends the last block, and strings never change). A cached `DictBlock` records its string count, and a reader at `G` uses it only for ids below its own high-water mark (the dictionary length in its snapshot's meta). A longer cached block is valid for every older id, so no generation check is needed; a shorter one is replaced on a miss. Dictionary entries still obey `F` (vacuum renumbers nothing today, but clear-all drops them for simplicity).

**Clear-all.** `vacuum` and `vacuum_marked` raise `F` to the generation their commit will publish. A snapshot install is not a write: it is a `StoreSlot` slot swap (`state_machine.rs` ~374-385), which drops the old store with its cache and starts an empty one. A phase 3 result-cache clear raises that cache's own floor.

**Touched-key coverage.** Every write path feeds the recorder:

| Write path | Keys touched |
|---|---|
| `commit_prepared` (index, re-index), via `index_prepared_marked` on Raft | `FileCtx`, `Symbols` per file; `Repo`, `Org` if created or changed (dictionary appends need no key) |
| `index_bytes_opts` (commits at v2.rs ~3355, ~3372) | as `commit_prepared`, for its file |
| `commit_each_counted` (v2.rs ~3540) | one touched set per chunk commit; each chunk is its own generation, and readers may land between chunks and see a consistent prefix |
| `ingest_file_with_origin` (origin refresh, v2.rs ~3377), via `ingest_file_marked` | `FileCtx` |
| `remove_content` (v2.rs ~2192), remove-repo, `prune_files`, `prune_files_marked` | `FileCtx`, `Symbols` per file; `Repo`, `Org` |
| `rebuild_refs` / `rebuild_refs_in` (v2.rs ~2322, ~3083) | `All` (raise `F`) unless it can name its files |
| `vacuum`, `vacuum_marked` | `All` (raise `F`) |
| marker-only openraft blank and membership entries (v2.rs ~3057-3062) | no keys, but still a commit, so they bump `gen` |
| test hooks (v2.rs ~2560-2593) | `TouchedKeys::All` |
| open-time writes: `rebuild_encoding_catalog_in`, the `from_db` schema upgrades (~2640-2731), `clear_raft_state` (~2796) | none: they run before the cache exists |

Raft applies use exactly these marked calls (`state_machine.rs` ~292, ~308, ~322, ~326: `index_prepared_marked`, `ingest_file_marked`, `prune_files_marked`, `vacuum_marked`), so followers invalidate as the leader does. A path that cannot name its keys precisely must declare `TouchedKeys::All`.

**Reader table and bounding `last_mod`.** Each reader that passes the seqlock check registers its `G` in a reader table between the second and third `gen` reads (so a reader is registered before it is trusted), and deregisters in `Drop`, which also runs on panic. The oldest registered generation is `Gmin`.

- Pruning and the cap run only inside the writer's step 3, under the `last_mod` map's lock, for the generation `N+2` being published.
- **Prune:** records with generation `<= Gmin` may be dropped, setting `F = max(F, max_dropped)`. A modification at `d` is visible to a snapshot at `d`, so the oldest reader (at `Gmin >= max_dropped`) keeps its cache hits. An absent record then means "older than F", which the use rule treats as unusable for any entry or reader below `F`.
- **Order:** the writer raises `F` (Release) **before** dropping the records; a reader reads `F` (Acquire) **after** its `last_mod` lookup; the map's lock gives the happens-before between them. So a reader that misses a dropped record always sees the raised floor.
- Snapshot handles and long readers pin `Gmin`. gRPC snapshot handles are tied to the existing snapshot-handle TTL and idle expiry, so an orphaned handle deregisters when it expires; a leaked embedded reader pins until dropped.
- `last_mod` is also capped (`max_mod_records`). Exceeding the cap sets `F` to `N+2`, the generation being published (as clear-all does), so every reader then active bypasses from its next lookup rather than memory growing.
- Metric: `read_cache_floor_raises`, counted by cause (vacuum, cap, prune, commit failure).

**Raft followers and crashes.** Followers apply through the same marked calls and recorder. Installing a snapshot is a slot swap and drops the cache. Linearizable reads (`Admin.ReadIndex`) wait for the applied index and then read like any reader. The cache, `gen`, `F` and `last_mod` are memory only: a restart starts empty at a fresh generation, so crash and re-run cannot see stale entries.

#### Engine and sizing

- A sharded, byte-weighted, scan-resistant cache in pure Rust: `quick_cache` or `moka`, each checked against `scripts/check-no-c-deps.py`, or an in-house sharded CLOCK if neither passes or fits. Shard count = next power of two >= 4 x available parallelism.
- One budget: `derive_cache_bytes(avail)` from phase 1 is split 50/50 between the redb page cache and this cache by default. `--cache-bytes` and `--read-cache-bytes` override each share. Resident bytes stay <= the share plus one maximum entry per shard (the documented slack).
- An entry larger than a shard's share is not cached (counted as a bypass).
- Metrics: lookups, hits, misses, bypasses, inserts, rejected inserts, evictions, resident bytes. `hits + misses + bypasses == lookups`.

#### Tests

- `run_differential` gains read-cache configurations: off, one entry, a mid-size eviction-heavy budget, a budget smaller than the smallest entry, and large.
- A consistency differential: concurrent writers against older readers and snapshot handles; each answer equals an uncached store's at the same generation.
- Deterministic interleavings: (a) reader begins, writer commits, reader looks up (must not see the new value); (b) the mirror: a newer reader populates an entry, an older reader must not be served it; (c) a slow old reader inserts after a newer write (rejected); (d) a failpoint between commit and bump; (e) a reader that begins during a commit (odd `gen`) bypasses; (f) a reader active while the `max_mod_records` cap fires, which then looks up a key re-populated at or above the new `F`, bypasses; (g) a reader between two `commit_each_counted` chunks sees a consistent prefix; (h) a reader looking up between the F raise and the record drop is never served stale data. For a cap it bypasses (F = N+2 is above every active reader); for a prune (F = max_dropped <= Gmin) it either sees the record or rejects an entry with Ge < F. The `loom` model and the snapshot-versus-`G` stress test pin the seqlock ordering.
- A concurrent eviction stress test: no stale answers, resident bytes within the bound.
- Snapshot handles held across install, compact and vacuum; `last_mod` pruning with a pinned handle.
- `run_crash_rerun_differential` with the cache on.
- Remote and embedded through `graph-client --test conformance` and `serve_e2e` with `--read-cache-bytes`.
- `ClusterTestbed`: follower reads after applies, a follower that installs a snapshot, and the linearizable-read path.
- Unit tests of the engine: a one-pass scan does not evict the hot set, weight accounting, shard count.
- The size gate and the `codec.rs` golden bytes pass unchanged.

### Phase 3 (optional): a query-result cache

A server-side cache of whole query results keyed by (query, generation), cleared on every write (raising its own floor). Built only if the phase 0 benchmark's request log shows at least 20% of queries are exact repeats within 60 seconds at an unchanged generation.

## Alternatives considered

- **Redis or another external cache (rejected).**
  - A network hop costs about 0.1-0.5 ms plus serialization. That is comparable to, or slower than, a local decode from the page cache.
  - It cannot follow per-reader MVCC snapshots, or each Raft node's applied index, without a versioning protocol. Every cluster node already has the full store locally.
  - It is an ops dependency for an embedded single-file database.
  - Revisit only for a phase 3 result cache shared by many stateless front ends.
- **Simply raising the page cache (rejected as the whole answer).** It keeps pages resident but does not remove the decode and allocation cost per query. Phase 1 still sizes it properly.
- **Caching per read transaction (rejected).** Keeping a read transaction alive to reuse its decodes pins pages and grows the file.
- **Coarse per-table invalidation (rejected in favour of the recorder).** Simpler, but any index run would empty the whole symbol cache. It remains the fallback (`TouchedKeys::All`).

## Consequences

- A new configuration dimension. Query-visible behaviour must not depend on it, under the configuration-equivalence invariant (`run_differential`).
- RSS rises, within the shared budget.
- The write path gains a `RecordingWriteTxn` wrapper and a small map insert per touched key, even with the cache off.
- No on-disk format change: no schema bump, no golden-byte changes, and the size gate is unaffected.

## Stories

| # | Story | Phase | Points |
|---|---|---|---|
| [45](../epic-code-memory-graph.md#story-45) | Read-path measurement and benchmark | 0 | 3 |
| [46](../epic-code-memory-graph.md#story-46) | Dict lookup without full decode; page-cache sizing | 1 | 3 |
| [47](../epic-code-memory-graph.md#story-47) | Decoded-object cache core and MVCC-safe invalidation | 2 | 8 |
| [48](../epic-code-memory-graph.md#story-48) | Read-cache tests, metrics and flags | 2 | 5 |
| [49](../epic-code-memory-graph.md#story-49) | Optional query-result cache | 3 | 3 |

## Open questions

1. The primary workload is assumed to be agents querying `serve` (many concurrent, short, read-only queries). If the main use is the embedded CLI, the cold numbers matter more than the concurrent ones.
2. Whether phase 3 is worth doing depends on what phase 0 shows about repeated identical queries.
