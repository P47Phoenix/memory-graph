# Spike: v2 store checkpoint measurements (ADR 0003 story 4)

Status: measurements for the story 4 go/no-go input. Date 2026-09-21. This document presents numbers and a recommendation; **it does not decide go/no-go, the user does.** ADR 0003 stays Proposed.

**TL;DR.** At 9,876,231 tokens (public corpus plus the `syn` crate, replicated 9x) the v2 store is 24x smaller than v1 (257.5 MiB vs 6,256.5 MiB, 27.3 vs 664 bytes per token), ingests 5.1x faster (15.2 s vs 76.9 s), and no measured query is more than 1.51x slower than v1 (the limit is 2x); broad queries are up to about 500x faster. The `search_symbols` regression of issue #22 is fixed (4.7x slower before, 0.7x to 0.9x now, 0.14x for the full listing). On the **real, unreplicated** `syn` crate alone (855,726 tokens) two numbers are worse: the size is just under the 40 bytes per token limit (39.95), and a selective token-grain term such as `new` is 2.7x to 2.9x slower than v1 (8 ms vs 3 ms), because v2 walks the whole token section of every candidate file. The recommendation is at the end.

## Method

**Machine.** Same as the [data model spike](data-model.md): Intel Core Ultra 9 275HX, 30 GB RAM, NVMe, btrfs, Linux, rustc 1.94.1, redb 2.6.3, `--release`, one process, one writer. Other sessions were running on the machine during these runs (load average 3 to 4 at the end), so treat differences under about 15% as noise. Timings are wall clock, in-process, warm page cache, p50 over 15 repetitions (syn) or 5 repetitions (9.9 M; a v1 query takes up to 5 s there). One run of each; no confidence intervals.

**Harness.** `crates/graph-store/examples/v2bench.rs` (committed, run with `--release`, not part of `cargo test`). It indexes the same files into a v1 file and a v2 file through the `Store` trait (`index_batch`, one write transaction per repo, as `memory-graph index` does), then runs the same queries against both, checks that the two backends return identical rows, and finally compacts both files with redb and reports the size again.

**Data.**
- *syn alone:* `syn` 2.0.119 from the cargo registry, 162 files, 855,726 tokens, 14,888 symbols. Real code, not replicated.
- *9.9 M set:* the eight-repo `testdata/corpus` plus `syn`, replicated 9 times with the scaler of the data model spike (copy k renames identifiers of length 4 or more with corpus frequency 50 or less by appending k; copy k goes to repo `<repo>-k`, org `org<k mod 10>`). 7,227 files, 9,876,231 tokens, 140,341 symbols. This is a **synthetic** stand-in: the copies have the same shape and the same common terms, so the dictionary and the file-size distribution are those of about 1.1 M real tokens repeated. Real repos overlap differently (the direction of the error for size is unknown).
- Query terms: `(` (most common), `self` (keyword), `new` and `Result` (identifiers), each at token, symbol, file, repo and org grain; a class filter; `--limit 100`; and `search_symbols` for `new`, `new*`, `*`, `fmt`.

**What "size" means here.** `ls -l` of the redb file (redb grows in large steps and copy-on-write leaves free pages), and the size after redb's own compaction. The ADR target is "size <= 40 B/token pages"; the page measure of the data model spike (tables' leaf and branch pages) was not repeated, so the file size is a slightly pessimistic stand-in.

Raw output: `spikes/data-model/logs/v2_checkpoint_9.9M.txt`, `v2_checkpoint_syn_slice2.txt` (this change) and `v2_checkpoint_syn_main.txt` (main before this change, same harness).

## Result 1: 9.9 M tokens, v1 vs v2

| Measure | v1 | v2 | Ratio | ADR target |
|---|---|---|---|---|
| Tokens / files / symbols | 9,876,231 / 7,227 / 140,341 | same | | |
| File size | 6,256.5 MiB | 257.5 MiB | 24.3x smaller | |
| Bytes per token (file) | 664.3 | 27.3 | | v2 <= 40: **met** |
| Size after redb compaction | 5,634.2 MiB | 257.5 MiB | 21.9x smaller | |
| Ingest, whole set | 76.9 s | 15.2 s | 5.1x faster | |
| Peak RSS (both stores, all queries, one process) | 2.76 GB | | | not separated |

