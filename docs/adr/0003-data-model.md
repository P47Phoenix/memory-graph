# ADR 0003: Data model for tokens, symbols and postings

**Status:** Proposed (draft for review). Evidence: [docs/spikes/data-model.md](../spikes/data-model.md). Supersedes the "JSON node" part of [ADR 0001](0001-storage.md); redb stays.

## Context

Stories 1-11 store every token occurrence as a JSON `Node` (`nodes` table), plus a `children` entry and a `tokens_by_text` entry per token. The suspicion was that this wastes memory and disk because a few token texts (`(`, `;`, `self`, `return`, `String`) account for most occurrences. Measured on `testdata/corpus` and on synthetic 1 M and 9.9 M token sets (all numbers are in the spike; [M] = measured, [E] = estimated):

- **The hunch about the distribution is right:** 11,629 distinct texts for 241,638 tokens (4.8%); the top 100 texts are 70.9% of occurrences, the top 1,000 are 87.9%; 47.7% of distinct texts occur once. [M]
- **The waste is not the repeated text.** The text is 3.1% of a token node's JSON. A token costs **525 B of tree pages** (248 B JSON + ~230 B B-tree slack + `children` 16 B + `tokens_by_text` 20 B); 1.66 MB of source becomes a 135 MB file (81x). At 9.9 M tokens: 5.2 GB of pages, 6.45 GB file. [M]
- **Cost is paid again at query time:** roll-up searches decode JSON per hit and per ancestor: `(` at org grain 2.4 s at 9.9 M tokens; token grain 11.3 s; `describe` 7.7 s. And every CLI `search`/`symbols` runs `describe` first (~134 ms of ~215 ms per call on the corpus DB; O(tokens)). [M]
- **Also:** re-index of a 5 k-token file 56-98 ms, ingest 129-178 k tokens/s, ingest RSS 1.2 GB at 9.9 M tokens (page cache). [M]

### Requirements the model must keep

1. Containment path org > repo > file > symbol > token, `parent` of anything in one cheap lookup.
2. Roll-up search by exact text at grain token / symbol / file / repo / org with hit counts, deterministic order `(org, repo, file, position)`.
3. Exact spans: byte offsets, 1-based line, 1-based column in Unicode scalar values, start and end.
4. Language-agnostic schema: language string + generic kind vocabulary; no per-language columns.
5. Incremental re-index and delete of one file; content-hash skip of unchanged files (PR `skip-unchanged-files` in flight); possible sharing of identical content.
6. `describe` (per repo: files, languages, symbol kinds, token classes), `symbols` search.
7. Pure Rust (no C dependencies), ~millions of tokens (target: comfortable at 10 M, plausible at 100 M).

## Options

Sizes are tree pages per token at 9.9 M tokens [M]; speeds are ingest at 9.9 M [M]; "E-" marks values I did not measure.

| | A. Node per token (JSON), today | E. Node per token, binary + term ids | B. Dictionary + per-file streams, search by scan | C. Dictionary + streams + positional postings | D. C with a stop-list (top-K terms: no postings) |
|---|---|---|---|---|---|
| Tokens are | JSON nodes | binary nodes | rows in a per-file stream | rows in a per-file stream | same as C |
| Pages per token | 529 B | 96 B (5.5x smaller) | ~14 B (37x) | **20.8 B (25x)** | 16.2 B (33x), K=256 |
| Ingest at 9.9 M | 129 k tok/s, 77 s, RSS 1.2 GB | 148 k tok/s, 67 s | 682 k tok/s (stop-100k row) | **549 k tok/s (4.3x)**, 18 s, RSS 290 MB | 623 k tok/s |
| Very common term, org grain (`(`, 800 k hits) | 2,409 ms | E-: ~2,000 ms (still walks tokens) | 301 ms (scan) | **6.5 ms** | 296 ms (scan) |
| Very common term, token grain | 11,261 ms | E-: ~5,000 ms | 588 ms | **588 ms** | 583 ms |
| Mid/rare term, any grain | 0.05-1.3 ms | E-: similar | ~275 ms (scan, every term) | **0.01-1 ms** | 0.01-1 ms |
| `describe` at 9.9 M | 7.7 s | E-: seconds | 4 ms | **4 ms** | 4 ms |
| Re-index 5 k-token file | 98 ms | E-: ~60 ms | ~10 ms | **14 ms** | 11 ms |
| Peak RSS search, 800 k rows | 1.9 GB | E-: ~1 GB | 353 MB | **353 MB** | 353 MB |
| Complexity | lowest (exists) | low: swap the codec | medium | medium | medium + policy |
| Migration from A | n/a | trivial (same shape, new rows) | rebuild from v1 tokens (no re-parse) | same | same |
| Fits blob sharing | no: a token has one parent | no | yes (stream keyed by content) | yes | yes |
| Fingerprint / skip-unchanged | works (JSON string) | works | works; 32-byte digest row; ~10 us decision | same | same |

