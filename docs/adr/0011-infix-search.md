# ADR 0011: Infix (substring) symbol search

**Status:** Accepted by the owner on 2026-10-10 (proposed on 2026-10-09), as proposed; the open questions are resolved in [Owner decisions (2026-10-10)](#owner-decisions-2026-10-10). Amended by the owner on 2026-10-12: D7 is now a per-class gate, and story 68's deviations are accepted ([Amendment](#amendment-2026-10-12-approved-by-the-owner)). Step 1 was delivered in #298; the speedup is story [70](../epic-code-memory-graph.md#story-70). Answers issue [#278](https://github.com/P47Phoenix/memory-graph/issues/278). Builds on [ADR 0003](0003-data-model.md) (the v2 store, the interned dictionary, `derived_version` self-heal) and [ADR 0010](0010-symbol-lookup-and-positions.md) D4 (case-insensitive lookup, `sym_fold`, `--exact-case`). Epic: story [68](../epic-code-memory-graph.md#story-68), preferably after story 61, and after story 58 (derived-table self-heal), which is a hard prerequisite of step 2.

## In plain words

1. Issue #278 asks whether `symbols Get` can find `Gadget`. Today it cannot: `symbols` matches a whole name, or a prefix with a trailing `*`.
2. **Scope: symbol names only.** `search` (tokens) stays whole-token. A token-level n-gram index would break the 105 B/token size gate.
3. **Syntax:** `symbols '*get*'`, a leading and a trailing `*`. Case-insensitive by default (ASCII folding, as in ADR 0010), exact with `--exact-case`. Bare `symbols Get` still means an exact name.
4. **How:** scan the distinct symbol names (the keys of the existing `sym_fold` table, or of `sym_idx` with `--exact-case`) for the substring. No new table and no format change in the first step. Only if the bench shows p95 above 50 ms do we add a trigram side table over distinct names, as a derived table with its own `derived_version`.
5. Needles shorter than 2 bytes are refused. Results come back in the same order as prefix queries, and the existing `limit` applies.

## Context

References are to `origin/main` on 2026-10-09 (602a255).

- `sym_idx` (`SYMBOLS`) is keyed by exact name. `sym_fold` (`SYM_FOLD`, `crates/graph-store/src/v2.rs` ~135, multimap folded-name -> symbol ids) was added by story 57 and carries `DERIVED_VERSION_SYM_FOLD_KEY` (~143). Exact lookup is a `get`; prefix (`name*`) is a byte-order `range` scan; a trailing `\*` means a literal `*` on exact lookup (~1741).
- `symbols` collects every matching id, sorts and dedups them, groups them by file and orders by (org, repo, path). Only the per-file walk stops at offset+limit.
- A substring cannot use a B-tree range: every distinct key has to be examined, or an n-gram index has to narrow the candidates.
- Distinct symbol names are far fewer than tokens. A token-level trigram index costs postings per token occurrence; with the store at 70 B/token against a 105 B/token gate there is no room for it. An index over *distinct names* scales with the vocabulary, not with the token count.

### Measurement (vendored corpus) and estimate

Measured 2026-10-09 with a throwaway script over `testdata/corpus` (2.43 MB of source):

| Quantity | Value |
|---|---|
| Identifier-shaped tokens | 236,499 |
| Distinct folded identifiers (an upper bound on distinct symbol names) | 12,056 |
| Bytes of those names | 130 KB |
| Substring scan of all of them for `get` (CPython, in memory; **indicative only**, not a Rust or redb number) | 1.8 ms |

**ESTIMATE for 877M tokens** (not measured): by Heaps' law with an exponent of 0.5-0.6, the vocabulary grows about 61-137x for a 3,700x larger corpus. That gives roughly 0.7-1.7M distinct names and **about 8-18 MB** of name bytes (130 KB x 61-137). A `memchr::memmem` scan of an in-memory buffer of that size takes a few milliseconds. Iterating the same keys out of redb costs about 50-100 ns per key, so 35-170 ms. **The storage walk and the id collection, not the comparison, are the risk to the 50 ms budget.** Story 68 starts by measuring them on the readbench database.

## Decision

### D1. Scope

Infix matching applies to **symbol names only**: `symbols` and its gRPC/MCP equivalents. `search` over tokens keeps whole-token semantics. Token-level infix is out of scope; reopening it needs a new ADR that shows it fits the size gate. Bare `symbols Get` keeps its meaning (an exact name, case-folded) and still returns nothing for `Gadget`. Only `*get*` matches inside a name.

### D2. Syntax and case

One shared parser lives in `graph-store`: `parse_symbol_pattern(&str) -> Result<Pattern, StoreError>`, with `Pattern = Exact(name) | Prefix(p) | Infix(n) | Suffix(n)`. The embedded store, the server and the CLI all use it, so they agree.

- **Escape, kept from today.** `v2.rs` ~1741 already treats a trailing `\*` as a literal `*` on an exact lookup: `foo\*` finds the name `foo*`. The parser keeps that rule and extends it symmetrically: a leading `\*` is a literal `*`, not an infix marker. No other escapes exist; `\` elsewhere is a literal byte.
- **Names can contain `*`.** The C, C#, F# and Haskell extractors handle operator declarations (a grep shows operator handling in each), so names such as `operator*` or `*` are possible. The exact names emitted were not verified here. Story 68 adds a check that records which extractors actually emit `*`; the escape is required either way.
- **Markers.**
  - An unescaped `*` at both ends makes an infix query.
  - A trailing `*` alone stays a prefix query.
  - A leading `*` alone is a suffix query: the same scan as infix, followed by an `ends_with` test.
  - The needle between the markers must not contain an unescaped `*` (no general globbing). It is used as written: whitespace is not trimmed.
- **Case.** By default the needle is ASCII-folded and matched against `sym_fold` keys (ADR 0010 D4); non-ASCII bytes compare exactly. `--exact-case` scans `sym_idx` (D4).

Expected parses. Needle lengths are in bytes (D3).

| Input | Result |
|---|---|
| (empty) | Rejected: "empty symbol pattern" (today's behaviour) |
| `*` | Rejected: the pattern has no name |
| `**` | Rejected: the infix needle is empty |
| `***` | Rejected: unescaped `*` inside the needle |
| `**get**` | Rejected: unescaped `*` inside the needle |
| `*a*b*` | Rejected: unescaped `*` inside the needle |
| `*get*` | Infix `get` |
| `*get\*` | Suffix `get*` (a leading marker and a trailing escaped literal) |
| `\*get*` | Prefix `*get` |
| `* get *` | Infix ` get ` (the spaces are part of the needle, so in practice it matches nothing) |
| `g*` | Prefix `g` (a prefix has no minimum length, as today) |
| `*g*` | Rejected: the infix needle is shorter than 2 bytes |
| `*é*` | Infix `é` (2 bytes: accepted) |
| `*中*` | Infix `中` (3 bytes: accepted) |

### D3. Bounds, ordering and refusal

- **Lengths are in bytes.** They count the needle after markers and escapes are removed, and the messages say "bytes".
  - Under 2 bytes is refused with `StoreError::Rejected` ("infix needle needs at least 2 bytes").
  - Over 256 bytes is refused ("infix needle is longer than 256 bytes").
  - Tests cover 255, 256 and 257 bytes, with a multibyte character straddling the 256 boundary. The needle is accepted or refused by its byte count and never split.
- **No early stop on the name walk.** Infix keeps today's `symbols` pipeline (Context), so **its result order is identical to a prefix query's**. It pays the full key walk plus id collection on every query.
  - A name-ordered early exit was considered and rejected: it would order results by name, not by (org, repo, path), and break the equality with prefix results.
  - The worst case is therefore a common needle such as `get`, which collects many ids. D7 benches it explicitly.
- **Deadline.** The existing request deadline applies. A scan that exceeds it returns `DeadlineExceeded`, never a partial result.
- **Missing table on a read-only open.** Infix inherits today's behaviour. If `sym_fold` is missing (an old file opened read-only), a folded query is `Rejected` with the existing "reopen it writable" message, and `--exact-case` still works through `sym_idx`.

### D4. Mechanism, in two steps

1. **Step 1 (no format change).**
   - **Default:** walk the `sym_fold` keys, test each with `memmem` against the folded needle, and collect the ids of the matching keys.
   - **`--exact-case`:** walk the **`sym_idx` keys directly** and test the exact needle. This costs the same as the folded walk, and it avoids a fold-then-filter step that would have to read each candidate symbol's real name.
   - **Name cache, if the redb walk is the bottleneck:** keep an in-memory, read-only copy of the distinct names (folded and exact) per open store.
     - It is built lazily and invalidated by the store's write generation.
     - It is a cache and is never persisted.
     - Its size is charged to the read cache pool of [ADR 0008](0008-read-cache.md). If the pool cannot hold it, the query falls back to the redb walk with the same answer.
2. **Step 2 (only if p95 > 50 ms after step 1).**
   - **Stable name ids.** Step 2 needs them, and `sym_fold` positions are not stable. It therefore uses the interned dictionary id (`DICT`, ADR 0003) of each distinct folded name, which is stable for the life of the file. Folded names that are not already in `DICT` are interned when they are written.
   - **Table.** A derived table `sym_tri` maps a trigram (3 folded bytes) to a sorted, delta-encoded list of dictionary ids.
   - **Query.** Intersect the lists for the needle's trigrams, resolve each id to its name through `DICT_REV`, verify with `memmem`, then look the name up in `sym_fold`. Needles of 2 bytes fall back to step 1.
   - **Size, ESTIMATE:** about 20-80 MB at 877M tokens, which is under 0.1 B/token. That is one posting per name byte at 2-4 bytes delta-encoded, plus the extra `DICT` entries for folded names.

### D5. On-disk format and self-heal

- Step 1 changes no stored bytes, so there is no schema or `derived_version` bump.
- Step 2 adds `sym_tri` with its own `DERIVED_VERSION_SYM_TRI_KEY`.
  - Like `sym_fold`, it is built on open when it is missing or stale, and it is maintained on every write that touches symbol names.
  - **Story 58 is a hard prerequisite of step 2.** Without story 58's detection of writes by older binaries, an older binary could add symbols without updating `sym_tri`, and infix would silently miss them.
  - `V2_SCHEMA_VERSION` does not change, because an older binary can still read the file.
  - Golden-byte tests in `codec.rs` pin the posting encoding.

### D6. Surface

- **Store API:** `SymbolQuery` gains no field. The markers travel in `pattern`.
- **Wire (a proto change).** `PROTOCOL_VERSION` stays 1, and `HelloResponse` has nothing to key on: `server_version` is free-form.
  - The ADR adds **`repeated string capabilities`** to `HelloResponse`. A story-68 server lists `"symbol_infix"`.
  - A new client sends an infix or suffix pattern only if the server lists `"symbol_infix"`. Otherwise it fails with a clear error: "this server does not support infix symbol search; upgrade it".
  - An old server would read `*get*` as the prefix `*get` and silently return nothing. That is why the capability field is recommended over documenting the empty result.
  - The `.proto` change is additive, and the generated code is regenerated with the xtask.
- **CLI:** `memory-graph symbols '*get*' [--exact-case]`. The help text says to quote the pattern so the shell does not glob it.
- **MCP:** the `symbols` tool description documents `*needle*`, `*needle` and the `\*` escape. There is no new parameter.
- **Docs:** story 68 updates `docs/guide/querying.md` and the README, replacing story 59's "no infix match yet", and answers #278 with the new syntax.

### D7. Performance threshold

- **Gate:** p95 <= **50 ms**, embedded, on the readbench database (877M tokens), with limit 50 and offset 0. It runs on the readbench machine, named in the PR with its CPU, RAM and disk type.
- **Needles.** Four needles chosen from that database, listed in the PR with their hit counts:
  - a very common one (`get`, the worst case, with thousands of matching names);
  - a mid-frequency one (about 100 matching names);
  - a rare one (1-5 matching names);
  - an absent one.

  Each needle is run with and without `--exact-case`.
- **Procedure:** 10 warm-up iterations, discarded, then 200 timed iterations per needle and mode. The first query, which builds the step 1 name cache, is reported separately as cold latency and is not counted in p95.
- **Remote:** p95 over `RemoteStore` is reported but not gated. It adds the RPC overhead measured in `docs/spikes/rpc-overhead.md`.
- **CI regression check:** on the vendored corpus, a test asserts that infix `*get*` takes at most k = 10 times as long as the prefix query `get*` (median of 20 runs). It catches an accidental quadratic path without depending on machine speed.
- If p95 is still above 50 ms after step 1, step 2 is required, not optional.

## Tests

- **`run_all` cases** (conformance, embedded and `RemoteStore`):
  - **#278's example:** a C# file with `public class Gadget { }`. `*get*` finds `Gadget`; `*Get*` with `--exact-case` does not; bare `Get` returns nothing.
  - **Syntax:** folding, suffix queries, the `\*` escapes, and every refusal in the D2 table.
  - **Exact-case ordering:** names `aB`, `Ab` and `ab`, with the result order identical to the prefix query's.
  - **Exact-case filling:** exact case drops some folded candidates, and enough exact matches remain to fill the limit.
  - **Limits:** 0, 1, N and N+1, where N is the number of matches.
  - **Prefix equality:** infix results equal prefix results whenever every name containing the needle starts with it.
- **Brute-force oracle proptest.**
  - **Inputs:** random names (including non-ASCII) and needles (including `*`, `\` and multibyte characters).
  - **Oracle:** fold ASCII only, then filter all symbol names by substring (infix) or `ends_with` (suffix), in both case modes.
  - **Properties:** infix is a superset of prefix for the same needle, and results follow the existing (org, repo, path) order.
- **Parser fuzz test:** arbitrary input to `parse_symbol_pattern` never panics. Bad input returns `Rejected`, and every accepted pattern answers without error against a small store.
- **Length bounds:** `*é*`, a 3-byte CJK character, and needles of 255, 256 and 257 bytes, with a multibyte character straddling the limit.
- **`run_differential`:** the same answers whatever the chunk/cache/jobs/compaction settings, with the name cache on, off and forced out by a tiny pool. For step 2, also with `sym_tri` built on open and built during indexing.
- **Cache staleness:**
  - Query, then index, delete and re-index, then query again; each answer is compared with a fresh store.
  - Concurrent reads and writes over `RemoteStore` never return an answer older than the read's snapshot.
- **Pool accounting:** with a small ADR 0008 pool, the name cache is charged to the pool (its stats show it) and is evicted or skipped without changing answers.
- **Deadline:** a scan slowed by a test failpoint returns `DeadlineExceeded` and no rows.
- **Wire:** a new client against a server that does not list `symbol_infix` gets the clear error, never an empty result. The server is a mocked `HelloResponse`, or a `TestServer` built with the capability disabled.
- **Size gate:** the database stays at or below 15x its source and 105 B/token. With step 2, the new B/token is recorded.
- **Self-heal (step 2):**
  - A `sym_tri` that is deleted or stamped stale is rebuilt on open.
  - An older binary writes to the file (story 58's harness), and a reopen with the new binary gives infix results equal to the oracle.

## Consequences

- #278 gets a small, format-neutral first step. The expensive index is built only if measurement demands it.
- Every infix query on a very large store pays a full vocabulary walk plus id collection. Common needles cost the most. D3 and D7 bound that cost.
- Older servers need the `capabilities` field to be detected. It is a small additive proto change.
- Token-level substring search remains unsupported, by design.

## Owner decisions (2026-10-10)

The owner accepted this ADR as proposed. Each open question is resolved with the ADR's own proposal.

1. Keep suffix queries (`*needle`), or allow only `*needle*`? **Kept:** `*needle` is supported.
2. Is the 2-byte minimum right, or should 1-byte infix be allowed? **Kept:** the 2-byte minimum stays.
3. Is the in-memory name cache (step 1), charged to the ADR 0008 pool, acceptable before considering `sym_tri`? **Allowed.**
4. Accept the `capabilities` field in `HelloResponse` (D6, a proto change), or accept the silent empty result from older servers? **The `capabilities` field is used:** the client refuses an infix query against a server that does not advertise `symbol_infix`.

## Amendment (2026-10-12, approved by the owner)

Story 68 (#298) delivered step 1 and measured it. Measured p95 values are over 200 warm runs. Setup:

- **Index:** the A5 readbench index (`readbench.redb`, 23.6 GB, 877M tokens), embedded.
- **Query:** limit 50, offset 0.
- **Runs:** 10 warm-ups, then 200 timed runs.
- **Bench:** `crates/graph-cli/tests/infix_bench.rs`.
- **Hits:** symbol rows with no limit.

| Class | Pattern | Case | Hits | p50 | p95 |
|---|---|---|---|---|---|
| common | `*get*` | folded | 321,401 | 613 ms | 696 ms |
| common | `*get*` | exact | 181,855 | 550 ms | 579 ms |
| common, prefix baseline | `get*` | folded | 244,071 | 188 ms | 269 ms |
| common, prefix baseline | `get*` | exact | 145,772 | 123 ms | 147 ms |
| mid | `*handler*` | folded | 16,378 | 452 ms | 507 ms |
| mid | `*handler*` | exact | 1,943 | 416 ms | 469 ms |
| mid | `*serializ*` | folded | 14,719 | 427 ms | 469 ms |
| mid | `*serializ*` | exact | 6,707 | 410 ms | 455 ms |
| absent | `*xyzzyq*` | folded | 0 | 386 ms | 406 ms |
| absent | `*xyzzyq*` | exact | 0 | 455 ms | 626 ms |

### Root cause

- **The name walk.** An infix query walks every distinct name, which costs about 400 ms. The absent needle measures exactly this cost.
- **Loading and sorting every match.** Every query loads the file context of every match and sorts all matches before the limit applies, whatever the pattern. That is why even the prefix query `get*` takes 269 ms with no name walk at all. No name index can remove that cost.

### D7, amended: a per-class gate on the A5 index

The setup is unchanged: embedded, limit 50, offset 0, 10 warm-ups, 200 timed runs, both case modes. The classes and needles are pinned as follows.

- **Rare** (1-5 matching names), **mid-frequency** (about 100 to a few thousand matching names) and **absent** needles: p95 <= 50 ms.
  - Pinned mid needles: `handler` and `serializ`.
  - Pinned absent needle: `xyzzyq`.
  - Pinned rare needle (story 70): `dijkstra`, 4 matching folded names on A5 (`dijkstra`, `dijkstraall`, `priorityqueue_dijkstrasmoketest`, `rundijkstra`).
- **Common** needles: p95 <= 3x the matching prefix query (`*get*` against `get*`), in the same case mode.
  - Pinned common needle: `get`.
- The CI regression check (`*get*` within 10x of `get*` on the vendored corpus) is unchanged.

### Result of step 1 against the amended gate

- **Rare, mid and absent: not met.** p95 is 406-626 ms, against 50 ms. Removing the name walk is still required for them. It is done by the step 2 `sym_tri` table or by a name cache, and is part of [story 70](../epic-code-memory-graph.md#story-70).
- **Common, folded: met.** 696 ms <= 3 x 269 ms = 807 ms.
- **Common, exact case: not met.** 579 ms > 3 x 147 ms = 441 ms.

[Story 70](../epic-code-memory-graph.md#story-70) owns closing all of these. Its first step is lazy file-context loading: a path-rank key, and loading only the files before `offset + limit`. That step also lowers the prefix baseline, so the 3x target tightens with it.

### Result of story 70 against the amended gate

Story 70 made two changes, both with byte-identical output:

- **Lazy file context.** A symbol query filters and orders its candidate files on their compact `file_keys` rows (story 61), by (repo rank, path, file id), with `LazySorted`. It loads the full file context and the stream only of the files the walk reaches, up to the one that fills `offset + limit`.
- **No full name walk: the in-memory name cache (D4 step 1), not `sym_tri`.** Both were measured on A5 before choosing:
  - A5 has 2,022,313 distinct folded names (42.6 MB of name bytes, 5,487,589 symbol ids). A substring scan over them in memory takes 1.2-2.3 ms; the redb walk takes about 400 ms warm.
  - A `sym_tri` table would hold 35.5M (trigram, name) postings over 44,892 trigrams (the longest list, `est`, has 327,432 names). It would need a new derived table, write-path maintenance with removal when a name's last symbol goes, self-heal and the story 58 harness, and it adds bytes per token. The cache needs none of that and is faster once warm.
  - **Cost of the choice:** the first infix or suffix query after an open or a write pays the walk plus building the cache (about 1.0-1.1 s on A5 with the file in the OS cache; 13.5 s on the very first touch of the file). A one-shot CLI command always pays the walk, as before story 70; `serve` and `mcp` keep the cache.
  - **Mechanism:** the cache is keyed by the store's write generation, a count of write transactions kept in memory by a wrapper around the redb `Database`. A read uses the cache only when its read transaction saw exactly that generation with no write in flight, otherwise it walks. Snapshots carry the generation of their read transaction, so a snapshot never sees a cache built after a later write. It is never persisted, so there is no format change, no `derived_version` and nothing for an older writer to miss (an older binary cannot write while the file is open).
  - **Bound:** at most 256 MiB per case mode (`DEFAULT_NAME_CACHE_MAX_BYTES`; about 100 MB on A5), settable with `V2Store::set_name_cache_max_bytes` (0 turns it off). A store whose names do not fit walks. The cap applies per store and per case mode, so one store can hold up to 512 MiB. `serve` and `mcp` have no flag to change it yet. A stale cache (one from before a write) stays in memory until the next infix or suffix query replaces it. Only one build runs per case mode at a time; a concurrent cold query walks instead of building a second copy. The ADR 0008 pool does not exist yet; when it does, this budget moves there.

Measured on A5 (`readbench.redb`, 23.6 GB, 877M tokens), embedded, limit 50, offset 0, 10 warm-ups then 200 timed runs, `crates/graph-cli/tests/infix_bench.rs`. Machine: AMD Ryzen 9 7950X (16 cores), 64 GB RAM, NVMe SSD (Crucial T700). The machine was shared and noisy; the common and prefix rows were run three times and all three are shown.

| Class | Pattern | Case | Hits | p50 | p95 | Gate | Met |
|---|---|---|---|---|---|---|---|
| rare | `*dijkstra*` | folded | 8 | 21.1 ms | 28.0 ms | 50 ms | yes |
| rare | `*dijkstra*` | exact | 4 | 16.9 ms | 18.5 ms | 50 ms | yes |
| mid | `*handler*` | folded | 16,378 | 32.4 ms | 40.8 ms | 50 ms | yes |
| mid | `*handler*` | exact | 1,943 | 22.2 ms | 23.8 ms | 50 ms | yes |
| mid | `*serializ*` | folded | 14,719 | 24.9 ms | 27.3 ms | 50 ms | yes |
| mid | `*serializ*` | exact | 6,707 | 19.2 ms | 20.4 ms | 50 ms | yes |
| absent | `*xyzzyq*` | folded | 0 | 3.6 ms | 3.7 ms | 50 ms | yes |
| absent | `*xyzzyq*` | exact | 0 | 3.5 ms | 3.5 ms | 50 ms | yes |
| prefix baseline | `get*` | folded | 244,071 | 95 / 67 / 69 ms | 140 / 72 / 95 ms | well below 269 ms | yes |
| prefix baseline | `get*` | exact | 145,772 | 144 / 41 / 41 ms | 325 / 50 / 45 ms | well below 147 ms (#298) | runs 2-3 yes; run 1 noisy (max 1,165 ms) |
| common | `*get*` | folded | 321,401 | 87 / 104 / 85 ms | 125 / 216 / 97 ms | 3x `get*` | yes in each run (run 2 at the limit: 216 vs 3 x 72) |
| common | `*get*` | exact | 181,855 | 62 / 62 / 61 ms | 70 / 64 / 69 ms | 3x `get*` | yes |

The remaining cost of the common needle is ordering its candidate files (one `file_keys` read per candidate file); it is about what the prefix query pays. The CI regression check (`*get*` within 10x of `get*` on the vendored corpus) is unchanged.

### Deviations accepted

- **`*` alone still lists every symbol.** The D2 table's "rejected" row for `*` is withdrawn.
- **The name cache of D4 step 1 is not charged to the ADR 0008 pool.** That pool does not exist yet. Story 70 added the cache with its own bound and chose it over `sym_tri` (see [Result of story 70](#result-of-story-70-against-the-amended-gate)).
- **No audit of which extractors emit `*` in names.** The `\*` escape works either way.
- **Kept, as documented in `docs/guide/querying.md`:**
  - patterns ending in `**` keep the "ambiguous" message;
  - a `\*` inside an infix or suffix needle is a literal `*`;
  - the deadline error is `StoreError::DeadlineExceeded`, carried in `StoreErrorDetail` field 21.