| Query (rows) | v1 p50 ms | v2 p50 ms | v2 / v1 |
|---|---|---|---|
| `(` token (847,386) | 3,798 | 935 | 0.25x |
| `(` symbol (129,134) | 5,062 | 587 | 0.12x |
| `(` file (6,606) | 3,091 | 11.9 | 0.004x |
| `(` repo (90) / org (9) | 2,956 / 2,848 | 7.3 / 7.1 | 0.002x |
| `(` token `--limit 100` | 3,542 | 6.8 | 0.002x |
| `self` token (123,768) | 409 | 266 | 0.65x |
| `self` symbol (41,715) | 558 | 232 | 0.42x |
| `self` file / repo / org | 326 / 315 / 312 | 1.7 / 1.2 / 1.1 | 0.005x |
| `new` token (26,937) | 88.8 | 129.4 | **1.46x** |
| `new` symbol (9,407) | 990 | 121 | 0.12x |
| `new` file (3,843) | 74.3 | 7.2 | 0.10x |
| `Result` token (17,100) | 87.2 | 101.9 | **1.17x** |
| `Result` symbol (16,785) | 219 | 103 | 0.47x |
| `Result` file (954) | 85.8 | 1.5 | 0.02x |
| `new` token, class = identifier | 88.4 | 130.0 | **1.47x** |
| `new` file, class = identifier (walks streams, few output rows) | 74.7 | 112.4 | **1.51x** |
| `search_symbols new` (468) | 1.7 | 1.3 | 0.76x |
| `search_symbols new*` (684) | 2.3 | 1.6 | 0.70x |
| `search_symbols fmt` (7,317) | 37.5 | 8.4 | 0.22x |
| `search_symbols *` (140,341) | 619.5 | 106.5 | 0.17x |
| `search_symbols * --limit 100` | 621 | 11.8 | 0.02x |

Every query returned identical rows from both backends. The worst ratio is 1.51x, under the 2x limit. The four rows above 1x are token-grain or class-filtered lookups of terms whose hits are few relative to the token count of the files that contain them.

## Result 2: real `syn` crate alone (855,726 tokens), before and after this change

Size and ingest are the same before and after (this change does not touch the write path): v1 514.5 MiB (630.5 B/token), ingest 5.6 s; v2 32.6 MiB (**39.95 B/token**, just under the 40 B limit), ingest 0.82 s (6.8x faster). After redb compaction v2 was 24.6 MiB in the run of this change and 32.6 MiB (unchanged) in the run on main; the compaction result varied between runs and is not relied on.

