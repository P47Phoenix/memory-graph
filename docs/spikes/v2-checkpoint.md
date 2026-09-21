# Spike: v2 store checkpoint measurements (ADR 0003 story 4)

Status: measurements for the story 4 go/no-go input. Date 2026-09-21. This document presents numbers and a recommendation; **it does not decide go/no-go, the user does.** ADR 0003 stays Proposed.

**TL;DR.** At 9,876,231 tokens (public corpus plus the `syn` crate, replicated 9x) the v2 store is 24x smaller than v1 (257.5 MiB vs 6,256.5 MiB, 27.3 vs 664 bytes per token), ingests 5.1x faster (15.2 s vs 76.9 s), and no measured query is more than 1.51x slower than v1 (the limit is 2x); broad queries are 4x to 1000x faster. The `search_symbols` regression of issue #22 is fixed (4.7x slower before, 0.7x to 0.9x now, 0.14x for the full listing). On the **real, unreplicated** `syn` crate alone (855,726 tokens) two numbers are worse: the size is exactly at the 40 bytes per token limit (40.0), and a selective token-grain term such as `new` is 2.7x to 2.9x slower than v1 (8 ms vs 3 ms), because v2 walks the whole token section of every candidate file. The recommendation is at the end.

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

Size and ingest are the same before and after (this change does not touch the write path): v1 514.5 MiB (630.5 B/token), ingest 5.6 s; v2 32.6 MiB (**40.0 B/token**, at the 40 B limit), ingest 0.82 s (6.8x faster). After redb compaction v2 was 24.6 MiB in the run of this change and 32.6 MiB (unchanged) in the run on main; the compaction result varied between runs and is not relied on.

| Query | v1 ms | v2 before ms (x v1) | v2 now ms (x v1) |
|---|---|---|---|
| `search_symbols new` (42) | 0.1 | 1.6 (15.1x) | 0.1 (0.86x) |
| `search_symbols new*` (66) | 0.2 | 2.0 (12.3x) | 0.1 (0.69x) |
| `search_symbols fmt` (793) | 2.2 | 5.8 (2.67x) | 0.5 (0.23x) |
| `search_symbols *` (14,888) | 50.6 | 33.1 (0.65x) | 6.9 (0.14x) |
| `search_symbols * --limit 100` | 49.0 | 33.4 (0.68x) | 1.0 (0.02x) |
| `(` token (74,673) | 167.5 | 113.7 (0.68x) | 76.3 (0.46x) |
| `self` token (13,512) | 34.5 | 35.2 (1.02x) | 25.3 (0.72x) |
| `new` token (1,237) | 3.0 | 9.9 (3.36x) | 8.1 (**2.71x**) |
| `Result` token (1,834) | 6.9 | 11.8 (1.70x) | 9.4 (1.40x) |
| `new` file, class = identifier (88 files) | 2.4 | not run | 7.2 (**2.96x**) |
| `(` token `--limit 100` | 160.0 | 106.7 (0.67x) | 0.4 (0.00x) |

(The issue #22 figures of 4.7x for `search_symbols` and 1.4x for token search came from QA's own query set; the terms here are the harness's fixed set, so the ratios differ.)

## What changed and why

1. **`search_symbols`.** v2 already had a name index (`symbols_by_name`, name to symbol id). The cost was decoding the entire stream (every token record) of every file with a hit, then a dictionary read per name. Now it decodes only the header and the symbol section (symbols come first in the stream, so no layout change), caches dictionary texts and org/repo rows for the query, filters at file level before reading any stream, and reads files in sorted-by-path order. No new table and no size change.
2. **Term search.** The token section is walked record by record without collecting it, and only the matching records are kept; roll-ups that need no symbol names skip the enclosing-symbol lookup. This helped broad terms (`(` 114 to 76 ms, `self` 35 to 25 ms at syn scale) but not selective ones: the walk itself is about 12 ns per token and every token of every candidate file is walked (roughly 600,000 tokens for `new` on syn, an estimate from the 88 candidate files).
3. **`--limit` push-down.** Candidate files are sorted by (org, repo, path); the walk stops at the first file group boundary after the limit is met, because every row of a later group sorts after every row of an earlier one (a group is the file at token and symbol grain, the repo at repo grain, the org at org grain). Result identical to the full walk (differential over limits 0 to 5 at every grain).
4. **Traversal.** `children`, `descendants` and `ancestors` on `StoreRead` (see the ADR story 4 row for the semantics), v1 and v2 checked against each other.

## What this does not show

- No cold-cache numbers, no concurrent readers, no updates or deletes at scale, and no per-table page accounting.
- The 9.9 M set is replicated data (see Method). A run on a real corpus of that size has not been done.
- Selective token-grain queries are still slower than v1 on real data (2.7x to 2.9x on syn, 1.5x on the replicated set). The cause is structural: without an index into a stream, finding one token means walking all tokens before it. The design answer is in the ADR already: sparse stream checkpoints (story 19) plus per-term ordinals in the postings. It costs roughly one more varint per token in the postings and a small checkpoint table; the size impact was not measured.
- The `irregular` escape and span derivation (story 2) are not built; sizes here are for the codec that stores all six span fields per record.

## Recommendation (the decision is yours)

Go on v2 as the storage direction, on these grounds: the size and ingest gains are large and hold on real data (15.8x smaller and 6.8x faster on syn alone, 24x and 5.1x at 9.9 M replicated), broad and roll-up queries are far faster, and the one regression the QA found (`search_symbols`) is fixed without new tables. Two conditions I would attach, because the ADR's own targets are missed on real data:

1. Treat sparse checkpoints with per-term ordinals (story 19) as required for the checkpoint to pass on real data, not optional: without them a selective token search is 2.7x to 2.9x v1 on `syn`, over the 2x limit. Absolute cost is 8 ms at 855 k tokens and 130 ms at 9.9 M, which may be acceptable for an agent workload, but that is a product call.
2. Re-measure size on a real corpus of about 10 M tokens before accepting: `syn` alone is at 40.0 B/token (the limit) because a smaller data set has a proportionally larger dictionary and fixed cost, and the replicated set's 27.3 B/token is flattered by repetition.

If the 2x query limit is a hard gate for selective token-grain search on real data today, the honest reading of these numbers is **no-go until story 19 lands**, and go after.
