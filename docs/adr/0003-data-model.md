# ADR 0003: Data model for tokens, symbols and postings

**Status:** Proposed (revised after architecture-board, developer and QA review; not accepted). Evidence: [docs/spikes/data-model.md](../spikes/data-model.md) (raw data and prototype code: `spikes/data-model/`). Proposed to supersede the "JSON node per token" part of [ADR 0001](0001-storage.md); redb stays as the per-shard engine.

## TL;DR

- Today a token costs ~525 B of redb pages, because every occurrence is a ~248 B JSON node plus B-tree slack plus two secondary entries; repeated text is only 2.2% of the node. [M]
- Proposal: tokens stop being rows. Per file, one compact **stream** (term id, class, exact span deltas); a **dictionary** (text -> id); **count postings** `(term, file) -> count`; symbols stay rows and carry a per-symbol token ordinal range. A prototype with per-occurrence postings measured ~20 B/token (order of magnitude ~25x smaller), ~3-5x faster ingest, 20-370x faster roll-ups of very common terms. [M, prototype; see "Honest numbers"]
- Decided by the user: the design must support **more than 100 M tokens and scale wide (sharding)**; **migration must exist and be tested before 1.0** (pre-1.0 re-index is fine); readers get **point-in-time snapshots**. These are now core scope (below), including a sharding model that is specified but not built.
- Status stays Proposed until the v2 checkpoint (ADR story 4) gives a go/no-go on real numbers.

## Decisions and open questions

**Decided by the user** (replace the earlier assumed defaults):

| # | Question | Decision |
|---|---|---|
| D1 | Scale target | More than 100 M tokens must be supported and we must be able to scale wide (horizontal). Packed dictionary and block postings are core (ADR stories 5, 6); the store trait must allow partitioning; a sharding model is specified here (stories 14-17). |
| D2 | Migration | Pre-1.0: re-index is acceptable. From 1.0 on existing data MUST migrate. The versioning and migration framework is designed now and must exist and be tested before 1.0 is tagged (ADR story 12). A v1 structure/NDJSON export path stays for agent-supplied data. |
| D3 | Reader consistency | Readers (CLI, MCP, long-running agents) see a consistent point-in-time view while ingest writes: redb MVCC within a shard, a manifest version across shards (ADR stories 10, 16). Visibility is atomic per redb commit (file level); a chunked repo ingest is visible partially and flagged `in_progress` until its final chunk. |
| D4 | Spikes in repo | Spike code, README and logs are committed under `spikes/data-model/`, outside the workspace build; docs get an index and a learnings page. |

**Still open** (assumed default, pending user confirmation, unless marked blocking):

| # | Question | Blocking vs defaultable | Assumed default (pending user confirmation) |
|---|---|---|---|
| Q1 | Are token `NodeId`s persisted by any consumer (MCP, epic story 17, traversal epic story 12)? | Defaultable | Token ids are **unstable across re-index** (stable within a snapshot); documented in the API. Consumers hold `(path, ordinal)` + snapshot if they need more. |
| Q2 | Sharing identical content (vendored deps, forks)? | Defaultable | `content_id` refcounts and a `content_id -> files` multimap exist from day one; fan-out to multiple files is **not** enabled until a real monorepo is measured. Skipped (unchanged) files never touch refcounts. |
| Q3 | Rollout: v2 default at once or opt-in? | Defaultable | v2 opt-in behind the store trait for one release, then flip the default. |
| Q4 | Shard granularity and partition key | Blocking for ADR stories 14-17 (not for 0-13) | Shard unit = one redb file; partition key = `(org, repo)` (a repo never spans shards, an org may); default one shard; split at a configured size (e.g. 200 M tokens or 20 GB). |
| Q5 | Cross-process access: **verified** (redb 2.6.3 `flock(LOCK_EX\|LOCK_NB)`, immediate `DatabaseAlreadyOpen`, no waiting) that a process holding the file blocks readers in other processes; D3 is not met for the separate-process CLI | **BLOCKING user decision** | Options: (a) one owning daemon/MCP process serving reads; (b) CLI retry-with-timeout/back-off; (c) per-shard files with writers on other shards. No default; needed before ADR story 10 is scheduled. |
| Q6 | Max snapshot age and retained-growth limit | Defaultable | 15 minutes, configurable; expired snapshots return `SnapshotExpired`; a warning is emitted at 50% of the limit. |
| Q7 | Shard count ceiling (id layout) | Defaultable | 1,024 shards (10 bits) in the id layout, format byte allows widening. |

Moved out of this ADR: the `keyword` token class gap (no extractor emits it, so `search --kind keyword` is always empty) is a separate issue; the default-limit / streaming CLI question is a separate issue (this ADR only guarantees the store can push `--limit` down). No stop-list and no compression (miniz_oxide) are part of the decision; the stream format byte leaves room to add either later.

## Context

Stories 1-11 store every token occurrence as a JSON `Node` (`nodes`), plus a `children` entry and a `tokens_by_text` entry per token. Measured on `testdata/corpus` (1x, 241,638 tokens) and synthetic 4x (0.97 M) and 41x (9.9 M) sets; all numbers are in the spike ([M] measured, [E] estimated; A = current, E = binary nodes, P = prototype of B/C/D). Baselines: A was measured on main `293bb2a`; PR #8 (skip-unchanged, measured at `47e8616`) is now merged as `231109a`.