| Query | v1 ms (from the "now" run; v1 is unchanged, the "before" run's v1 values were within noise) | v2 before ms (x v1) | v2 now ms (x v1) |
|---|---|---|---|
| `search_symbols new` (42) | 0.1 | 1.6 (15.1x) | 0.1 (0.86x) |
| `search_symbols new*` (66) | 0.2 | 2.0 (12.3x) | 0.1 (0.69x) |
| `search_symbols fmt` (793) | 2.2 | 5.8 (2.67x) | 0.5 (0.23x) |
| `search_symbols *` (14,888) | 50.6 | 33.1 (0.65x) | 6.9 (0.14x) |
| `search_symbols * --limit 100` | 49.0 | 33.4 (0.68x) | 1.0 (0.02x) |
| `(` token (74,673) | 167.5 | 113.7 (0.68x) | 76.3 (0.46x) |
| `self` token (13,512) | 34.5 | 35.2 (1.02x) | 25.3 (0.72x) |
| `new` token (1,237) | 3.0 | 9.9 (3.36x) | 8.1 (**2.71x**) |
| `new` symbol grain (674) | 6.5 | 9.8 (1.59x, v1 5.8 same run) | 7.9 (1.22x) |
| `new` token, class = identifier (1,237) | 2.9 | 9.8 (3.44x) | 8.1 (**2.83x**) |
| `Result` token (1,834) | 6.9 | 11.8 (1.70x) | 9.4 (1.40x) |
| `new` file, class = identifier (88 files) | 2.4 | not run | 7.2 (**2.96x**) |
| `(` token `--limit 100` | 160.0 | 106.7 (0.67x) | 0.4 (0.00x) |

(The issue #22 figures of 4.7x for `search_symbols` and 1.4x for token search came from QA's own query set; the terms here are the harness's fixed set, so the ratios differ.)

## What changed and why

1. **`search_symbols`.** v2 already had a name index (`symbols_by_name`, name to symbol id). The cost was decoding the entire stream (every token record) of every file with a hit, then a dictionary read per name. Now it decodes only the header and the symbol section (symbols come first in the stream, so no layout change), caches dictionary texts and org/repo rows for the query, filters at file level before reading any stream, and reads files in sorted-by-path order. No new table and no size change.
2. **Term search.** The token section is walked record by record without collecting it, and only the matching records are kept; roll-ups that need no symbol names skip the enclosing-symbol lookup. This helped broad terms (`(` 114 to 76 ms, `self` 35 to 25 ms at syn scale) but not selective ones: the walk itself is about 12 ns per token and every token of every candidate file is walked (roughly 600,000 tokens for `new` on syn, an estimate from the 88 candidate files).
3. **`--limit` push-down.** Candidate files are sorted by (org, repo, path); the walk stops at the first file group boundary after the limit is met, because every row of a later group sorts after every row of an earlier one (a group is the file at token and symbol grain, the repo at repo grain, the org at org grain). Result identical to the full walk (differential over limits 0 to 5 at every grain).
4. **Traversal.** `children`, `descendants` and `ancestors` on `StoreRead` (see the ADR story 4 row for the semantics), v1 and v2 checked against each other.

## Addendum: sparse checkpoints and per-term ordinals (ADR story 19)

The follow-up change (this section was added with it) builds the fix named below. Numbers are from the same harness, before and after, on the `syn` 2.0.119 sources as this invocation loads them (106 files, 478,831 tokens, 8,409 symbols; the 855,726-token run above used a larger file set, so absolute times are not comparable with Result 2, only before against after in this table). 25 repetitions, p50, one run each, other sessions running (treat under 15% as noise). Raw output: `spikes/data-model/logs/v2_checkpoint_syn_story19_before.txt` and `v2_checkpoint_syn_story19.txt`.

**Design.** Stream format 2 stores the symbol-section byte length and, before every 64th token record, a checkpoint (byte offset and the delta-coding state, about 6 bytes per 64 tokens). The postings value changes from a count to `count, ordinal gaps...` (varints). A token, symbol or class-filtered search reads the ordinals of the term in each candidate file and decodes only those records, starting at the nearest checkpoint (at most 63 records of walking per hit, none between hits that share a block). The symbol section is decoded only for token and symbol grain and only when a record matched. Roll-ups without a class filter still read the count alone. The codec format byte is 2 and the store schema version is 4; v2 files of the old layout are refused rather than migrated (the layout is unreleased), and v1 is untouched.

| Query (rows) | v1 ms | v2 before ms (x v1) | v2 after ms (x v1) |
|---|---|---|---|
| `new` token (788) | 1.7 | 4.4 (2.61x) | 1.2 (0.70x) |
| `new` symbol grain (378) | 3.6 | 4.3 (1.18x) | 1.1 (0.29x) |
| `new` token, class = identifier (788) | 1.6 | 4.4 (2.60x) | 1.2 (0.71x) |
| `new` file, class = identifier (57 files) | 1.4 | 3.9 (2.78x) | 0.6 (0.44x) |
| `Result` token (1,130) | 3.2 | 5.3 (1.65x) | 1.7 (0.53x) |
| `self` token (7,407) | 17.6 | 14.7 (0.76x) | 8.2 (0.47x) |
| `(` token (41,563) | 87.9 | 39.2 (0.45x) | 40.2 (0.46x) |
| `symbols *`, `symbols fmt`, roll-ups | | unchanged within noise | |

The goal (selective token search within 2x of v1) is met with margin: the worst ratio among the token-grain searches is 0.71x (`new`, class filter) and the densest term, `(`, is 0.46x. Every query returned identical rows from both backends. v1 times differ slightly between the two runs (for example `self` 19.2 vs 17.6 ms); each ratio uses its own run's v1, the v1 column shows the after run. Both runs: 106 files, 478,831 tokens (the 855,726-token / 162-file figures in Result 2 are a different, earlier run).

**Cost.** Logical bytes (values of the `stream` and `post` tables, before page slack): 4,996,694 + 210,088 = 5,206,782 before; 5,041,544 + 577,675 = 5,619,219 after, so +412,437 bytes, +0.86 B/token, +7.9%. The postings account for +367,587 of it (an ordinal is a varint of about 1 byte; the old value was a fixed 8-byte count per `(term, file)` row) and the checkpoints and header for +44,850 (about 0.09 B/token). The redb file size did not change at the granularity redb grows in (16.6 MiB before and after, 36.3 B/token), so the real page-level effect is under one growth step; expect roughly 37 B/token, still under the 40 limit, but this was **not measured** at page level and **not re-run at 9.9 M tokens**. Ingest time is unchanged (0.47 s). Denser postings (block or bitmap ordinals) would recover part of the postings growth if it matters.

**Limits.** The checkpoint spacing (64) was not tuned; only that value was measured. Ordinal reads trust the checkpoints (a full decode verifies them and reports a mismatch as corruption). A term that occurs in most tokens of a file degrades to the previous sequential walk, as `(` shows (0.45x before, 0.46x after, within noise).

## Addendum: churn and vacuum (ADR story 3)

Harness: `crates/graph-store/examples/churn.rs` (`cargo run --release -p graph-store --example churn -- <dir> [rounds]`). It indexes every `.rs` file under a directory with the fallback tokenizer (19 files, 97,993 tokens: this repo's `crates/`), then repeats: replace every file with slightly different content (`reindex`), run `vacuum`, print the file size. One machine, release build; small, so read it as a shape, not a benchmark.

| Round | File before vacuum | After vacuum | Terms removed by vacuum |
|---|---|---|---|
| 0 (first index) | 2.52 MiB | 2.52 MiB | 0 |
| 1 (replace all) | 4.53 MiB | 4.53 MiB | 1 |
| 2 to 8 (replace all, each) | 4.53 MiB | 4.53 MiB | 1 |

What it shows:
- **Steady state, no growth.** After the first full replacement the file stays at 4.53 MiB through seven more full replacements. redb reuses the pages a finished replacement frees, so repeated churn does not grow the file.
- **The one-time step is 1.8x.** Replacing every file in a single write transaction needs the old and the new pages at the same time, so the file grows to about old plus new once and stays there. That is a peak of the transaction size, not a leak. Chunked commits (below) bound it: a smaller cap frees pages between chunks. The step was measured with one chunk per batch (the batch fits under the default 64 MiB cap); it was not re-measured with small caps.
- **`vacuum` does not shrink the file.** It removes dead dictionary terms (here one term per round: the churn marker) but the file stays the same size, as documented. Returning space to the operating system needs a compaction (copy to a new file), which is not built; the ADR gate "soak keeps the file within 1.5x after vacuum" (story 7) is not met by this measurement (1.8x at the peak, then flat) and is not claimed.
- **Not measured:** churn with symbol-bearing (extracted) files.

**Prune-then-vacuum.** Harness: `crates/graph-store/examples/prune_churn.rs` (`cargo run --release -p graph-store --example prune_churn -- <dir> [rounds]`). Same corpus source as `churn.rs` above (this repo's `crates/`), re-walked at measurement time: 26 files now vs. 19 when `churn.rs` was first measured, since the tree has grown across the intervening PRs. Each round prunes down to one file (removing 25 of 26, `Store::prune_files` with a one-file `keep` set and the `ORIGIN_DIRECTORY` origin `prune_files` requires), vacuums, restores the full set, vacuums again.

| Round | After prune, before vacuum | After vacuum (pruned) | Terms removed | After restore + vacuum |
|---|---|---|---|---|
| 1 to 4 (each) | 4.53 MiB | 4.53 MiB | ~3,690 (of ~3,990) | 4.53 MiB |

What it shows: pruning 25 of 26 files removes about 92% of the dictionary's terms (exact counts drift slightly run to run: the corpus is this repo's own `crates/` tree, so it includes the harness's own source), and `vacuum` correctly drops every one of them (`check_consistency`-style: no orphan rows, confirmed separately by the consistency proptest) — but the file stays at 4.53 MiB regardless, the same non-shrinking behavior `churn.rs` already showed for replace. Repeating the prune/restore cycle four times shows no growth and no shrink either way: steady state, not a leak, but confirms **compaction (not vacuum) is the only way to reclaim space after a prune**, even in the extreme case of removing almost the whole store.

## Addendum: term-length policy and chunked commits (ADR story 3)

- **Term-length policy.** The dictionary keeps a term inline as its own key while it is at most `MAX_INLINE_TERM` (256) bytes and does not start with NUL. A longer term, or one starting with NUL, is keyed by `"\0"` plus its SHA-256 (hex), so B-tree keys stay small. The term's full text is stored once, whole, in the reverse table, and spans live in the stream, so no text or span is lost. Every lookup compares the stored text, so a digest collision is probed to the next key (`.n`) and can never merge two terms; the inline and hashed key spaces cannot overlap because inline keys never start with NUL. Symbol names are **not** capped: the symbol index is range-scanned by prefix and needs the text as the key. Tested: terms of 255, 256 and 257 bytes, 300 bytes, 100,000 bytes, multi-byte text, NUL-leading text, a short term shaped like a hashed key, a forged collision, and a 1,024-byte symbol name, all with exact search, symbol search, describe and file-token results equal to v1, plus the fixed-query differential; replace and vacuum of long terms leave no orphan.
- **Chunked commits.** `index_batch` on v2 commits when the source bytes in the current write transaction reach a cap (default 64 MiB, `V2Store::set_chunk_bytes`) and at the end. Atomicity is **per chunk**: a storage error rolls back only the chunk in progress; earlier chunks stay committed and visible; per-file failures never abort a chunk; a batch under the cap is one transaction, all or nothing, as before. A file is never split, so one file above the cap is a chunk of its own. Re-running a failed batch skips stored files by fingerprint. v1 is unchanged (one transaction per batch). Not yet wired to a CLI flag or the `Store` trait, and the "in progress" flag of ADR decision D3 (readers seeing a repo marked in progress between chunks) is not built.
- **Consistency proptest.** After every step of random sequences of ingest (replace), prune, chunked batch and vacuum, an oracle recomputes postings, the symbol index, the dictionary (both directions) and the describe catalog from the streams and requires them to match exactly; after a vacuum the dictionary holds no dead term.

## What this does not show

- No cold-cache numbers, no concurrent readers, no updates or deletes at scale, and no per-table page accounting.
- The 9.9 M set is replicated data (see Method). A run on a real corpus of that size has not been done.
- (As measured before the addendum above; the addendum fixes it on syn, not re-run at 9.9 M.) Selective token-grain queries were slower than v1 on real data (2.7x to 2.9x on syn, 1.5x on the replicated set). The cause is structural: without an index into a stream, finding one token means walking all tokens before it. The design answer is in the ADR already: sparse stream checkpoints (story 19) plus per-term ordinals in the postings. It costs roughly one more varint per token in the postings and a small checkpoint table; the size impact was not measured.
- The `irregular` escape and span derivation (story 2) are not built; sizes here are for the codec that stores all six span fields per record.

## Recommendation (the decision is yours)

The data supports proceeding with v2, conditional on the two items below, on these grounds: the size and ingest gains are large and hold on real data (15.8x smaller and 6.8x faster on syn alone, 24x and 5.1x at 9.9 M replicated), broad and roll-up queries are far faster, and the one regression the QA found (`search_symbols`) is fixed without new tables. Two conditions I would attach, because the ADR's own targets are missed on real data:

1. Treat sparse checkpoints with per-term ordinals (story 19) as required for the checkpoint to pass on real data, not optional: without them a selective token search is 2.7x to 2.9x v1 on `syn`, over the 2x limit. Absolute cost is 8 ms at 855 k tokens and 130 ms at 9.9 M, which may be acceptable for an agent workload, but that is a product call.
2. Re-measure size on a real corpus of about 10 M tokens before accepting: `syn` alone is at 39.95 B/token (the limit) because a smaller data set has a proportionally larger dictionary and fixed cost, and the replicated set's 27.3 B/token is flattered by repetition.

If the 2x query limit is a hard gate for selective token-grain search on real data today, the honest reading of these numbers is **no-go until story 19 lands**, and go after. (Story 19 has since landed and is measured in the addendum; the decision remains yours.)

## Addendum: `children`/`descendants`/`ancestors`, v1 vs v2-before vs v2-after (ADR story 3, slice 3k)

Closing benchmark for slices 3g-3j (per-symbol token ranges): a measured comparison of traversal latency across three states, over this repo's own `crates/` tree (27 `.rs` files, real `RustExtractor`, the same corpus every prior slice in this story used).

- **v1** -- `RedbStore`'s own typed-row implementation (`children` reads a dedicated `children` table directly; `descendants`/`ancestors` are the default `StoreRead` trait walk built on repeated `children`/`parent` calls, one redb read transaction per call).
- **v2-before** -- the eager path every one of these operations used before slice 3g: a full `codec::decode` of the file's whole stream, then a linear walk (`stream_tree`/`file_walk`) or a stream-order parent-chain walk for `ancestors`. Measured directly via `V2Store`'s `#[cfg(test)]` fallback hooks (`children_via_fallback[_file]`, `descendants_via_fallback[_file]`, and a new hook `ancestors_via_fallback` added for this slice, which reproduces `ancestors`' pre-3g body verbatim -- slice 3g's PR removed the eager path outright with no fallback kept, unlike `children`/`descendants`), called on the *same* indexed store and the *same* node ids as "v2-after", so this is apples-to-apples on identical data rather than a separate build from an older commit (the simpler of the two options the scoping plan allowed).
- **v2-after** -- today's default: the range-based path (slices 3h-3j) where `Lazy::ranges_dense()` is true, falling back verbatim to the eager path otherwise.

**Method.** Test `graph_store::v2_tests::traversal_latency_v1_vs_v2_before_vs_v2_after_on_this_repos_own_corpus` (`cargo test -p graph-store --release traversal_latency -- --nocapture`; committed, run in `--release`). Both stores are built from the same 27 real Rust files. Ids are sampled per backend (each backend assigns its own ids over the same source, so the sample is chosen independently, not id-for-id): every indexed file (up to 10), plus the 5 symbols with the largest subtree (`descendants().len()`), the 5 with the smallest, and the 5 with the deepest ancestor chain (25 ids per backend). Each id is timed p50 over 15 repetitions (matching this document's own convention for a corpus this size), then summed per operation across the sampled ids to report one aggregate number per operation.

| operation | v1 ms | v2-before ms | v2-after ms | v2-after / v1 |
|---|---|---|---|---|
| `children` | 0.226 | 7.527 | 0.393 | 1.74x |
| `descendants` | 1,375.042 | 13.710 | 498.578 | 0.36x |
| `ancestors` | 0.163 | 6.301 | 0.311 | 1.91x |

**Acceptance criterion ("v2 at or better than v1, or the gap is documented with a named cause").** Mixed, reported honestly:

- **`descendants`: met.** v2-after (498.6 ms) is 2.8x *faster* than v1 (1,375.0 ms) in aggregate. This is not primarily v2's range optimization winning on its own merits -- it is dominated by a real v1 weakness on this corpus: v1 has no override for `descendants`, so it uses the generic `StoreRead` default (recursive `children` calls, one fresh redb read transaction per node), which is effectively O(n) transactions for an n-node subtree. On the largest sampled symbol/file subtrees this is markedly more expensive than either v2 path.
- **`children` and `ancestors`: not met, gap documented.** v2-after costs 1.74x and 1.91x of v1 respectively. Named cause: v1's `children`/`parent` are O(1) typed-row reads from a dedicated `children` table; v2 must open a read transaction and decode at least the file's symbol section (`Lazy`/`with_lazy`) even on the fast, range-based path, which is a heavier constant per-call cost than a typed row get. In absolute terms both sides are sub-millisecond in aggregate over 25 ids (0.39 ms and 0.31 ms for v2-after), so this is architecture-level, not a practical latency problem at this corpus size; it matches the same v1-wins-at-small-grain shape already recorded for story 4's file/repo/org-grain search numbers above.
- **v2-after vs v2-before, `children`/`ancestors`: the optimization holds**, 19-20x and 20x faster than the pre-3g/3i eager path respectively, consistent with the per-decode-record measurements in the story 3 row.
- **v2-after vs v2-before, `descendants`: a real, named regression on this corpus.** v2-after (498.6 ms) costs *more* than v2-before (13.7 ms) here. Cause: `descendants_ranged_file` calls `descendants_ranged` once per top-level symbol in the file, and each of those calls independently rebuilds the whole file's symbol-to-children map from scratch via `with_lazy` -- so the cost is quadratic in the file's top-level symbol count, whereas the eager `file_walk` fallback builds that tree exactly once for the whole file. This dominates the aggregate for files with many top-level symbols and is worth a follow-up slice (not built here, this PR is measurement-only); tracked for a future story-3 slice or issue.

Raw numbers above are reproducible by re-running the committed test; they will drift slightly (this repo's own source changes over time) but the shape -- v1 wins small-grain `children`/`ancestors`, v2-after beats v2-before except for `descendants` -- should hold.
