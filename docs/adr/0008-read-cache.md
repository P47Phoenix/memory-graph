# ADR 0008: Read cache pool

**Status:** Proposed (2026-10-05). Not accepted; the owner decides. Builds on [ADR 0003](0003-data-model.md) (the v2 store) and [ADR 0004](0004-client-server-and-replication.md) (`serve`, Raft). Epic amendment, if accepted: stories 45-49 in the [epic](../epic-code-memory-graph.md).

## In plain words

1. Today the only thing a query reuses from the last query is redb's page cache. Everything decoded from those pages (dictionary blocks, symbol sections, org/repo/file nodes) is thrown away at the end of each call and decoded again by the next.
2. This ADR first measures how much time that re-decoding costs (phase 0), then takes two cheap wins (phase 1), and only if decode is a large share of query time adds an in-process cache of decoded objects (phase 2).
3. The cache must never return something a reader's MVCC snapshot would not see. A write bumps a generation counter and records the keys it touched; a reader uses an entry only if that key is unchanged since both the entry was made and the reader's snapshot began. Otherwise it reads redb as today.
4. It is pure Rust, in process, sized from the hardware, and `--read-cache-bytes 0` turns it off. There is no on-disk format change.
5. An external cache such as Redis is rejected: it is slower than a local decode, cannot follow MVCC snapshots or Raft applied indexes, and adds an ops dependency to an embedded single-file database.

## Context

References are to `origin/main` on 2026-10-05.

- **The only cross-query cache is redb's page cache** (`--cache-bytes`; `V2Store::open_with_cache_bytes`, `crates/graph-store/src/v2.rs` ~2608). When no size is given the code leaves redb's 1 GiB default, but ADR 0003 (line 242) says the cache is set explicitly to 256 MiB. The two disagree.
- **Per-query memos die with each call.** `R.texts`, `R.ents` and the files maps live in the per-call reader and are dropped when it returns.
- **Hot spots:**
  1. `dict_rev_lookup` (v2.rs ~251-277) decodes a whole reverse block, up to `DICT_BLOCK_MAX_BYTES` of strings, to read one id. Hot terms are re-decoded on every query.
  2. `decode_lazy` / `symbols()` (`codec.rs` ~483) decode the full symbol section of each candidate file per query. Owner and sibling resolution multiplies this.
  3. Each call opens a new read transaction, about 13 tables, and re-decodes the org, repo and file nodes it touches.
  4. A snapshot install or a compact reopens the store, which empties the page cache.
- **No cold or concurrent numbers.** `docs/spikes/v2-checkpoint.md` (line 143) measures warm single-reader queries only. We do not know the decode share of query time, nor the behaviour with many concurrent readers (the expected load from agents querying `serve`).

## Decision

Four phases. Each later phase is gated on the one before.

### Phase 0: measure

- Decode and hit counters, and timers, for dictionary blocks, symbol sections and node decodes, exported on the metrics endpoint (story 24).
- A read benchmark with an agent-like workload (mixed `search`, `symbols`, grain roll-ups, `describe`), run cold, warm, and with 8 to 32 concurrent readers. Results recorded in a spike doc next to `v2-checkpoint.md`.
- **Gate:** go on to phase 2 only if decode is at least about 25% of warm query time. Phase 1 goes ahead regardless.

### Phase 1: cheap wins

- **Find an id inside an encoded dict block without decoding it.** Scan the varint lengths, skip strings that are not the target, and allocate only the hit.
- **Page-cache sizing.** Fix the 256 MiB / 1 GiB mismatch (code and ADR 0003 must agree) and derive the default from the hardware (a fraction of available memory, clamped), keeping `--cache-bytes` as the override.

### Phase 2: a decoded-object cache