Notes on each:

- **A. Current.** Correct and simple; the cost is a JSON envelope per occurrence, B-tree slack because ~250 B values half-fill 4 KiB pages, and two secondary entries per token. Every roll-up decodes JSON per ancestor. Not scalable past a few million tokens.
- **E. Binary nodes.** Varint ids, term ids instead of text, no field names. Cheapest step (5.5x smaller, 1.15x faster ingest) and keeps the "everything is a node" API. But it keeps one row and two secondary entries per token (`e_toks`, `e_children` = 16 + 16 pages per token), so the per-token floor is ~96 B, all roll-ups stay O(hits x ancestors), and `describe` stays O(tokens). Sharing identical files stays impossible.
- **B. Stream + dictionary only.** Smallest and fastest to ingest, and simplest. Every search decodes every stream (~28 ns per token: 22 ms at 1 M, 275 ms at 9.9 M, ~3 s at 100 M [E, linear]). Viable as the first milestone for ~1 M tokens; not the target.
- **C. Stream + dictionary + positional postings.** Search touches only files that contain the term. Roll-up at file/repo/org grain never decodes a stream: counts live in the posting row. Prototype verified equal to A on 66 query cases. **This is the recommendation**, with the improvements listed below.
- **D. Stop-list.** Dropping postings for the top 256 texts saves 22% of C's pages (46 MB of 206 MB at 9.9 M) and 12% of ingest time, but a search for a stopped term (`public`, `return`, `String`...) becomes a full scan: 45-380 ms at 9.9 M instead of 5-14 ms. Because the tokens stay in the stream (spans stay exact, `file_tokens` unchanged) correctness is unaffected. A stop-list is an optimisation for later (100 M scale), not the primary lever. Better variants: count-only postings for stopped terms (a per-file count is ~14 B, little cheaper than positions, [E]) or a lazily built posting.

### Improvements that are estimates, not measurements (E)

- A single sorted dictionary (text -> id, and id -> text as one packed blob or a table) instead of two tables; in the prototype the dictionary is 37% of P's pages at 9.9 M (76 MB for 412 k terms, mostly page slack). Packed, it should be a few MB.
- Block-encoded postings: one row per term and ~4 KiB block, delta-coded file ids, instead of one row per `(term, file)` (~41 B of pages each in the prototype for 3-5 occurrences).
- Together, target ~8-12 B per token [E]. Optional per-file deflate of the stream (pure-Rust `miniz_oxide`): 3.0x on streams [M], costing decode CPU.
- Result streaming and `--limit` push-down: iterate files in `(org, repo, path)` order, stop when the limit is reached. Today `--limit` is applied after the full result is built (RSS 1.9 GB for 800 k rows). [E]
- Sparse checkpoints in the stream (every 64 tokens) so token-grain queries on big files need not decode from the start. [E]

## Decision (recommendation)

**Adopt option C (interned dictionary + one compact stream per file + positional postings) as storage format v2; tokens stop being stored nodes. Keep files, repos, orgs and symbols as entity rows. Do not ship a stop-list initially; design the posting layer so one can be added.** Do the `describe` fix immediately, independent of the format.

Concretely (all keys/values binary; no serde_json on the hot path; redb tables):

| Table | Key -> value |
|---|---|
| `meta` | as today (`schema_version = 2`, `next_id`, `next_term`, index versions) |
| `names` | `parent \0 kind \0 name` -> id (unchanged, org/repo/file) |
| `ent` | id -> binary row: kind, parent, name, and by kind: file (language, has_errors, origin, 32-byte digest, extractor version, symbol id range, token count, `content_id`), symbol (symbol kind, lang kind, span) |
| `dict` | term text -> u32 id, plus `id -> text` (one packed dictionary in the final form) |
| `stream` | `content_id` -> versioned byte stream: per token `varint(term<<4 | class<<1 | irregular)`, `varint(gap<<1 | newline)`, (`line_delta`, `col`) after a newline, explicit end fields only for multi-line or non-ASCII columns |
| `post` | `(term, content_id)` -> count + ordinal deltas (later: per-term blocks) |
| `symbols_by_name` | name -> symbol id (as today) |