- **The distribution hunch is right:** 11,629 distinct texts for 241,638 tokens (4.8%); top 100 texts = 70.9% of occurrences, top 1,000 = 87.9%; 47.7% of distinct texts occur once. [M]
- **The waste is not the repeated text.** Average token text is 5.53 B of a 247.8 B JSON node (2.2%). A token costs **525 B of tree pages** (the components ~248 B JSON + ~225-230 B B-tree slack + `children` ~16 B + `tokens_by_text` ~20 B sum to ~514 B by rounding; the table-level measurement is 489 + 15.7 + 19.5 + small = 525). 1.66 MB of source becomes a 135 MB file (81x). At 9.9 M tokens: 5.2 GB of pages, 6.45 GB file. [M]
- **Cost recurs at query time:** roll-up decodes JSON per hit and per ancestor: `(` at org grain 2.4 s at 9.9 M tokens, token grain 11.3 s, in-process `describe` 7.7 s. Every CLI `search`/`symbols` runs `describe` first: **134 ms of the ~215-261 ms per CLI call** on the corpus DB (O(tokens), in `validate_filters`). [M]
- Re-index of a 5 k-token file 56-98 ms; ingest 129-178 k tokens/s; ingest RSS 1.2 GB at 9.9 M (page cache). [M]

### Requirements the model must keep

