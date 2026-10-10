# ADR 0011: Infix (substring) symbol search

**Status:** Proposed on 2026-10-09. The owner accepts it; no code for story 68 lands before that. Answers issue [#278](https://github.com/P47Phoenix/memory-graph/issues/278). Builds on [ADR 0003](0003-data-model.md) (the v2 store, the interned dictionary, `derived_version` self-heal) and [ADR 0010](0010-symbol-lookup-and-positions.md) D4 (case-insensitive lookup, `sym_fold`, `--exact-case`). Epic: story [68](../epic-code-memory-graph.md#story-68), preferably after story 61 and after story 58 (derived-table self-heal).

## In plain words

1. Issue #278 asks whether `symbols Get` can find `Gadget`. Today it cannot: `symbols` matches a whole name, or a prefix with a trailing `*`.
2. **Scope: symbol names only.** `search` (tokens) stays whole-token. A token-level n-gram index would break the 105 B/token size gate.
3. **Syntax:** `symbols '*get*'`, a leading and a trailing `*`. Case-insensitive by default (ASCII folding, as in ADR 0010), exact with `--exact-case`.
4. **How:** scan the distinct folded symbol names (the keys of the existing `sym_fold` table) for the substring. No new table and no format change in the first step. Only if the bench shows p95 above 50 ms do we add a trigram side table over distinct names, as a derived table with its own `derived_version`.
5. Patterns shorter than 2 characters (after removing the `*`s) are refused; results are bounded by the existing `limit`.

## Context

References are to `origin/main` on 2026-10-09 (602a255).

- `sym_idx` (`SYMBOLS`) is keyed by exact name; `sym_fold` (`SYM_FOLD`, `crates/graph-store/src/v2.rs` ~135, multimap folded-name -> symbol ids) was added by story 57 and carries `DERIVED_VERSION_SYM_FOLD_KEY` (~143). Exact lookup is a `get`; prefix (`name*`) is a byte-order `range` scan.
- A substring cannot use a B-tree range: every distinct key has to be examined, or an n-gram index has to narrow the candidates.
- Distinct symbol names are far fewer than tokens. A token-level trigram index costs postings per token occurrence; with the store at 70 B/token against a 105 B/token gate there is no room for it. An index over *distinct names* scales with the vocabulary, not with the token count.

### Measurement (vendored corpus) and estimate

Measured 2026-10-09 with a throwaway script over `testdata/corpus` (2.43 MB of source):

| Quantity | Value |
|---|---|
| Identifier-shaped tokens | 236,499 |
| Distinct folded identifiers (an upper bound on distinct symbol names) | 12,056 |
| Bytes of those names | 130 KB |
| Substring scan of all of them for `get` (CPython, in memory) | 1.8 ms |

**ESTIMATE for 877M tokens** (not measured): by Heaps' law with an exponent of 0.5-0.6, the vocabulary grows about 60-140x for a 3,700x larger corpus, so roughly 0.7-1.7M distinct names and 10-25 MB of name bytes. A `memchr::memmem` scan of an in-memory buffer of that size is a few milliseconds. Iterating the same keys out of redb costs about 50-100 ns per key, so 35-170 ms: **the storage walk, not the comparison, is the risk to the 50 ms budget.** Story 68 starts by measuring it on the readbench database.

## Decision

### D1. Scope

Infix matching applies to **symbol names only** (`symbols`, its gRPC/MCP equivalents). `search` over tokens keeps whole-token semantics. Token-level infix is out of scope; reopening it needs a new ADR that shows it fits the size gate.

### D2. Syntax and case

- A `symbols` pattern of the form `*needle*` is an infix query. `needle*` stays a prefix query; a bare name stays exact. A leading `*` alone (`*needle`, a suffix query) is accepted and treated as infix-then-filter-by-suffix; it costs the same scan.
- `needle` must not contain `*` (no general globbing). `\*` is not supported; names containing `*` are not a real case in any shipped extractor.
- Case: by default `needle` is ASCII-folded and matched against `sym_fold` keys (ADR 0010 D4). With `--exact-case` / `exact_case`, candidates found through the folded keys are filtered by an exact-case substring test on the real name. Non-ASCII bytes compare exactly in both modes, as in ADR 0010.
- A match is a byte substring of the name. Results keep the existing ordering and roll-up rules of `symbols`, so infix is a superset of the prefix query with the same needle.

### D3. Bounds and refusal

- `needle` shorter than **2 bytes** is refused with `StoreError::InvalidQuery` ("infix pattern needs at least 2 characters"); `**` and `*` alone are refused the same way. A 1-character infix matches almost every name and is never what the user wants.
- `needle` longer than 256 bytes is refused (no name is that long; it bounds the trigram path's work).
- The existing `limit` applies; the scan stops once it has `limit` matches *after* ordering is guaranteed. Because results are ordered by name, the scan walks the keys in order and can stop early, so a common needle is cheap and a rare one pays the full scan.
- A server-side deadline (the existing request deadline) applies as for every read.

### D4. Mechanism, in two steps

1. **Step 1 (no format change).** Walk `sym_fold` keys in order, test each with `memmem`, and resolve matching keys to symbol ids exactly as a prefix query does. If the redb walk is the bottleneck (see the estimate), keep an in-memory, read-only copy of the distinct folded names per open store, built lazily and invalidated by the write generation; it is a cache, never persisted, and its size (about the name bytes) is accounted against the read cache pool of [ADR 0008](0008-read-cache.md).
2. **Step 2 (only if p95 > 50 ms after step 1).** A derived table `sym_tri`: trigram (3 folded bytes) -> a sorted, delta-encoded list of distinct-name ordinals. A query intersects the lists of the needle's trigrams and verifies candidates with `memmem`; 2-byte needles fall back to the step 1 scan. It indexes distinct names, so its size scales with the vocabulary (ESTIMATE: under 10 bytes per name byte, so tens of MB at 877M tokens, well under 0.1 B/token).

### D5. On-disk format and self-heal

- Step 1 changes no stored bytes: no schema or `derived_version` bump.
- Step 2 adds `sym_tri` with its own `DERIVED_VERSION_SYM_TRI_KEY`, built on open when missing or stale, exactly like `sym_fold`, and maintained on every write that touches symbol names. It is covered by story 58's self-heal: a file written by an older binary that does not maintain `sym_tri` is detected and the table rebuilt on the next open. `V2_SCHEMA_VERSION` does not change, because an older binary can still read the file (it ignores the table). Golden-byte tests in `codec.rs` cover the posting encoding.

### D6. Surface

- **Store API:** `SymbolQuery` gains no new field; the pattern itself carries the `*`s, parsed by one shared parser in `graph-store` (`parse_symbol_pattern -> Exact | Prefix | Infix | Suffix`), so embedded and remote agree.
- **Wire:** no proto change; the pattern string already crosses the wire. `InvalidQuery` maps to `INVALID_ARGUMENT` as today. `PROTOCOL_VERSION` is unchanged; an older server sees `*get*` as a literal and returns nothing, so the client checks the server's version in `Hello` and refuses infix with a clear error against a server that predates story 68.
- **CLI:** `memory-graph symbols '*get*' [--exact-case]`. Help and `docs/guide/querying.md` say to quote the pattern so the shell does not glob it.
- **MCP:** the `symbols` tool description documents `*needle*`; no new parameter.

### D7. Performance threshold

p95 of infix queries on the readbench database (877M tokens) must be **<= 50 ms**, warm, embedded, for a fixed set of needles: common (`get`), mid-frequency, rare and absent, each with and without `--exact-case`. Recorded in the story 68 PR next to story 61's numbers. Above 50 ms after step 1, step 2 is required, not optional.

## Tests

- **`run_all` case** (conformance): infix semantics, folding, `--exact-case`, suffix, refusal of short patterns; runs embedded and against `RemoteStore`.
- **Brute-force oracle proptest:** random names and needles; infix results equal a substring filter over all symbol names (both case modes).
- **Parser fuzz test:** arbitrary input to `parse_symbol_pattern` never panics; bad input returns `InvalidQuery`.
- **`run_differential`:** the same answers whatever the chunk/cache/jobs/compaction settings, with and without the in-memory name cache, and (step 2) with `sym_tri` built on open vs during indexing.
- **Size gate:** stays at or below 15x and 105 B/token; with step 2, the gate's measured B/token is recorded.
- **Self-heal (step 2):** a `sym_tri` deleted or stamped stale is rebuilt on open, and queries match a fresh index.

## Consequences

- Answers #278 with a small, format-neutral first step; the expensive index is built only if measurement demands it.
- Infix on a very large store may cost a full vocabulary scan for rare needles; D3 and D7 bound that.
- Token-level substring search remains unsupported, by design.

## Open questions for the owner

1. Accept suffix (`*needle`) as part of the syntax, or only `*needle*`?
2. Is the 2-character minimum right, or should 1-character infix be allowed with a small limit?
3. Is an in-memory name cache (step 1) acceptable as a memory cost charged to the ADR 0008 pool, before considering `sym_tri`?
4. Against an older server, refuse infix in the client (as proposed) or let it return no results?