- **Contents:** decoded dictionary blocks; symbol sections together with stream headers and checkpoints; file context (file, repo and org nodes). Values are immutable and shared as `Arc`.
- **Scope:** one cache per `V2Store`. The slot swap on a snapshot install or a compact drops the old store and its cache with it, so a replaced database can never serve stale entries.
- **MVCC-safe rule:**
  - A generation counter is bumped once per committed write.
  - The writer records the keys it touched: the files written in `commit_prepared`, and the dictionary block extended in `dict_rev_append`. Vacuum clears the whole cache.
  - Raft followers invalidate the same way, because they apply through the same write path (`state_machine.rs` calls the store's marked writes).
  - A reader captures the generation at `begin_read`. It uses an entry only if the key has not changed since both the entry's creation and the reader's snapshot. Otherwise it bypasses the cache and reads redb, so an old reader (including a snapshot handle) still sees exactly its own snapshot.
  - Never cache a `ReadTransaction`: a long-lived read transaction pins pages and makes the file grow (ADR 0003's file-growth warning).
- **Engine:** a sharded, byte-weighted, scan-resistant cache in pure Rust. Candidates are `quick_cache` and `moka`, each checked against `scripts/check-no-c-deps.py`, or an in-house sharded CLOCK if neither passes or fits.
- **Sizing:** one hardware-derived byte budget shared between the redb page cache and this cache. `--read-cache-bytes` sets this cache's share; `0` turns it off. Metrics: hits, misses, evictions and resident bytes.
- **Tests:**
  - `run_differential` gains read-cache configurations: off, one entry, and large.
  - A new consistency differential runs concurrent writers against old readers and snapshot handles, and compares each answer with an uncached store at the same generation.
  - Install and compact equivalence: answers after a snapshot install or a compact must equal an uncached store's.

### Phase 3 (optional): a query-result cache

A server-side cache of whole query results keyed by (query, generation) and cleared on every write. Built only if phase 0 shows agents repeating identical queries often enough to matter.

## Alternatives considered

- **Redis or another external cache (rejected).**
  - A network hop costs about 0.1-0.5 ms plus serialization. That is comparable to, or slower than, a local decode from the page cache.
  - It cannot follow per-reader MVCC snapshots, or each Raft node's applied index, without a versioning protocol. Every cluster node already has the full store locally.
  - It is an ops dependency for an embedded single-file database.
  - Redis over TLS conflicts with the pure-Rust gate (`ring` and `aws-lc-sys` are denied).
  - Revisit only for a phase 3 result cache shared by many stateless front ends.
- **Simply raising the page cache (rejected as the whole answer).** It keeps pages resident but does not remove the decode and allocation cost per query. Phase 1 still sizes it properly.
- **Caching per read transaction (rejected).** Keeping a read transaction alive to reuse its decodes pins pages and grows the file.

## Consequences

- A new configuration dimension. Query-visible behaviour must not depend on it, under the configuration-equivalence invariant (`run_differential`).
- RSS rises, within the shared budget.
- No on-disk format change: no schema bump, no golden-byte changes, and the size gate is unaffected.
- The write path gains a little bookkeeping (the generation bump and the touched keys).

## Stories

Proposed epic stories, numbered after story 44:

| # | Story | Phase | Points |
|---|---|---|---|
| [45](../epic-code-memory-graph.md) | Read-path measurement and benchmark | 0 | 3 |
| [46](../epic-code-memory-graph.md) | Dict lookup without full decode; page-cache sizing | 1 | 3 |
| [47](../epic-code-memory-graph.md) | Decoded-object cache core and MVCC-safe invalidation | 2 | 8 |
| [48](../epic-code-memory-graph.md) | Read-cache tests, metrics and flags | 2 | 5 |
| [49](../epic-code-memory-graph.md) | Optional query-result cache | 3 | 3 |

## Open questions

1. The primary workload is assumed to be agents querying `serve` (many concurrent, short, read-only queries). If the main use is the embedded CLI, the cold numbers matter more than the concurrent ones.
2. Whether phase 3 is worth doing depends on what phase 0 shows about repeated identical queries.
