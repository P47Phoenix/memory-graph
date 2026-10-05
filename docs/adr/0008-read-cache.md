# ADR 0008: Read cache pool

**Status:** Proposed (2026-10-05; revised the same day after dev and QA review). Not accepted; the owner decides. Builds on [ADR 0003](0003-data-model.md) (the v2 store) and [ADR 0004](0004-client-server-and-replication.md) (`serve`, Raft). Epic amendment: stories 45-49 in the [epic](../epic-code-memory-graph.md), added as Proposed with the owner's approval.

## In plain words

1. Today the only thing a query reuses from the last query is redb's page cache. Everything decoded from those pages (dictionary blocks, symbol sections, org/repo/file nodes) is thrown away at the end of each call and decoded again by the next.
2. This ADR first measures how much time that re-decoding costs (phase 0), then takes two cheap wins (phase 1), and only if decode passes an exact threshold adds an in-process cache of decoded objects (phase 2).
3. The cache must never return something a reader's MVCC snapshot would not see. Every committed write bumps a generation and records, per key, the generation that last changed it. An entry is tagged with the snapshot generation of the reader that built it, and a reader may use it only if the key has not changed since either snapshot. Anything uncertain bypasses to redb.
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

- `gen`: the generation, bumped once per successful commit.
- `last_mod: Map<Key, u64>`: the generation that last changed each key.
- `F`: the floor generation. Nothing below it is trusted.

**Writer protocol.** Every write goes through a `RecordingWriteTxn` wrapper. Its only commit method takes the touched-key set (`commit(self, touched: TouchedKeys)`), so a write cannot commit without declaring what it touched; this is type-enforced, not a convention. Commit then does, under the cache's `gen_lock` (a mutex):

1. commit the redb transaction;
2. if it succeeded, set `last_mod[k] = gen + 1` for each touched key, then publish `gen = gen + 1`.

The bump happens only after a successful commit. An aborted transaction bumps nothing. A failure between commit and bump (a failpoint covers this) is handled by invalidating everything (`F = gen + 1`, then the bump), which can only over-invalidate.

**Reader protocol.** A reader takes `gen_lock`, calls `begin_read`, reads `G = gen`, and releases the lock. Holding the lock across both makes `G` exactly the generation of the snapshot the reader sees. (A seqlock was considered; the mutex is simpler and is held only for `begin_read`, which is cheap.) If `G < F`, the reader bypasses the cache entirely.

**Entry tag.** An entry is tagged with `Ge`, the snapshot generation of the reader that decoded it, not the time it was inserted.

- **Insert:** rejected if `last_mod(K) > Ge` (a newer write already changed the key) or `Ge < F`. This covers a slow old reader inserting after a newer write.
- **Use:** a reader at `G` may use an entry iff `Ge >= F` and `last_mod(K) <= min(G, Ge)`. Otherwise it reads redb and may try to insert its own decode.
- This is correct in both directions: a newer reader never sees an entry built before a change it can see, and an older reader never sees an entry built from a change it cannot see.

**Dictionary blocks.** Blocks are append-only, so a cached block also records its string count. A reader at `G` uses a cached block only for ids below its own high-water mark (the count it would see in its snapshot, held in the reader's meta); a longer cached block is still valid for older ids. `dict_rev_append` records its block as touched as usual.

**Clear-all.** Vacuum, `vacuum_marked`, snapshot import and any phase 3 result-cache clear set `F = gen + 1` (before the bump of their own commit). Entries tagged below `F` are rejected and readers with `G < F` bypass.

**Touched-key coverage.** The recorder must be fed by every write path:

| Write path | Keys touched |
|---|---|
| `commit_prepared` (index, re-index) | `FileCtx`, `Symbols` for each file; `Repo`, `Org` if created or changed; each extended `DictBlock` |
| `dict_rev_append` | the extended `DictBlock` |
| `ingest_file_with_origin` (origin refresh, v2.rs ~3377) | `FileCtx` |
| `remove_content` (v2.rs ~2192), remove-repo, `prune_files` / `prune_files_marked` | `FileCtx`, `Symbols` per file; `Repo`, `Org` |
| marked writes (Raft `raft_sm` path) | as for the unmarked write they wrap |
| `vacuum`, `vacuum_marked`, snapshot import | clear-all (raise `F`) |

A path that cannot name its keys precisely must declare `TouchedKeys::All`, which raises `F`.

**Bounding `last_mod`.** The cache tracks the oldest active reader generation `Gmin` (each reader, and each snapshot handle, registers its `G` and deregisters on drop). Records with generation `<= Gmin` are pruned and the floor is raised to the largest pruned generation (`F = max(F, pruned)`). No active reader is below the new floor, so none loses correctness; a later reader simply finds those entries rejected and repopulates. A snapshot handle held for a long time pins `Gmin`, so `last_mod` grows until it is released; the map is also capped (`max_mod_records`), and exceeding the cap raises `F` to `gen + 1`, which makes old readers bypass rather than grow memory.

**Raft followers.** Applies go through the same `RecordingWriteTxn` path (`state_machine.rs` calls the store's marked writes), so followers invalidate exactly as the leader does. Installing a snapshot is a slot swap and drops the cache. Linearizable reads (`Admin.ReadIndex`) wait for the applied index and then read like any other reader.

**`--read-cache-bytes 0`.** No cache is allocated and the read path does no cache work (no lookups, no counters move). Writes still bump `gen` and record keys, so the protocol does not depend on configuration; the cost is a map insert per touched key.

#### Engine and sizing

- A sharded, byte-weighted, scan-resistant cache in pure Rust: `quick_cache` or `moka`, each checked against `scripts/check-no-c-deps.py`, or an in-house sharded CLOCK if neither passes or fits. Shard count = next power of two >= 4 x available parallelism.
- One budget: `derive_cache_bytes(avail)` from phase 1 is split 50/50 between the redb page cache and this cache by default. `--cache-bytes` and `--read-cache-bytes` override each share. Resident bytes stay <= the share plus one maximum entry per shard (the documented slack).
- An entry larger than a shard's share is not cached (counted as a bypass).
- Metrics: lookups, hits, misses, bypasses, inserts, rejected inserts, evictions, resident bytes. `hits + misses + bypasses == lookups`.

#### Tests

- `run_differential` gains read-cache configurations: off, one entry, a mid-size eviction-heavy budget, a budget smaller than the smallest entry, and large.
- A consistency differential: concurrent writers against older readers and snapshot handles; each answer equals an uncached store's at the same generation.
- Deterministic interleavings: (a) reader begins, writer commits, reader looks up (must not see the new value); (b) the mirror: a newer reader populates an entry, an older reader must not be served it; (c) a slow old reader inserts after a newer write (rejected); (d) a failpoint between commit and bump.
- A concurrent eviction stress test: no stale answers, resident bytes within the bound.
- Snapshot handles held across install, compact and vacuum; `last_mod` pruning with a pinned handle.
- `run_crash_rerun_differential` with the cache on.
- Remote and embedded through `graph-client --test conformance` and `serve_e2e` with `--read-cache-bytes`.
- `ClusterTestbed`: follower reads after applies, a follower that installs a snapshot, and the linearizable-read path.
- Unit tests of the engine: a one-pass scan does not evict the hot set, weight accounting, shard count.
- The size gate and the `codec.rs` golden bytes pass unchanged.

### Phase 3 (optional): a query-result cache

A server-side cache of whole query results keyed by (query, generation), cleared on every write (raising its own floor as a clear-all). Built only if the phase 0 benchmark's request log shows at least 20% of queries are exact repeats within 60 seconds at an unchanged generation.

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