- **Containment and parents.** File/repo/org: `parent` in the row as today. Symbols: one row each with `parent`; ids contiguous per file, so a file's symbols are one range read. A token is addressed `(file id, ordinal)`; its parent is the innermost enclosing symbol found by binary search/sweep over the file's symbol range (same rule as `ingest_into` uses today). `Node` for a token is materialised on demand (the public `get`/`parent` API can keep returning `Node`; the token id becomes a packed `(file, ordinal)`).
- **Spans exact.** Six span fields per token are reconstructed from the stream; class stored per occurrence; byte length and end column derived from the dictionary text. Round trip verified on all 241,638 corpus tokens (multi-line comments, non-ASCII, BOM included in the corpus). Property tests must cover CRLF and combining characters.
- **Roll-up.** File/repo/org grain: read posting rows for the term, group by file parent chain, sum counts (no stream reads unless a class filter is present). Symbol/token grain: for the files with postings, decode the stream and emit at the requested grain. Language filter applies on the file row before decoding.
- **Deterministic order.** Unchanged `(org, repo, file path, offset, symbol id)`; term ids are internal and never exposed.
- **Incremental update/delete.** Replace = delete stream, postings for the file's distinct terms (derived by decoding the old stream), symbol range and `symbols_by_name` entries, then insert the new ones. The dictionary is append-only (garbage terms of deleted content remain until an explicit `vacuum`). Measured 14 ms for 5 k tokens at 9.9 M (vs 98 ms).
- **Skip-unchanged (in-flight PR).** Same fingerprint semantics (`sha256 | language | extractor version | format`), stored as a 32-byte digest plus small ints instead of a ~90-char string inside JSON; decision cost is a point read plus the hash (2-12 us measured with SHA-NI). The PR's logic does not need to change beyond the accessor; merge it first.
- **Blob sharing (defer, keep the door open).** Key `stream` and `post` by an opaque `content_id`, equal to the file id at first. Later, identical `(digest, language, extractor version)` files can point at one `content_id` (refcounted); posting hits then fan out to the referencing files through a small `content_id -> files` multimap. Symbol rows stay per file (they are ~0.3% of tokens), so `parent(symbol)` remains a single parent. Not possible in A/E because a token node has exactly one parent. Spike data: only 0.3% duplicate bytes in the test corpus, so the benefit is unproven; decide after measuring a real monorepo.
- **Language-agnostic.** Nothing in the layout knows a language; `class` is the generic 3-bit vocabulary; extractors and NDJSON ingest (story 13) still produce `Extraction { symbols, tokens }`.
- **Pure Rust.** No new required crate. `sha2` (already in the PR) and, optionally, `miniz_oxide` for stream deflate; both pass `scripts/check-no-c-deps.py`.

## Consequences

**Positive**
- ~25x less disk (5.2 GB -> 206 MB at 9.9 M tokens; target ~100 MB with the E-improvements); 4-6x faster ingest; ingest RSS 4x lower; re-index 7x faster; file/repo/org roll-ups of very common terms 16-370x faster, token/symbol grain 19-23x; `describe` O(files); CLI calls lose the O(tokens) validation scan.
- Enables `--limit` push-down and streaming output, blob sharing, and cheaper cold starts (15-24 MB read per search instead of 50-500 MB).
- Fingerprint and skip-unchanged become cheaper and smaller.

**Negative / risks**
- Tokens are no longer independently addressable rows: `Node` for tokens is synthesized; anything that stores token node ids externally must migrate (none known in-repo). Traversal story 12 (`children(file)`) decodes a stream.
- More moving parts: codec, dictionary, postings must stay consistent (mitigated by a differential test against v1 output on the corpus, property tests on the codec, and a `verify` command).
- Token-grain search decodes whole files; very large files with a very common term are the worst case (mitigation: checkpoints, later).
- Dictionary grows monotonically; needs `vacuum`.
- redb write amplification rises (5.9x written/final size at 9.9 M in the prototype) because hot B-tree pages are rewritten per commit; larger batches and the block-postings improvement reduce it.
- Codec format is a long-lived commitment: each stream carries a format byte so it can evolve.
- The prototype is simplified (see spike "Not measured"); real numbers will differ, especially for postings and dictionary layout.

