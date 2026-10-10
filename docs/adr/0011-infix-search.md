# ADR 0011: Infix (substring) symbol search

**Status:** Accepted by the owner on 2026-10-10 (proposed on 2026-10-09), as proposed; the open questions are resolved in [Owner decisions (2026-10-10)](#owner-decisions-2026-10-10). Answers issue [#278](https://github.com/P47Phoenix/memory-graph/issues/278). Builds on [ADR 0003](0003-data-model.md) (the v2 store, the interned dictionary, `derived_version` self-heal) and [ADR 0010](0010-symbol-lookup-and-positions.md) D4 (case-insensitive lookup, `sym_fold`, `--exact-case`). Epic: story [68](../epic-code-memory-graph.md#story-68), preferably after story 61, and after story 58 (derived-table self-heal), which is a hard prerequisite of step 2.

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