1. Containment org > repo > file > symbol > token, `parent` of anything in one cheap lookup.
2. Roll-up search by exact text at token / symbol / file / repo / org grain with hit counts, deterministic order `(org, repo, file path, offset, ordinal)`.
3. Exact spans: byte offsets, 1-based line, 1-based column in Unicode scalar values, start and end; agent-supplied tokens stored "exactly as given" (epic story 13).
4. Language-agnostic schema.
5. Incremental re-index and delete of one file; skip-unchanged by content fingerprint (merged, PR #8); optional sharing of identical content.
6. `describe`, `symbols` search, traversal (epic story 12).
7. Pure Rust (no C dependencies); above 100 M tokens; horizontally scalable; snapshot reads (decisions D1-D3).

## Options

Sizes: tree pages per token at 9.9 M tokens [M]; speeds: ingest at 9.9 M [M]; "E-" marks values not measured. Columns C and D are the prototype with per-occurrence postings; the decision below uses count-only postings (cheaper still, not built).

| | A. Node per token (JSON), today | E. Binary nodes + term ids | B. Dictionary + streams, search by scan | C. B + postings | D. C with a stop-list |
|---|---|---|---|---|---|
| Pages per token (9.9 M) | 529 B | 96 B (5.5x smaller) | ~14 B (37x) | **20.8 B (25x)** | 16.2 B (33x) |
| Ingest at 9.9 M | 129 k tok/s, 77 s, RSS 1.2 GB | 148 k tok/s | 682 k tok/s (stop-100k row) | 549 k tok/s (4.3x; prototype skips validation/origin/prune) | 623 k tok/s |
| `(` at org grain (800 k hits) | 2,409 ms | E-: ~2,000 ms | 301 ms (scan) | **6.5 ms** | 296 ms (scan) |
| `(` at token grain | 11,261 ms | E-: ~5,000 ms | 588 ms | 588 ms (streams must be decoded) | 583 ms |
| Mid/rare term, any grain | 0.05-1.3 ms | E-: similar | ~275 ms (scan) | 0.01-1 ms | 0.01-1 ms |
| `describe` | 7.7 s | E-: seconds | 4 ms | 4 ms | 4 ms |
| Re-index 5 k-token file | 98 ms | E-: ~60 ms | ~10 ms | 14 ms | 11 ms |
| Peak RSS search, 800 k rows | 1.9 GB | E-: ~1 GB | 353 MB | 353 MB | 353 MB |
| Complexity | lowest | low | medium | medium | medium + policy |
| Fits content sharing / sharding | no | no | yes | yes | yes |

Notes:
- **A.** Correct and simple; not scalable past a few million tokens.
- **E.** Cheapest step (5.5x smaller, 1.15x faster ingest, ~6 dev-days with story 0), keeps "everything is a node", but keeps O(hits x ancestors) roll-ups, O(tokens) `describe` and cannot share content. Defensible **only if 10 M is the ceiling**; with D1 (>100 M, sharding) it is rejected, since it needs a second migration to reach the stream model.
- **B.** Smallest, simplest; every search decodes every stream (~28 ns/token: 22 ms at 1 M, 275 ms at 9.9 M, ~2.8 s at 100 M [E, linear]). Used as the go/no-go checkpoint, not the target.
- **C.** Roll-ups at file/repo/org grain never decode a stream. The prototype's positional (ordinal) lists are **not** adopted: token grain for `(` costs ~588 ms either way because streams must be decoded, and no measured query needs ordinals in the posting. Count-only `(term, file) -> count` is the default; positional data is added only if evidence appears (e.g. token-grain on huge files).
- **D.** Stop-list rejected for the decision: -22% pages, but a stopped term becomes a full scan (45-380 ms at 9.9 M, ~3 s at 100 M [E]); correctness is unaffected, so it can be added later if size demands it. Dependencies (no miniz_oxide): none new beyond `sha2`.

### Estimates that are not yet measurements [E]

- Packed single sorted dictionary and block-encoded postings (one row per term per ~4 KiB block, delta-coded file ids): in the prototype the dictionary is 37% of pages at 9.9 M (76 MB for 412 k terms, two copies of every text, mostly slack) and postings ~41 B per `(term,file)` row. **Target 8-12 B/token [E] until stories 5-6 build and measure it.** At 100 M tokens: ~2.1 GB of pages at the prototype's 20.8 B/token [E], ~0.8-1.2 GB at the target [E].
- Sparse stream checkpoints, `--limit` push-down (sorted-by-path iteration, below).

## Decision (recommendation)

**Adopt option C in its count-only form as storage format v2 behind a store trait: interned packed dictionary, one compact stream per content, count-only postings; tokens are not stored rows; files, repos, orgs and symbols are rows. No stop-list, no compression in the decision. Design the store for sharding and snapshots from the start.**

Milestone order: **story 0** (describe fix) -> **store trait / port** (v1 adapter first) -> **v2 dictionary + streams checkpoint (search by scan, go/no-go)** -> **packed dictionary and count postings**. v2 is opt-in behind the trait for one release, then default flips.

| Table (per shard) | Key -> value |
|---|---|
| `meta` | `schema_version`, per-component format versions (below), `next_id`, `next_term`, `commit_epoch` |
| `names` | `parent \0 kind \0 name` -> id (org/repo/file) |
| `ent` | id -> row. File: language, `has_errors`, `origin`, fingerprint (`sha256:<hex>` digest as 32 bytes plus language, extractor+tokenizer version, format: the `sha256:<hex>|lang|extractor+tokN|format` semantics of PR #8), `content_id`, token count, symbol count, per-class token counts, symbol id range. Symbol: symbol kind, lang kind, span, `parent`, first/last token ordinal. |
| `dict` | term text -> id and id -> text (one packed dictionary after ADR story 5) |
| `stream` | `content_id` -> versioned byte stream (format below) |
| `refs` | `content_id` -> refcount; `content_files`: `content_id` -> file ids (multimap) |
| `post` | `(term, content_id)` -> count (block-encoded after ADR story 6) |
| `symbols_by_name` | name -> symbol id |

Stream per token (unchanged from the prototype): `varint(term<<4 | class<<1 | irregular)`, `varint(gap<<1 | newline)`, `line_delta` and `col` after a newline, explicit end fields only when `irregular`. A format byte leads every stream.

### Exact semantics (normative; the differential tests enforce them)

**Spans.** Fields as stored today: `start_byte`, `end_byte`, `start_line`, `end_line` (1-based), `start_col`, `end_col` (1-based, Unicode scalar values, end exclusive). Lines break on `\n`, `\r\n` and bare `\r` as the extractors do today (the extractor defines the truth; the codec must reproduce whatever spans it is given). A BOM is a scalar value in column counting exactly as the v1 path counts it.

**Regular token** (`irregular = 0`): single-line, `end_byte - start_byte == text.len()`, `end_col - start_col == number of scalar values in text`, `end_line == start_line`, and `text == source[start_byte..end_byte]` is not checked by the store (the store has no source), but the other three are. Everything else is `irregular = 1` and stores end line, end column and byte length explicitly (multi-line tokens, tabs or combining marks if an extractor reports them with drift).

**Validation before any write.** `ingest` validates the whole extraction (tokens and symbols) and rejects it with `InvalidSpan` **before** the first write of the transaction, or stores it round-trip exact. The stream must be able to represent: tokens sorted by `(start_byte, ordinal)` non-decreasing; **overlapping, out-of-order and zero-length tokens** are representable through signed `gap` (zigzag) and explicit length when `irregular`; `text` need not equal a source slice (the store has none). Anything not representable (e.g. `start_byte > end_byte`, line 0, column 0, span beyond `u32`, NUL-free constraints of `names`) is rejected, never truncated. This is the epic story 13 rule "exactly as given", and it means the v1 behaviour on the same input (accept or reject) is the oracle.

**Token ordinal.** Position in the file's token sequence in input order after a stable sort by `start_byte`; ties keep input order. Ordinal is dense `0..token_count`.

**Parent of a token (exact rule; mirrors `ingest_into` in `crates/graph-store/src/lib.rs`).** Symbols are sorted by `(start_byte, Reverse(end_byte))` with a **stable** sort, so at equal start the **outer (longer) symbol comes first** and equal-span symbols keep input order; tokens are stably sorted by `start_byte`. The two lists are merged in source order and a symbol is taken before a token when `sym.start <= tok.start`. The stack holds open symbols; before each item, symbols with `end <= pos` are popped:
- a symbol contains a token iff `sym.start <= tok.start` and `tok.start < sym.end` (a token starting **at** a symbol's end is outside it);
- **zero-length symbols contain nothing** (pushed, but popped before the next item);
- when a symbol and a token begin at the same byte the **symbol wins ties** (it is taken first, so the token is inside it);
- **equal-span symbols nest by input order**: the later one is the child of the earlier; a shorter symbol starting at the same byte as a longer one is the longer one's child (longer first), so the symbol row keeps its own `parent`;
- a token contained by no symbol has the file as parent.

**Rejections main enforces (`InvalidSpan`), which the differential oracle depends on and v2 keeps unchanged:** (a) `start > end` for any symbol or token; (b) a symbol **or token** that starts inside an enclosing symbol but ends **after** it (partial overlap of an enclosing symbol). v2 also keeps that overlapping tokens *among themselves* and out-of-order tokens are accepted (representable via signed `gap`); only (a), (b) and the unrepresentable cases above reject. Any relaxation of (a)/(b) is a behaviour change that must be made in v1 first (ADR story 1), or the v1-vs-v2 oracle diverges.

v2 stores, per symbol, the **first and last token ordinal it directly contains** (a range; direct children are contiguous because nesting is a stack), giving O(1) `children(symbol)` and, with the symbol list sorted by `(start, Reverse(end))`, `parent(token)` in one binary search. Tokens outside every symbol are the file's direct token children; children of a file are returned in ordinal order interleaved with symbols by `(start_byte, symbol first)`.

**Per-file counters on the file row:** token count, symbol count, **per-symbol-kind symbol counts** (one counter per `SymbolKind`/`kind_names` entry), and per-token-class counts (vocabulary of 7 classes today; see below). *Story 0 shipped this as an incrementally maintained `catalog` table (per repo/language counts plus per-kind and per-class counts) instead of per-file counters; the v2 design keeps that table or subsumes it with these file-row counters, the reads and results are the same.* `describe` reads only the counters (file rows in the v2 layout): per-language totals are the sum of the file rows of that language, which reproduces `RepoInfo.languages[..].symbol_kinds` (what `validate_filters` uses for `--kind`/`--symbol-kind`), so filter validation needs no scan. `no_symbols` versus `no_matching_symbol` are decided from the symbol count and the per-kind counts. Kinds are counted for the file's language, so a language x kind filter is exact.

**Token `NodeId`.** Namespace: a tag bit separates token ids from entity ids. Layout: one tag bit, then shard, then local id (see Sharding model; a single shard uses shard 0). **JSON safety:** ids are emitted in JSON as **strings** (values above 2^53 must not be numbers); entity ids may also be emitted as numbers only while < 2^53. **Stability:** a token id is stable within a snapshot; across re-index or delete it may be reused or vanish. Resolving a stale id yields `not found` (a token whose file's `content_id`/token count no longer covers the ordinal, or a file id that no longer exists), never a different token silently: the id embeds the file id **and the file's ingest generation** (low bits of `commit_epoch` at last replace), checked on resolution.

**Roll-up and order.** Roll-up counts at file/repo/org read postings, group by parent chain, and sum; class filters or symbol/token grain decode streams of files that have postings. Language filter is applied on the file row first. Deterministic order `(org, repo, file path, start_byte, ordinal)`; the tie-break (..., offset, ordinal) replaces today's symbol-id tie-break and is what the v1 adapter is changed to as well (ADR story 1). **`--limit` push-down:** candidate files are iterated in sorted-by-path order (`names` order) so iteration stops as soon as the limit is reached.

**Epic story 12 traversal on v2.** `children(file)`: symbols and tokens outside symbols in the order above; `children(symbol)`: child symbols and direct tokens by ordinal range; `descendants`: depth-first over the same; `ancestors(token)`: symbol chain by `parent`, then file, repo, org. **Fallback files** (no symbols; fallback tokenizer): children of the file are all tokens in ordinal order. Traversal uses the snapshot's token ids and pages by `(snapshot, cursor)`.

**Class vocabulary:** `TokenClass` (`crates/graph-core/src/schema.rs`) has **7 variants** today (Identifier, Keyword, Literal, Operator, Punctuation, Comment, Other); a 3-bit field has capacity 8, so one value is spare. The `irregular` flag takes the low bit, so `term<<4 | class<<1 | irregular` fits; extending the vocabulary beyond 8 needs a wider field and bumps the stream format byte.

### Incremental update, delete and content sharing

- Replace = validate, then in one chunk: decrement refcount of the old `content_id`, delete its stream, postings and symbol rows when the count reaches zero (old terms found by decoding the old stream), insert new rows. **Skipped unchanged files never touch refcounts** (the fingerprint matches before any write).
- `content_id` equals the file id while sharing is not enabled; the `refs` and `content_files` tables exist from day one so enabling fan-out is not a format change.
- Dictionary is append-only; garbage terms of deleted content remain until `vacuum` (a minimal `vacuum` is in core: rebuild the dictionary from live streams into a new file, then atomic rename; the rebuilt file **preserves `commit_epoch`** and is swapped under the writer lock, so the manifest epoch check still passes; see robustness).

### Robustness (required)

- **Max term length:** terms longer than 256 bytes are stored as `hash(term)` in the dictionary with the text in a side blob (or not interned and stored inline in the stream); policy fixed in ADR story 3 and covered by tests (e.g. minified single-line files, base64 blobs).
- **Per-file size cap or chunking:** a stream larger than a configured cap (default 8 MiB, i.e. millions of tokens) is split into chunks keyed `(content_id, chunk)`; ordinals are global across chunks; a file above a hard cap (token count 2^25 = 33.5 M) is rejected with a clear error.
- **Chunked commits:** ingest commits every N files or M MiB (default 64 MiB) so one transaction never grows unbounded; a crash leaves a whole number of committed files, and the repo/file rows commit last so a half-indexed repo is detected by an `indexing` marker and resumed.
- **redb cache size** is set explicitly (`Builder::set_cache_size`, default 256 MiB, configurable) instead of the 1 GiB default; RSS is a tuning knob (spike observed 1.2 GB at 9.9 M with defaults).
- **Reader MVCC file growth:** an open read transaction pins pages; long snapshots make the file grow and defeat compaction. See Snapshots.
- **Churn / soak benchmark:** a required benchmark re-indexes and deletes a fraction of files repeatedly (e.g. 20 rounds of 10% churn on the 1x and 4x sets) and asserts file size stays within 1.5x of the fresh-ingest size after `vacuum`, and that `vacuum` restores it.

## Versioning and migration (decision D2)

**What `SCHEMA_VERSION` means:** a breaking change of the on-disk **layout** (tables, key encoding), checked at open. v2 layout = `schema_version 3` (story 0 already moved the v1 layout to `schema_version 2`: it adds the `describe` catalog, and the bump makes builds without the catalog refuse the database instead of letting it drift). Each component also has a format version in `meta`: `stream_format` (codec), `postings_format`, `dictionary_format`, `fingerprint_format` (PR #8's `FINGERPRINT_FORMAT_VERSION` plus tokenizer version, so extraction changes force a re-index), and **`derived_version`**. `symbol_index_version` is **redefined as `derived_version`**: the version of derived tables (`symbols_by_name`, per-symbol token ranges, per-class counters), which can be rebuilt from v2 rows without source access and is rebuilt automatically on open when it lags.
- **Old binaries** opening a v2 file get `SchemaMismatch` (documented; nothing corrupts). **A v2 binary opening a v1 file** says: "database is format v1; re-run `index` (pre-1.0) or run `memory-graph migrate` / `export`", and does not modify the file.
- **Migration framework (must exist and be tested before 1.0 is tagged):** `memory-graph migrate --from old.redb --to new.redb`: free-space preflight (need >= 1.2x source size, or the estimated target size), writes to a **temp file next to the target**, per-repo transactions, **differential verification** (counts of files/symbols/tokens per language and class, every span of a sampled 1% plus all spans of the first and last file of each repo, and v1-vs-v2 query output on a fixed query set), then **atomic rename**; the source is never modified. v1->v2 needs no re-parse (v1 nodes carry tokens with spans, class and text). Estimated ~1-2 minutes per 10 M tokens [E].
- **Export path:** `memory-graph export --format ndjson` writes the epic story 13 NDJSON structure (files, symbols, tokens with spans) from any supported version, so agent-supplied data can always be re-ingested.
- From 1.0 on every layout or component format change ships with a migration step and a migration test on a golden v-(N-1) file (see test plan).
- Pre-1.0: re-index is the accepted path; the migrate tool may lag but the framework tests are a 1.0 gate.

## Snapshot isolation (decision D3)

- **API:** `Store::snapshot() -> Snapshot` returns a handle that answers the whole read API (search, symbols, describe, get/parent/children, file_tokens, paging cursors). A multi-call query or traversal (epic story 12, paging) takes one snapshot and uses it throughout; calls on `Store` directly are one-shot conveniences (`snapshot()` then call). A snapshot is `Send`, cheap to clone, released on drop.
- **Single shard:** a redb read transaction (MVCC). Writers are never blocked by readers and readers never see a partial commit (atomicity is **per redb commit, i.e. per chunk**, and gives file-level atomicity; it is not per logical ingest, see the chunked-ingest rule). Repeatable: the same query on the same snapshot returns identical results.
- **Token ids inside a snapshot** are stable (the embedded ingest generation cannot change within a snapshot); after the snapshot, see D-stability rule above.
- **Long-lived snapshots vs file growth and vacuum:** an open read transaction prevents reuse of pages freed by later commits, so the file grows while it lives and `vacuum`/compaction cannot reclaim them. Limits (Q6): **max snapshot age 15 min**, configurable; on expiry the next read returns `SnapshotExpired`; `vacuum` refuses (or waits) while snapshots older than a threshold exist. **Observability:** `store.stats()` reports open snapshot count, oldest snapshot age, DB file size versus live size, and pages retained estimate; a warning is logged at 50% of the max age.
- **Sharded store:** the catalog (manifest) has a monotonically increasing **manifest version**. A writer commits a single shard (a repo never spans shards; a logical ingest may be several chunked redb commits, see the batch rule below) and then publishes a new manifest version (temp file + fsync + atomic rename) recording each shard's `commit_epoch`. redb can only open a read transaction on the **latest committed** state of a file (no time travel), so a snapshot cannot be reconstructed lazily later. `snapshot()` therefore: (1) reads manifest version V; (2) **eagerly opens a read transaction on every shard in the manifest**, all under the manifest protocol before returning; (3) validates each shard's `commit_epoch` (stored in the shard's `meta`, read inside its read txn) against V's record. If every epoch matches, the handle pins V and the open transactions; if a shard is ahead of V (writer committed but has not yet published) or behind, the snapshot **drops the transactions and retries** (re-read the manifest, reopen). The commit-then-publish window is short but real, so the retry is bounded: **up to 5 s (configurable, jittered backoff from 1 ms), after which `snapshot()` returns `SnapshotUnavailable`**. Writers publish the manifest immediately after commit and must not hold a shard commit unpublished across other work. Consequences: no lazy per-shard open; a snapshot costs one open read txn per shard for its whole life; **reader-held pages block page reuse and vacuum in every shard** the snapshot holds, not only the ones it queries, so the max snapshot age (Q6) matters more with many shards. Operations touching several shards (rebalance, move repo) copy to the target, publish a manifest that switches routing (the routing change is carried in the pending intent, see the crash-recovery bullets), and only delete from the source after no snapshot pins a manifest that routes to it (bounded by max snapshot age).
- **Chunked ingests: one batch, one epoch bump, resumable (decision).** A logical ingest (a repo index run) may be split into several redb commits by the size cap (story 3). `commit_epoch` is bumped **once per logical ingest, at its final chunk**; intermediate chunks do not change it. Every chunk transaction stamps the files it writes with the run's `batch_id` and, in the same transaction, writes `meta.open_batch = {batch_id, repo}`; the final chunk clears `open_batch` and bumps the epoch. The V+1 pending record carries `{shard_id, intended_epoch, batch_id}`. Invariants: (i) each file is replaced atomically inside one chunk, so every file row is complete and self-consistent at every commit; (ii) **there is no repo-level atomic visibility during a chunked ingest.** redb serves only the latest committed state (no time travel) and `commit_epoch` does not move between chunks, so a `snapshot()` taken while a batch is open passes the eager-txn and epoch check (the shard's epoch equals the manifest's, so no `SnapshotUnavailable`) and sees a **per-file-consistent but per-repo partially updated view**: some files new, some old, deleted files not yet pruned. This is by design (the alternative, hiding committed chunks, is not possible with redb MVCC). Each snapshot reads `meta.open_batch` **inside its own read transaction** and marks affected repos `in_progress` (batch open) or `incomplete` (crashed batch, per repair) in `describe` and in search results/metadata, so consumers can tell; a completed ingest's snapshot is unflagged and complete. **`repair` policy: mark, do not undo.** If a shard has `open_batch` and the batch never finalized, `repair` verifies shard-internal consistency (chunks are individually consistent), keeps the written files, marks the repo `incomplete` in the manifest (surfaced by `describe` and `verify`), and clears the pending record; the next `index` run of that repo resumes by skipping files whose content fingerprint already matches (the skip-unchanged mechanism) and finalizes with a new batch id and the epoch bump. Deleted-file pruning is done only in the final chunk, so an incomplete repo may still contain files that a completed run would remove.
- **Crash recovery for commit-then-publish (roll-forward is THE rule):** if the writer dies after committing shard N but before publishing the manifest, shard N stays permanently ahead of manifest V and every `snapshot()` would burn the 5 s bound and return `SnapshotUnavailable`. redb commits are durable and cannot be undone, so **rollback is impossible; recovery always rolls forward.** Mechanism: (1) **write-ahead intent:** before committing any shard, the writer atomically publishes (temp file + fsync + rename) a manifest version V+1 that keeps V's committed epochs and adds a `pending` record `{shard_id, intended_epoch, batch_id, routing_change?}` per shard it is about to commit (`routing_change: {repo, from, to}` is present for move/rebalance and is applied to the routing table in V+2 together with the epoch promotion); after the commit it publishes V+2 with the epoch promoted and `pending` cleared. (2) **Reconcile (`repair`)** runs under the exclusive writer lock on writer open, and on demand via `memory-graph repair` (`verify` reports the same findings read-only). For each shard it reads `commit_epoch` from the shard's `meta`: if it equals the manifest's, nothing to do; if it equals the shard's `pending.intended_epoch` (committed, unpublished), it verifies that shard's own consistency (the `verify` checks: counters, refcounts, postings vs streams) and then republishes a manifest with the epoch promoted; if it matches neither the recorded nor the intended epoch, or verification fails, `repair` fails loudly and marks the shard `state = needs_attention` (never guesses). A `pending` record whose shard epoch was not advanced (crash before commit) is simply cleared. **Multi-shard operations (move/rebalance):** a crash after the target shard commits but before the routing switch is published leaves an orphan copy. `repair` therefore also reconciles repos against the routing table: for a pending `routing_change {repo, from, to}` it checks that the target holds the complete repo (file count, token count and per-file fingerprints equal the source's); if so it **rolls the move forward** (publishes routing to `to`), else it **drops the orphan copy** on the target and keeps routing at `from`; a repo present on a shard the routing table does not point at is always treated as an orphan (removed once no snapshot pins a manifest that routes to it). **Fan-out search routes strictly by the manifest routing table, never by presence**, so an orphan can never produce duplicate hits even before repair runs. Repair is idempotent and crash-safe (it only publishes via the atomic manifest rename). (3) **Readers never repair:** on exhausting the bound `snapshot()` returns `SnapshotUnavailable` with a message naming the shard, its epoch versus the manifest's, and the hint "a writer appears to have crashed between commit and publish; run `memory-graph repair` (or restart the owning daemon, which repairs on open)". A live writer's short window still resolves by retry as before.
- **Constraint (Q5, VERIFIED in redb 2.6.3):** `src/tree_store/page_store/file_backend/unix.rs` (~L37-41) takes `flock(LOCK_EX | LOCK_NB)` on open; if another handle holds it, open fails **immediately** with `DatabaseAlreadyOpen`. The lock is exclusive and non-blocking, so a process that holds the file blocks even **readers in other processes**, and redb does **not** wait. Snapshot isolation protects only readers inside the owning process. **D3 is NOT satisfied for today's CLI-vs-writer topology** (CLI and MCP/indexer are separate processes on one file). This is a **blocking user decision (Q5)**: options are (a) a single owning daemon/MCP process that serves all reads and writes (CLI talks to it); (b) CLI retry-with-timeout/back-off on `DatabaseAlreadyOpen` (no snapshot guarantee across processes, only availability between writes); (c) per-shard files, with writers on other shards leaving the queried shards openable. `vacuum` (rebuild into a new file, then atomic rename) is compatible with a live handle lock only because the new file is a different inode: the rename does not disturb the old lock holder, snapshots opened on the old file keep the **old inode** and see the pre-vacuum data until dropped, and new opens see the new file; vacuum itself needs the exclusive lock on the source for its read pass, so it is subject to the same topology decision. **`verify`** is read-only in intent but still opens the files, so it must go through the owning daemon (or it fails with `DatabaseAlreadyOpen` while a daemon holds them); opening a crashed redb file may itself trigger redb's own repair, which needs write access, so in that case `verify` may need the writer lock (not yet verified against redb behaviour on crash-reopen).

## Sharding model (specified, not built; ADR stories 14-17)

- **Unit:** a shard is one redb file holding whole repos (partition key `(org, repo)`; org spans shards, repo never does), with its own dictionary, streams, postings and entity rows. Shard-local ids.
- **Catalog:** `manifest` (small file, atomically replaced) lists shards `{shard_id, path, state, repos, commit_epoch, format versions}` and the routing `(org, repo) -> shard_id`. New repos are assigned to the least-loaded shard under the size limit (Q4).
- **IDs:** internal `u64`: `tag(1) | shard(10) | local(53)` for entities; for tokens `1 | shard(10) | file_local(28) | ordinal(25)`; JSON as strings. Widening is a format-byte bump.
- **Dictionaries: per-shard** (chosen) versus a global dictionary. Per-shard: no cross-shard write coordination, shards are movable and independently vacuumed, term ids never leave a shard; cost: a term's text is stored once per shard that contains it (the common terms are the small part: 11.6 k distinct texts per 241 k tokens) and search resolves the text to a term id in **each** shard. Global: smaller total and one lookup, but a serial write point, and rebalancing rewrites ids. Per-shard is the recommendation; a global dictionary is not needed for correctness because term ids are internal.
- **Search fan-out and merge:** resolve the text per shard, run the per-shard query on a pinned snapshot in parallel, and **merge by the deterministic key `(org, repo, path, start_byte, ordinal)`** (k-way merge of sorted per-shard streams; roll-up counts are summed by group key). `--limit` is applied after the merge with per-shard early termination (each shard yields at most `limit` rows in order). Order is identical to the single-shard result (tested).
- **Rebalancing:** split = move a subset of repos to a new shard by copying their streams (no re-parse), publishing a new manifest, then deleting from the source after snapshots drain. Merge is the inverse. Content sharing (`content_id`) does not cross shards.
- **Failure model:** an unavailable shard fails the query (or returns a partial result only when explicitly requested, marked as such).

## Consequences

**Positive**
- ~25x less disk (order of magnitude, 9.9 M set; target 8-12 B/token [E] after ADR stories 5-6), ingest ~3-5x faster, ingest RSS ~4x lower, re-index ~7x faster, roll-ups of very common terms 20-370x faster, `describe` O(files); CLI calls lose the O(tokens) validation scan.
- Enables `--limit` push-down, content sharing, snapshots and horizontal scale; cold starts read 15-24 MB instead of 50-500 MB per search.

**Negative / risks**
- Tokens are not independently addressable rows: ids are synthetic and unstable across re-index (Q1); a consumer that persists ids must adapt.
- Codec, dictionary and postings must stay consistent: mitigated by differential tests, proptests, golden bytes and a `verify` command.
- Token-grain search decodes whole streams; mitigated by checkpoints later.
- Dictionary grows until `vacuum`; write amplification measured 5.9x (via `wchar`, not device bytes) in the prototype; long snapshots grow the file.
- The epic lists "distributed storage" as out of scope; sharding here means several redb files in one process behind one manifest, not a network service, but the epic text needs a matching amendment when this ADR is accepted.
- The codec is a long-lived commitment (format bytes and migration framework are the mitigation).
- Estimates rely on a prototype that omits validation, `origin`, `has_errors` and prune (see spike section 9).

## Honest numbers

- Ingest "~3-5x": prototype speedup is 5.0x at 1x, 4.7x at 4x, 4.3x at 41x, and the prototype's own throughput degrades from 827 k to 549 k tok/s (4x to 41x); it skips validation, `origin`, `has_errors` and prune, so the real gain is expected at the low end.
- Size "order of magnitude ~25x" (measured 25-26x on pages at 4x and 41x; file-size multiples are coarser because redb grows in steps).
- Roll-up speedups "20-370x" for very common terms (`(`: file 179x, repo 340x, org 370x at 9.9 M; A at 41x is 2 repetitions); token/symbol grain 19-23x. Multiples mix data sets: size at 9.9 M, ingest at 1x-41x, latency at 9.9 M unless stated.
- 100 M projections (~2.1 GB pages, ~2.8 s scan) are [E] linear extrapolations.
- Synthetic scaling: copies rename rare identifiers uniformly; the distinct-text ratio is understated for real repos and dictionary sizes are uncertain in both directions.
- Write amplification (5.9x) is bytes passed to write(2) (`wchar`) over final file size, not device bytes.
- B's re-index cost (~10 ms) and E's search/re-index numbers are unmarked "E-" estimates (not measured); E's search latency was not built.
- 8-12 B/token is a target [E] until packed dictionary and block postings exist.
- The symbol count at 41x (21,061) does not equal 741 x 41 = 30,381 and is unreconciled (spike section 4).

## Dissent recorded

- **Option E** (story 0 + binary nodes, ~6 dev-days, 5.5x smaller) was defended as sufficient **if 10 M tokens is the ceiling**; it is rejected here because D1 sets the ceiling above 100 M.
- **Migration member** objected to a flag-day cutover; hence v2 opt-in for one release, the v1 adapter behind the trait, and the migration framework as a 1.0 gate (D2).
- **Storage member** wanted packed dictionary and block postings in core scope; adopted (ADR stories 5, 6) once D1 raised the scale.

## Story breakdown and estimates

Ideal developer-days including tests, one developer. The single-shard core re-baseline (stories 0-4, 7, 8) is **25-27 days** (recomputed from the story table: 1+3+3+6-7+7-8+4+1; 23 days was the board's earlier figure) before the user's scale/migration/snapshot decisions (story 4 query port 7-8 d); the +/-30% assumes prototype numbers hold. Each story has an ACCEPTANCE line.

| # | Story | Days | ACCEPTANCE |
|---|---|---|---|
| 0 | Remove the O(tokens) `describe` scan from CLI validation (per-file token-class and per-symbol-kind counters on the file row, per-language sums for `RepoInfo.languages[..].symbol_kinds`) | 1 | `search`/`symbols` CLI wall time on the corpus DB drops by >= 100 ms; `describe` results (incl. per-language per-kind symbol counts) unchanged; `--kind`/`--symbol-kind` validation and `no_symbols`/`no_matching_symbol` messages unchanged and need no scan. **Done** (story 0 PR): implemented as an incrementally maintained `catalog` table (per repo/language file, symbol, token counts, symbol-kind and token-class counts; versioned) instead of per-file counters. `SCHEMA_VERSION` 1 -> 2 so older builds refuse the database; opening a version-1 database upgrades it in place (one-time catalog backfill, needs a writable file). Corpus (241 k tokens) p50: `search '('` 239 -> 89 ms, `symbols new` 202 -> 2 ms, `describe` 200 -> 2 ms. |
| 1 | Store trait/port + snapshot handle skeleton, v1 adapter, tie-break `(…, offset, ordinal)` in v1, differential harness | 3 | Existing e2e/corpus tests pass through the trait unchanged; harness runs the fixed query set against two implementations. |
| 2 | Codec: varint, stream, format byte, span validation and `irregular` escape, golden bytes | 3 | Round-trips all 241,638 corpus tokens; property tests (CRLF, lone CR, BOM, combining, astral, tabs, multi-line, zero-length, overlapping, inverted spans) pass; invalid spans rejected before any write; golden fixture bytes committed and checked. |
| 3 | v2 storage layer: entity rows, per-symbol token ranges, counters, streams, refs/content_files, replace/delete/prune, term-length policy, size cap/chunking, chunked commits, explicit cache size, minimal `vacuum` | 6-7 | Replace/delete leave no orphan rows (consistency proptest); skipped files leave refcounts unchanged; oversized term and file cases handled per policy; `vacuum` shrinks after churn. |
| 4 | Query port on v2 (all grains, filters, `symbols`, `describe`, `file_tokens`, get/parent/children/descendants/ancestors, sorted-by-path `--limit`) **and v2 checkpoint: search by scan, go/no-go** | 7-8 | Differential v1-vs-v2 output identical over the full grain x filter x limit matrix incl. BOM, CRLF, bare CR, equal-span/zero-length symbols, tokens outside symbols, overlapping/inverted spans. Go/no-go recorded with real numbers vs targets (size <= 40 B/token pages, no query > 2x the prototype). |
| 5 | Packed single sorted dictionary (D1) | 3 | Dictionary <= 15% of pages at 9.9 M; lookups unchanged; measured, replaces the [E] estimate. |
| 6 | Block-encoded count postings + 100 M measurement (D1) | 4 | Total <= 12 B/token pages at 9.9 M or the ADR target is revised with the measurement; 100 M projection replaced by a measured run. |
| 7 | Parity, guards and soak: existing suites on v2, golden bytes gate, mutation/consistency proptests, size/throughput guard with generous thresholds, churn/soak benchmark | 4 | v2 GA is blocked without golden bytes; guard fails at > 2x regression in pages/token or ingest tok/s; soak keeps the file within 1.5x after `vacuum`. |
| 8 | Opt-in flag for v2, then flip default one release later | 1 | Both engines selectable; default flip is a one-line change guarded by a release note. |
| 9 | Versioning: per-component format versions in `meta`, `derived_version` rebuild (`SYMBOL_INDEX_VERSION` in `graph-store/src/lib.rs` and its rebuild-on-open already exist, so this is mostly a rename plus extension to the new derived tables), v1 detection message, `SchemaMismatch` on old binaries | 2 | v2 binary on v1 file prints the re-run/export message and leaves it untouched; lagging `derived_version` rebuilds on open. |
| 10 | Snapshots, single shard: `snapshot()` handle, max age/`SnapshotExpired`, observability, growth note | 3 | Concurrent reader while re-indexing/deleting sees repeatable results (in-process; cross-process access per Q5 decision); a snapshot older than the limit is refused; stats show snapshot count/age/size. |
| 11 | Query API paging/traversal on a snapshot (epic story 12 semantics incl. fallback files) | 2 | Traversal and paged search over one snapshot yield the same result as a single call while a writer runs. |
| 12 | Migration framework: `migrate` (v1 -> v2), preflight, temp file + verify + atomic rename, differential verification, NDJSON/structure `export` | 6 | Migrating a golden v1 file yields a DB whose query output equals v1's; a failing verification leaves the source and target untouched; export then re-ingest round-trips. **Gate: must exist and be tested before 1.0 is tagged.** |
| | **Core (0-12)** | **~45-47** | ~25-27 d re-baselined single-shard core (stories 0-4, 7, 8) plus D1/D2/D3 additions (stories 5, 6, 9-12 = 20 d). |
| 13 | Sharding design spike: measure per-shard dictionaries at real distinct-text ratios; fix key/ids | 2 | Decision on partition key and id layout recorded; Q4/Q7 answered. |
| 14 | Shard catalog/manifest, id layout, manifest-version snapshots, two manifest publishes per ingest, `repair`, `verify`, kill tests | 8 | Writers publish manifests atomically; `snapshot()` opens read txns on all shards eagerly and validates each `commit_epoch` against the pinned version; a test injects a commit-before-publish window and asserts retry then success, and `SnapshotUnavailable` after the bound; killing a writer mid-publish leaves the previous manifest valid. **Crash recovery:** kill the writer between a shard commit and the manifest publish; assert readers get `SnapshotUnavailable` carrying the repair hint, `repair` republishes a consistent manifest (roll-forward, from the pending intent record), `snapshot()` then succeeds, and no committed data is lost (the killed ingest's data is present and query output equals an uninterrupted run); also kill during repair and re-run to show idempotence. **Kill between chunks:** kill a chunked ingest after chunk k of n; assert `repair` marks the repo `incomplete` without losing committed files, every file row is complete, and re-running `index` skips the already-written files by fingerprint, finalizes the epoch once, and yields query output equal to an uninterrupted run. |
| 15 | Partitioned store: routing, per-shard dictionaries, cross-shard fan-out, deterministic k-way merge, limit early termination | 8 | Sharded results are identical to single-shard results over the differential matrix; ordering deterministic; shard failure reported. |
| 16 | Rebalance (split/move repo) with cross-shard snapshot consistency | 6 | **Kill between target commit and routing publish:** no duplicate hits at any point (fan-out routes by manifest only), `repair` rolls forward if the target is verified complete else drops the orphan, and the repo's data is intact and equal in both outcomes. A concurrent reader during a move sees either the old or the new location, never both or neither; source deleted only after snapshots drain. |
| 17 | Cross-shard snapshot tests and soak (readers during re-index, delete and move) | 3 | Snapshot repeatability and cross-shard consistency tests pass under concurrent writers; **open-batch visibility:** a snapshot opened between chunks returns per-file-consistent data with the repo flagged `in_progress` (in `describe` and search metadata), and after the final chunk a new snapshot is unflagged and complete (the earlier snapshot stays flagged and repeatable); a long snapshot is shown to block vacuum/page reuse on all shards, and expires per Q6. |
| | **Sharding (13-17)** | **27** | Specified, not built; estimates assume ADR stories 0-12 numbers hold. |
| 18 | Content sharing by digest (fan-out via `content_files`) | 4 | Only if real corpora are duplicate-heavy (Q2). |
| 19 | Stream checkpoints for token-grain on huge files | 2 | Only if profiling shows a need. |
| | **With everything** | **~78-80** | |

## Test plan

- **Differential v1-vs-v2** over the full grain x filter x limit matrix on the corpus, including files with BOM, CRLF, bare CR, equal-span symbols, zero-length symbols, tokens outside symbols and overlapping/inverted spans; the v1 store is the oracle.
- **Span property tests against the real extractors** (fallback tokenizer and Rust): CRLF, lone CR, BOM, combining characters, astral (non-BMP) scalars, tabs, multi-line tokens; assert encode/decode identity.
- **Golden fixture bytes** for the codec, dictionary and postings formats; **no v2 GA without them**, and every format change adds a migration test from the previous golden file.
- **Mutation/consistency proptests:** random sequences of index/replace/delete/skip; invariants: no orphan rows, refcounts equal references, postings equal decoded streams, counters equal sums.
- **Snapshot tests:** concurrent reader while re-indexing/deleting, repeatability, cross-shard consistency, `SnapshotExpired`.
- **Migration tests:** golden v1 file migrates and verifies; failure paths (disk full preflight, verification mismatch) leave the source intact.
- **Size/throughput guard** with generous thresholds (fail at > 2x regression) plus the **churn/soak** benchmark.