## Migration plan

1. Land `skip-unchanged-files` first (small, independent).
2. Ship storage format v2 behind `schema_version = 2`. The current `SchemaMismatch` check already refuses to open the wrong version, so nothing silently corrupts.
3. `memory-graph migrate --from old.redb --to new.redb`: reads v1 (`nodes` + `children`, all tokens with spans/classes/text are already there, **no re-parse and no source access needed**), writes v2 in per-repo transactions, verifies counts (files, symbols, tokens per language and class) and a sample of spans, leaves the old file untouched. Cost ~ one v1 read plus one v2 ingest: extrapolating from measured throughput, 9.9 M tokens in about 1-2 minutes [E].
4. Alternative for small DBs (all current users): re-run `index`; the corpus re-indexes in 0.3 s (v2) vs 1.4 s (v1).
5. Keep a v1 read path for one release only if a user asks; otherwise delete v1 code after `migrate` exists.
6. Fingerprints carry `FINGERPRINT_FORMAT_VERSION`, so bumping it also forces a re-index of any file whose extraction semantics change.

## Story breakdown and estimates

Ideal developer-days including tests; one developer; +-30%. Stories 12-19 of the epic are unaffected in scope.

| # | Story | Days | Notes |
|---|---|---|---|
| 0 | Remove the O(tokens) `describe` scan from CLI validation (per-file counters on the file row, or validate lazily on empty results) | 1 | Independent of the model; lands now; measured 134 ms of 215 ms per CLI call |
| 1 | Codec module: varint, dictionary-agnostic stream encode/decode, format byte; proptests (CRLF, BOM, multibyte, multi-line tokens, zero-length) | 2 | Spike codec already round-trips the corpus |
| 2 | v2 storage layer: entity rows, dictionary, stream, postings, symbol id ranges, replace/delete/prune, digest row | 5 | Reuse `ingest_into`'s containment sweep |
| 3 | Query port: token/symbol/file/repo/org grains with filters, `symbols`, `describe`, `file_tokens`, `get`/`parent` for synthetic token nodes, `--limit` push-down | 5 | Differential test vs v1 first |
| 4 | Migration `migrate` (v1 -> v2) with verification | 3 | No re-parse |
| 5 | Parity and regression: existing e2e/corpus suites on v2, differential A/P test on the corpus, size/throughput guard for story 18 | 3 | Also update `docs/spikes/storage.md` |
| | **Core total** | **19** | about 4 weeks of one developer |
| 6 | Single dictionary + block-encoded postings (+ optional deflate of streams) | 4 | Target ~8-12 B/token [E] |
| 7 | Adaptive stop-list / count-only postings and `vacuum` | 3 | Only if 100 M scale is a goal |
| 8 | Content sharing by digest (`content_id` fan-out, refcounts) | 4 | Only if real corpora are duplicate-heavy |
| 9 | Stream checkpoints for token-grain on huge files | 2 | Only if profiling shows a need |
| | **With optionals** | **32** | |

Cheaper alternative, not recommended: story 0 plus option E (binary nodes) is ~6 days and gives 5.5x, but needs a second migration later to reach C, and does not fix roll-up cost or blob sharing.

## Open questions

1. **Scale target:** is ~10 M tokens the realistic ceiling, or 100 M+? It decides stories 6-7.
2. **Public API:** are token `NodeId`s used by any consumer (MCP, story 17, the traversal story 12)? If yes, synthetic packed ids must stay stable across re-index of the same content.
3. **Migration:** is "re-index" acceptable pre-1.0 (skip story 4), or must existing databases migrate?
4. **Sharing identical content:** are duplicate-heavy inputs (vendored deps, forks, generated code) expected? The test corpus has 0.3% duplicate bytes.
5. **Keyword class:** no extractor emits `keyword` (0 of 241,638 tokens), so `search --kind keyword` is always empty. Bug/gap to track separately.
6. **Latency budget for common terms:** is 300 ms at 10 M tokens acceptable for stopped-term searches (option D) in exchange for -22% disk? Otherwise never stop-list.
7. **Dependencies:** OK to add `miniz_oxide` (pure Rust) for optional stream compression?
8. **Result size:** should `search` stream and default to a limit for very common terms (800 k rows is 350 MB - 1.9 GB of RSS today)?
