# Spike: data model (epic story 18 precursor)

Status: measurements for [ADR 0003](../adr/0003-data-model.md) (revised after architecture, dev and QA review). Date 2026-09-19. Author: solution-architect review.

**TL;DR.** Today a token costs ~525 B of tree pages because every occurrence is a ~248 B JSON node, not because of repeated text. A dictionary + per-file compact stream + `(term,file)` postings prototype measured ~20 B/token (about 25x smaller, order of magnitude), 3-5x faster ingest, 20-370x faster roll-ups of very common terms, with exact spans round-tripping on all 241,638 corpus tokens. Separately, every CLI call spends 134 ms in an O(tokens) `describe`. Details follow; raw data is in `spikes/data-model/` (code, README, logs).

**Baselines.** Models A/E/P were measured against `main` at commit `293bb2a`. The skip-unchanged work was measured on branch commit `47e8616`; it has since been merged to main as `231109a` (PR #8). Re-baseline the A numbers after that merge if they matter (the fingerprint string is now in the file node).

**Labels.** Every figure is **[M] measured** unless marked **[E] estimated**. Where a number is an estimate the method is stated next to it. Nothing in `crates/` or `Cargo.lock` changed; the prototype is committed as reference code under `spikes/data-model/` and is **not** part of the workspace build (see "Method" and `spikes/data-model/README.md`).

## 0. Summary

| Question | Answer |
|---|---|
| Where does the space go today? | Not in token text (2.2% of the JSON bytes of a token node: average text 5.53 B of 247.8 B). It goes into the per-occurrence node envelope: ~248 B of JSON per token (six span fields with key names, kind, parent, class), ~230 B of B-tree page slack on top, plus a `children` entry and a `tokens_by_text` entry per token. [M] |
| Current cost | 525 B of tree pages per token (560 B of file per token); 5.2 GB of pages for 9.9 M tokens. [M] |
| Best measured alternative | Interned dictionary + one compact stream per file + `(term,file)` postings with per-occurrence ordinals in the prototype (tokens are not nodes): 20 B of pages per token, **about 25x smaller** (order of magnitude; 9.9 M-token set); ingest 5.0x faster at 1x and 4.3x at 9.9 M, but the prototype omits validation, `origin`, `has_errors` and prune and its own throughput degrades 827 k -> 549 k tok/s from the 4x to the 41x set, so read "~3-5x" as the honest range; roll-ups of very common terms 20-370x faster (9.9 M set, `(`: file 179x, repo 340x, org 370x) and 19-23x at token/symbol grain. Headline multiples mix data sets: size is 9.9 M, ingest is 1x-41x, latency is 9.9 M unless stated. A's 41x search numbers are 2 repetitions. Spans stay exact (round-trip verified on all 241,638 corpus tokens). [M] |
| Does a stop-list of very common terms help? | Yes but modestly: -22% size, -12% ingest time, at the price of 25-50x slower file/repo/org roll-ups for the stopped terms (scan). The big win is the representation, not the stop-list. [M] |
| Cheaper intermediate | Binary per-token nodes (still one node per token): 96 B/token pages, 5.5x smaller than today, trivial migration, but 4.6x bigger than the stream model and keeps the O(tokens) costs. [M] |
| A finding independent of the model | Every CLI `search` and `symbols` call runs `Store::describe` (full node scan) to validate flags: 134 ms of the ~215-261 ms wall time of a CLI call on the 241 k-token corpus DB (the rest is process start and the query); in-process `describe` is ~0.54 s at 4x and ~7.7 s at 9.9 M tokens, before any query work. [M] |

## 1. Method

**Machine.** Intel Core Ultra 9 275HX (24 threads), 30 GB RAM, NVMe, btrfs (`/var/home`), Linux 7.2.0 (Fedora), rustc 1.94.1, redb 2.6.3, `--release`. Single process, single writer. Timings are wall clock. Other jobs were running on the machine (about 19 GB RAM in use), so treat differences under ~15% as noise.

**Data.**
- *Corpus (1x):* `testdata/corpus`, 8 repos, 641 files, 1,659,936 source bytes, 241,638 tokens, 741 symbols (only the 37 Rust files yield symbols). Indexed through the real store at commit `293bb2a` (main) with the release CLI and, for comparability, through a spike binary that calls the same `Store::index_batch`.
- *Scaled sets (~1 M and ~9.9 M tokens):* the corpus replicated 4x and 41x. Copies 1..n rename **rare** identifiers (corpus frequency <= 50 and length >= 4) by appending the copy number, and go to repo `<repo>-<k>` under org `org<k mod 10>`. Common tokens (punctuation, keywords, frequent names) stay shared. This is a synthetic stand-in: it keeps the shape of the frequency distribution but real repos overlap more in rare identifiers than these copies do (so the dictionary here is larger than reality, **[E]** direction only). Distinct texts: 11,629 (1x), 41,656 (4x, 4.3%), 412,008 (41x, 4.2%).
- 1 M = 966,552 tokens in 2,564 files (32 repos); 10 M = 9,907,158 tokens in 26,281 files (328 repos). One write transaction per repo, as `memory-graph index` does.

**Models measured.**
- **A** (current): the real `graph-store` (`nodes` JSON, `names`, `children`, `tokens_by_text`, `symbols_by_name`).
- **E**: per-token nodes, but binary (varint) records, term ids instead of text, same `children` multimap, `e_toks` (term id -> node id).
- **P** (prototype of B/C/D): entities (org, repo, file, symbol) as binary rows; per-file token stream; dictionary (`text -> id` and `id -> text`); postings table keyed `(term id, file id)` -> count + ordinal deltas. Symbols keep their own rows with contiguous ids per file (range read per file). Variants by which terms get postings:
  - **P all postings** = model C.
  - **P stop-256**: the 256 most frequent texts have no postings; searches for them scan file streams = model D.
  - **P scan only** (no postings at all) = model B (dictionary + streams, search by scanning). At 10 M this row is really "stop-100,000": the ~312 k rarest texts still have postings, so its size is a few MB above a true no-postings model.
- Stream codec (P): per token `varint(term<<4 | class<<1 | irregular)`, `varint(gap<<1 | newline_before)`, and, only after a newline, `line_delta` and `col-1`. Byte length and end column are derived from the dictionary text; `irregular` (multi-line tokens, non-ASCII column drift) stores explicit end line/column/length. **Round trip verified**: all six span fields, class and text of every one of the 241,638 tokens decode identically (`spike verify`).
- Correctness cross-check: for 66 query/grain/filter cases, row counts and hit counts of P equal the real store's (same output for both).

**What each figure measures.**
- *File bytes*: `ls -l` after ingest. redb grows the file in large steps and its copy-on-write leaves free space, so this is coarse (identical for different P variants). *Tree pages*: `(leaf_pages + branch_pages) * 4096` from redb table stats, summed over all tables: the steady-state size after compaction. *Logical*: redb `stored_bytes + metadata_bytes` (payload without page slack).
- *Latencies*: in-process, warm page cache, p50/p95 over N repetitions (N=30 corpus, 10-20 at 1 M, 2-10 at 10 M for P, **2 for A at 10 M** because a single A query takes up to 11 s; p95 = max of 2 there). "Cold" = the file's pages dropped with `dd oflag=nocache` before a fresh process.
- *Peak RSS*: `VmHWM` of the process. redb keeps a page cache (1 GiB by default, **[E]** from the redb docs, not re-verified here) so RSS during ingest of a big DB is dominated by that cache, not by the workload.
- *IO*: `/proc/self/io` `wchar`, `write_bytes`, `read_bytes`.

**Reproduction.** The spike is committed at `spikes/data-model/spike.rs` (~500 lines: corpus loader, scaler, codec, model E, prototype P, benchmarks). It needs the `graph-store` crate at commit `293bb2a` plus two extra dev-dependencies; see `spikes/data-model/README.md`. It is not built by the workspace. Raw 1 M and 10 M benchmark logs are in `spikes/data-model/logs/`; the corpus-level (1x) search table in section 7.1 and the process-level CLI/cold-cache timings (7.4, 7.5) were not saved as log files.

**Pure-Rust gate.** The spike needs two dev-dependencies (not added to the workspace), `sha2` (already in the in-flight fingerprint PR) and `miniz_oxide` (deflate, used only to measure compressibility). `python3 scripts/check-no-c-deps.py` passes with both ("checked 88 dependencies, pure-Rust gate passed"). No other new crate is proposed.

## 2. The data: token frequency (corpus, [M])

241,638 tokens, 1.66 MB of source, **11,629 distinct texts (4.8% of occurrences)**, average token text 5.53 bytes, 120 multi-line tokens (block comments, strings).

| Top-N distinct texts | % of all occurrences |
|---|---|
| 10 | 44.3% |
| 100 | 70.9% |
| 1,000 | 87.9% |
| 10,000 | 99.3% |

- 5,552 texts (47.7% of distinct texts) occur exactly once; they are 2.3% of occurrences.
- Top texts: `(` 8.06%, `)` 8.05%, `.` 6.26%, `;` 5.50%, `=` 3.40%, `,` 2.90%, `{` 2.62%, `:` 2.62%, `}` 2.61%, `$` 2.27%; then `public` 0.80%, `using` 0.76%, `new` 0.73%, `var` 0.60%, `string` 0.60%, `if` 0.53%, `return` 0.45%. Rank 100 = `headers` (207), rank 500 = `Time` (32), rank 2000 = a 33-character identifier (7).
- Breakdown by class (occurrences): identifier 100,043 (41.4%), punctuation 104,346 (43.2%), operator 25,843 (10.7%), literal 4,031 (1.7%), comment 7,375 (3.1%), **keyword 0** and other 0. By majority class over distinct texts: identifier 6,619, literal 1,802, comment 3,167, operator 15, punctuation 26.
- Consequences: (1) the user's hunch is right about the distribution (the 100 most common texts are 71% of all tokens); (2) the dictionary is small: 414,017 bytes of text for 11,629 entries; (3) **no extractor currently emits the `keyword` class** (the fallback tokenizer labels keywords as identifiers, the Rust extractor uses the same tokenizer), so `search --kind keyword` returns nothing in every model. That is a separate bug or gap; the design keeps the class per occurrence (3 bits) so it does not assume class is a function of text.
- Identical-content files: 2 groups of duplicates among 641 files, 4,976 redundant bytes (0.3%). The corpus is a poor test for blob sharing (see ADR); real monorepos and vendored trees have far more.

## 3. Current model (A): where the bytes go [M]

Corpus, after `memory-graph index` of 8 repos (8 write transactions):

| Table | Entries | Payload (stored+meta) | Tree pages | Per token |
|---|---|---|---|---|
| `nodes` (JSON) | 243,029 | 64.2 MB | 118.1 MB | 489 B |
| `children` (multimap u64->u64) | 243,028 | 2.0 MB | 3.8 MB | 15.7 B |
| `tokens_by_text` (multimap text->u64) | 241,638 | 2.6 MB | 4.7 MB | 19.5 B |
| `names`, `symbols_by_name`, `meta` | ~1.4 k | <0.1 MB | 0.2 MB | |
| **Total** | | **68.8 MB** | **126.8 MB** | **525 B** |
| **DB file** | | | **135.3 MB** | **560 B** |

- A token node is **247.8 B of JSON on average** (org 151, repo 162, file 223, symbol 247). Field names (`"start_line"`, `"end_col"`, `"token_class"`, ...), a 20-digit-capable id and parent, and `null`/default fields dominate; the token text is 2.2% of it (5.53 B average text / 247.8 B).
- `nodes` payload is 64.2 MB (stored + metadata) but occupies 118 MB of pages: **54 MB (46%) is B-tree slack** (values of ~250 B in 4 KiB pages; `fragmented_bytes` in redb stats). Compaction (`Database::compact`) reclaims little of it (9.9 M tokens: 6.45 GB -> 5.85 GB file; 1x and 4x: nothing).
- 1.6 MB of source becomes a **135 MB** file: **81x the source size**.
- Existing spike (`docs/spikes/storage.md`) measured ~690 B/token on a single file; consistent.

## 4. Size [M unless marked E]

Bytes per token use each set's token count (241,638 / 966,552 / 9,907,158).

| Set | Model | DB file | Tree pages | Payload | Pages B/token | File B/token | vs A (pages) |
|---|---|---|---|---|---|---|---|
| 1x | A | 135.3 MB | 126.8 MB | 68.8 MB | 525 | 560 | 1x |
| 1x | E | 34.2 MB | 22.7 MB | 11.7 MB | 94 | 142 | 5.6x smaller |
| 1x | P all postings (C) | 17.4 MB (9.6 MB after compact) | 4.8 MB | 3.2 MB | 19.9 | 72 (40) | 26x |
| 1x | P stop-256 (D) | 9.0 MB | 3.8 MB | 2.6 MB | 15.7 | 37 | 33x |
| 1x | P scan only (B) | 9.0 MB (6.5 MB after compact) | 3.0 MB | 2.1 MB | 12.4 | 37 (27) | 42x |
| 4x (0.97 M) | A | 539.5 MB | 508.9 MB | 275.8 MB | 526 | 558 | 1x |
| 4x | E | 135.3 MB | 91.1 MB | 47.0 MB | 94 | 140 | 5.6x |
| 4x | P all (C) | 34.2 MB | 19.2 MB | 12.7 MB | 19.9 | 35.4 | 26x |
| 4x | P stop-256 (D) | 34.2 MB | 15.0 MB | 10.3 MB | 15.5 | 35.4 | 34x |
| 4x | P scan only (B) | 24.8 MB (compacted) | 11.9 MB | 8.3 MB | 12.3 | 25.6 | 43x |
| 41x (9.9 M) | A | 6,451 MB (5,855 MB compacted) | 5,241 MB | 2,850 MB | 529 | 651 (591) | 1x |
| 41x | E | 1,078 MB | 953 MB | 492 MB | 96 | 109 | 5.5x |
| 41x | P all (C) | 270 MB | 206 MB | 133 MB | 20.8 | 27.3 | 25x |
| 41x | P stop-256 (D) | 270 MB | 160 MB | 109 MB | 16.2 | 27.3 | 33x |
| 41x | P stop-100k ("scan only") | 270 MB | 140 MB | 95 MB | 14.1 | 27.3 | 37x |

File-size caveat: at 4x and 41x the three P variants show the same file size because redb grew the file by the same step; use the pages/payload columns to compare them.

**Where P's bytes go at 9.9 M tokens** (pages): dictionary 76 MB (`text->id` 35 MB plus `id->text` 41 MB, 412 k terms), postings 79 MB (1.95 M `(term,file)` entries, ~41 B each in pages), streams 40 MB (**4.0 B/token**, exact spans), entities 7 MB (26,281 files + 21,061 symbols as reported by the spike; 741 symbols x 41 copies would be 30,381 and the difference was not reconciled, so treat symbol counts and symbol-grain row counts at this scale as unverified; the 7 MB is dominated by files), names 4 MB. Observations:
- The stream itself is **3.13 B/token** on the corpus (756,432 B for 241,638 tokens, exact spans, class and term id). Per-file `deflate(6)` (miniz_oxide) shrinks the streams 3.0x to **1.05 B/token** (253,732 B). Whole-file `gzip -6` on the 4x DBs: A 539.5 MB -> 40.0 MB (13.5x), E not run, P 34.2 MB -> 6.5 MB (5.3x). Gzipped A (40.0 MB) is 1.2x the uncompressed P file (34.2 MB) and 2.1x its tree pages (19.2 MB); gzipped A vs gzipped P (6.5 MB) is 6x. A's redundancy is enormous, and compression alone gets it only to about P's uncompressed size.
- Dictionary and postings are the largest P components at scale, and both are **inefficiently stored in the prototype** (two copies of every term text; one postings row per `(term,file)` with ~19 B fixed overhead for typical 3-5 occurrence lists). **[E]** A single sorted dictionary plus block-encoded postings (one row per term per ~4 KiB block, delta-coded file ids) should reach roughly 8-12 B/token total; this was not built, so treat it as a target, not a result.
- A stop-list of 256 terms removes 46 MB of postings at 9.9 M tokens (-22% of P's pages).

## 5. Ingest [M]

Single-threaded, extraction included (the Rust extractor for `.rs`, the fallback tokenizer elsewhere), one commit per repo.

| Set | Model | Time | files/s | tokens/s | source MB/s | Peak RSS | Bytes written (`wchar`) | Written / final file | Largest single-commit file growth |
|---|---|---|---|---|---|---|---|---|---|
| 1x | A | 1.36-1.41 s (3 runs) | 455-471 | 172-178 k | 1.18-1.22 | 83 MB | - | - | - |
| 1x | E | 1.24-1.26 s | 509-516 | 192-194 k | 1.32-1.34 | 30 MB | - | - | - |
| 1x | P all | 0.27-0.28 s | 2,299-2,390 | 866-901 k | 5.95-6.19 | 20 MB | - | - | - |
| 1x | P stop-256 | 0.24 s | 2,666-2,689 | 1.01 M | 6.9 | 19 MB | - | - | - |
| 1x | P scan only | 0.19-0.20 s | 3,200-3,460 | 1.2-1.3 M | 8.3-9.0 | 18 MB | - | - | - |
| 4x | A | 6.19 s | 414 | 156 k | 1.10 | 294 MB | 586 MB | 1.09 | 270 MB |
| 4x | E | 5.34 s | 480 | 181 k | 1.27 | 69 MB | 160 MB | 1.18 | 67 MB |
| 4x | P all | 1.17 s | 2,193 | 827 k | 5.81 | 42 MB | 73 MB | 2.13 | 17 MB |
| 4x | P stop-256 | 1.03 s | 2,480 | 935 k | 6.57 | 41 MB | 57 MB | 1.66 | 17 MB |
| 4x | P scan only | 0.84 s | 3,046 | 1.15 M | 8.07 | 39 MB | 38 MB | 1.11 | 17 MB |
| 41x | A | 76.8 s | 342 | 129 k | 0.93 | 1,181 MB | 7,447 MB | 1.15 | 2,156 MB |
| 41x | E | 67.0 s | 392 | 148 k | 1.07 | 640 MB | 2,675 MB | 2.48 | 539 MB |
| 41x | P all | 18.0 s | 1,457 | 549 k | 3.97 | 290 MB | 1,581 MB | 5.86 | 135 MB |
| 41x | P stop-256 | 15.9 s | 1,652 | 623 k | 4.51 | 280 MB | 1,314 MB | 4.87 | 135 MB |
| 41x | P stop-100k | 14.5 s | 1,810 | 682 k | 4.94 | 266 MB | 1,021 MB | 3.78 | 135 MB |

- Three repetitions at 1x gave <=4% spread (A: 1.36/1.38/1.41 s). 4x and 41x are single runs (each costs minutes): treat +-10% as noise.
- Bytes written per token at 41x: A 751 B, E 270 B, P 160 B, P stop-256 133 B. Bytes written are `wchar` (bytes passed to write(2)), not device bytes, so they exclude btrfs/NVMe amplification and count cached rewrites. The write-amplification ratio (bytes written / final size) of P is high (up to 5.9x) because the commits rewrite the same hot pages (dictionary and postings B-trees) once per repo transaction; in absolute bytes it is still 4.7x less than A. Larger batches reduce it. **[E]**
- P's ingest is not extractor-bound at this scale (>=1 M tokens/s including tokenizing); A is bound by JSON serialization plus three B-tree inserts per token.
- Real CLI (release, `memory-graph index` per repo, corpus): 0.01-0.51 s per repo, 1.59 s in total for 8 processes, peak RSS 10-57 MB per process.
- RSS during A ingest at 41x (1.18 GB) is the redb page cache filling up; P at 290 MB. Cache size is configurable (`Builder::set_cache_size`), so RSS is a tuning knob for all models; it is not per-token memory.

## 6. Re-index, delete, skip-unchanged [M]

| Operation | Corpus DB (A) | 4x A | 41x A | 4x P all | 41x P all | 41x P stop-256 |
|---|---|---|---|---|---|---|
| Re-index largest file (45,897 B, 5,065 tokens) | 56 ms | 73 ms (p95 83) | 98 ms (p95 150) | 9.6 ms (p95 12) | 14.0 ms (p95 17) | 10.6 ms |
| Delete that file | 27 ms | 39 ms | 52 ms | 5.3 ms | 8.0 ms | 4.9 ms |
| Re-index smallest file (31 B, 12 tokens) | 2.1 ms | 2.6 ms | 2.5 ms | 1.4 ms | 2.0 ms | 1.4 ms |
| Delete smallest file | 2.1 ms | 2.9 ms | 2.6 ms | 1.4 ms | 1.9 ms | 1.3 ms |
| Skip-unchanged decision (hash + lookup) | see below | | | 12 us (large) / 2 us (small) | 11 us / 2 us | 12 us / 2 us |

- 20 repetitions for re-index, 5 for delete, 200 for skip. Every write includes one durable commit (fsync), which is the ~1.3-2.5 ms floor.
- A's "delete" here is `prune_files` (the only delete in the API today; it also scans the repo's files). A dedicated delete would be somewhat faster, not 5x.
- **Skip-unchanged on the in-flight branch** (`skip-unchanged-files`, commit `47e8616`, release CLI, corpus, 8 repos = 8 processes): first index 1.66 s; unchanged re-index of all 641 files **0.115-0.131 s in total** (three runs), about 15 ms per process including process start (14x faster). The store reads the file node (JSON) and compares a `sha256:<hex>|<lang>|<extractor version>|<format>` string. P stores the digest as 32 raw bytes in a binary row: the decision cost is 2-12 microseconds of hash plus one point read (**the hash time depends on SHA-NI; on CPUs without it the hash is ~300-500 MB/s, [E]**).
- Cost scales with tokens of the file for A (JSON delete of every token node, its `children` and `tokens_by_text` entries) and with distinct terms of the file for P (delete stream, delete one posting row per distinct term).

## 7. Search, symbols, describe, open [M]

Percentiles are p50 / p95 in ms, in-process, warm. Counts are hit counts; rows are the rows returned at that grain. "P scan only" is the no-postings model (at 10 M, stop-100k).

### 7.1 Corpus DB (241 k tokens): selected rows
(Raw logs are in `spikes/data-model/logs/`; the 1 M and 10 M tables below use the same cases.)

| Case | A | P all |
|---|---|---|
| `(` token grain (19,481 rows) | 37.7 / 39.2 | 11.0 / 14.7 |
| `(` symbol grain (1,153 rows) | 128.7 / 129.9 | 7.8 / 7.8 |
| `(` file grain (575 rows) | 23.9 / 25.1 | 0.25 / 0.26 |
| `(` repo grain (8 rows) | 22.4 / 23.7 | 0.13 / 0.13 |
| `(` org grain (1 row) | 22.0 / 22.9 | 0.12 / 0.12 |
| `public` token grain (1,930 rows) | 2.7 / 3.2 | 3.9 / 4.0 |
| `public` symbol grain (481 rows) | 80.1 / 83.8 | 3.7 / 3.7 |

### 7.2 ~1 M tokens (966,552)

Terms: `(` = 77,924 occurrences, `public` = 7,720, `Time` (rank ~500 in the corpus) = 32, a rare identifier = 8.

| term (occurrences) | grain | filter | result rows | A | P all postings | P stop-256 | P scan only |
|---|---|---|---|---|---|---|---|
| very common punct `(` (77924) | token | none | 77924 | 172 / 621 | 52.9 / 59.9 | 48.9 / 50.2 | 49.1 / 50.1 |
| very common punct `(` | token | lang=rust | 22732 | 127 / 130 | 12.2 / 13.6 | 11.2 / 12.0 | 10.7 / 11.1 |
| very common punct `(` (77924) | symbol | none | 4036 | 535 / 626 | 31.9 / 32.2 | 32.2 / 32.6 | 32.6 / 32.8 |
| very common punct `(` (77924) | file | none | 2300 | 112 / 114 | 1.1 / 1.1 | 29.8 / 30.2 | 30.3 / 31.5 |
| very common punct `(` | file | lang=rust | 144 | 99.9 / 101 | 0.515 / 0.52 | 5.4 / 5.5 | 5.5 / 5.7 |
| very common punct `(` (77924) | repo | none | 32 | 104 / 106 | 0.626 / 0.638 | 24.5 / 24.7 | 25.6 / 26.1 |
| very common punct `(` (77924) | org | none | 4 | 106 / 108 | 0.588 / 0.601 | 23.3 / 23.9 | 23.8 / 24.3 |
| common ident `public` (7720) | token | none | 7720 | 16.2 / 17.1 | 16.0 / 16.3 | 24.1 / 24.3 | 24.7 / 25.1 |
| common ident `public` | token | lang=rust | 0 | 10.9 / 11.4 | 0.399 / 0.414 | 3.9 / 3.9 | 3.9 / 4.0 |
| common ident `public` (7720) | symbol | none | 1924 | 326 / 329 | 14.8 / 15.0 | 23.4 / 25.7 | 23.8 / 24.0 |
| common ident `public` (7720) | file | none | 1924 | 13.3 / 13.5 | 0.897 / 0.91 | 23.1 / 23.3 | 23.4 / 23.7 |
| common ident `public` | file | lang=rust | 0 | 10.4 / 11.6 | 0.398 / 0.405 | 3.9 / 3.9 | 4.0 / 4.3 |
| common ident `public` (7720) | repo | none | 24 | 10.9 / 11.6 | 0.51 / 0.519 | 21.7 / 22.0 | 22.4 / 24.3 |
| common ident `public` (7720) | org | none | 4 | 10.9 / 11.4 | 0.487 / 0.507 | 21.5 / 21.7 | 22.6 / 24.5 |
| mid (rank~500) `Time` (32) | token | none | 32 | 0.053 / 0.127 | 0.608 / 0.63 | 0.598 / 0.617 | 21.7 / 22.5 |
| mid (rank~500) `Time` (32) | symbol | none | 30 | 12.5 / 13.0 | 0.6 / 0.613 | 0.597 / 0.623 | 22.3 / 23.0 |
| mid (rank~500) `Time` (32) | file | none | 30 | 0.052 / 0.083 | 0.012 / 0.014 | 0.013 / 0.016 | 21.6 / 22.6 |
| mid (rank~500) `Time` (32) | repo | none | 2 | 0.042 / 0.045 | 0.009 / 0.01 | 0.009 / 0.011 | 23.7 / 24.1 |
| mid (rank~500) `Time` (32) | org | none | 1 | 0.042 / 0.045 | 0.009 / 0.01 | 0.009 / 0.01 | 21.7 / 23.6 |
| rare (count 2) `\"    \"` (8) | token | none | 8 | 0.019 / 0.052 | 0.091 / 0.098 | 0.092 / 0.097 | 23.3 / 24.0 |
| rare (count 2) `\"    \"` (8) | symbol | none | 4 | 0.018 / 0.02 | 0.106 / 0.111 | 0.106 / 0.109 | 22.5 / 26.4 |
| rare (count 2) `\"    \"` (8) | file | none | 4 | 0.018 / 0.021 | 0.004 / 0.005 | 0.004 / 0.005 | 22.2 / 22.7 |
| rare (count 2) `\"    \"` (8) | repo | none | 4 | 0.018 / 0.019 | 0.004 / 0.004 | 0.004 / 0.004 | 21.7 / 21.9 |
| rare (count 2) `\"    \"` (8) | org | none | 4 | 0.018 / 0.019 | 0.004 / 0.004 | 0.004 / 0.004 | 21.7 / 22.2 |


### 7.3 ~9.9 M tokens (9,907,158)

Terms: `(` = 798,721 occurrences, `public` = 79,130, rank-500 term = 32, rare = 82. **A: 2 repetitions.**

| term (occurrences) | grain | filter | result rows | A | P all postings | P stop-256 | P scan only |
|---|---|---|---|---|---|---|---|
| very common punct `(` (798721) | token | none | 798721 | 11261 / 11261 | 588 / 691 | 583 / 704 | 588 / 702 |
| very common punct `(` | token | lang=rust | 233003 | 9732 / 9732 | 135 / 137 | 134 / 135 | 135 / 136 |
| very common punct `(` (798721) | symbol | none | 39593 | 9064 / 9064 | 397 / 406 | 403 / 420 | 407 / 410 |
| very common punct `(` (798721) | file | none | 23575 | 2563 / 2563 | 14.3 / 14.7 | 379 / 382 | 381 / 383 |
| very common punct `(` | file | lang=rust | 1476 | 2407 / 2407 | 5.7 / 6.1 | 65.1 / 65.8 | 65.0 / 65.5 |
| very common punct `(` (798721) | repo | none | 328 | 2451 / 2451 | 7.2 / 8.0 | 322 / 324 | 326 / 327 |
| very common punct `(` (798721) | org | none | 10 | 2409 / 2409 | 6.5 / 6.5 | 296 / 299 | 301 / 326 |
| common ident `public` (79130) | token | none | 79130 | 370 / 370 | 212 / 212 | 315 / 321 | 315 / 317 |
| common ident `public` | token | lang=rust | 0 | 150 / 150 | 4.3 / 5.4 | 45.7 / 46.7 | 45.4 / 46.4 |
| common ident `public` (79130) | symbol | none | 19721 | 5016 / 5016 | 194 / 198 | 298 / 305 | 301 / 308 |
| common ident `public` (79130) | file | none | 19721 | 181 / 181 | 11.2 / 11.9 | 292 / 295 | 295 / 301 |
| common ident `public` | file | lang=rust | 0 | 149 / 149 | 4.3 / 4.5 | 45.0 / 45.3 | 45.3 / 45.7 |
| common ident `public` (79130) | repo | none | 246 | 166 / 166 | 5.7 / 6.3 | 278 / 280 | 281 / 282 |
| common ident `public` (79130) | org | none | 10 | 158 / 158 | 5.3 / 5.8 | 275 / 278 | 277 / 279 |
| mid (rank~500) `Time` (32) | token | none | 32 | 0.249 / 0.249 | 0.666 / 0.921 | 0.67 / 0.893 | 273 / 277 |
| mid (rank~500) `Time` (32) | symbol | none | 30 | 17.9 / 17.9 | 0.661 / 0.668 | 0.668 / 0.69 | 278 / 384 |
| mid (rank~500) `Time` (32) | file | none | 30 | 0.094 / 0.094 | 0.013 / 0.02 | 0.013 / 0.019 | 720 / 731 |
| mid (rank~500) `Time` (32) | repo | none | 2 | 0.051 / 0.051 | 0.01 / 0.016 | 0.01 / 0.016 | 368 / 681 |
| mid (rank~500) `Time` (32) | org | none | 1 | 0.049 / 0.049 | 0.01 / 0.01 | 0.01 / 0.012 | 310 / 335 |
| rare (count 2) `\"    \"` (82) | token | none | 82 | 1.3 / 1.3 | 0.851 / 1.2 | 0.862 / 1.1 | 274 / 327 |
| rare (count 2) `\"    \"` (82) | symbol | none | 41 | 0.221 / 0.221 | 1.0 / 1.1 | 1.1 / 1.1 | 282 / 284 |
| rare (count 2) `\"    \"` (82) | file | none | 41 | 0.215 / 0.215 | 0.031 / 0.042 | 0.031 / 0.038 | 276 / 278 |
| rare (count 2) `\"    \"` (82) | repo | none | 41 | 0.212 / 0.212 | 0.03 / 0.037 | 0.031 / 0.037 | 275 / 279 |
| rare (count 2) `\"    \"` (82) | org | none | 10 | 0.204 / 0.204 | 0.026 / 0.028 | 0.026 / 0.028 | 275 / 277 |


Reading the tables:
- **Roll-up grains are where A hurts.** A walks every token of the term and decodes JSON ancestors: `(` at org grain takes 2.4 s at 9.9 M tokens, P with postings 6.5 ms (370x), because file/repo/org counts come from the posting rows without touching token positions.
- **Token and symbol grain for very common terms** are bound by materializing 800 k rows. P 588 ms vs A 11.3 s (19x); symbol grain 397 ms vs 9.1 s (23x). At 41x the P cost is dominated by decoding the streams of 23,575 files and by building the result map.
- **Rare and mid-frequency terms** are sub-millisecond to ~1 ms in both A and P; A is a little faster for a rare token-grain query (`tokens_by_text` gives the node in one lookup, P decodes the file stream). Not a differentiator.
- **Stop-list cost:** stopping the top 256 terms makes their file/repo/org grain searches scan every stream: ~24-30 ms at 1 M, ~280-380 ms at 10 M (linear in tokens), versus 0.5-14 ms with postings. Still 7-9x faster than A at 10 M, and rare-term searches are unaffected (0.03-1 ms).
- **No postings at all** (scan only): every search costs 22 ms at 1 M and ~275 ms at 10 M regardless of the term (rare included). That is a floor of ~28 ns per token; viable up to a few million tokens, not beyond.
- Language filter: with postings P applies `lang=rust` after reading posting rows; when the term is absent from that language it exits quickly (0.4-4 ms). A applies the filter after decoding.
- `class=keyword` returns nothing anywhere (see section 2); the P cost with a class filter includes decoding the streams of every file with a posting (13 ms for `public` at 1 M).

### 7.4 Symbols, describe, open, process-level

| | A corpus | A 4x | A 41x | P 4x | P 41x |
|---|---|---|---|---|---|
| `symbols new` exact (in-process p50) | 0.023 ms | 0.09 ms | 3.9 ms (370 hits) | 0.024 ms | 0.28 ms |
| `describe` (in-process p50) | 134 ms (p95 137) | 542 / 552 ms | 7,736 ms | 0.36 / 0.41 ms | 4.0 / 4.7 ms |
| Open (first, in-process) | 1.9 ms (P: 1.3 ms) | 19.5 ms | 7.5-12.2 ms | 7.1 ms | 1.9 ms |

- **`describe` is O(tokens) in A** (it iterates every node to count tokens per language and class); P computes it from the file rows (O(files)): 1,900x faster at 9.9 M tokens.
- **CLI overhead (release CLI, corpus DB, 15 runs, p50 / p95 wall):** `search '('` 261 / 280 ms, `search '(' --grain org` 245 / 259 ms, `search Rebus --grain file` 218 / 224 ms, `symbols new` 215 / 221 ms, `describe` 217 / 227 ms; peak RSS 167 MB for all of them. The in-process query itself takes 0.09-38 ms, so **134 ms of every ~215-261 ms CLI call is the `store.describe` validation** at the top of `search` and `symbols` (`crates/graph-cli/src/main.rs`, `validate_filters`). This is separate from the model choice but proportional to token count and should be fixed first (e.g. keep per-file counts on the file node; or validate lazily on an empty result).
- Open time is 1-20 ms for every model; the run-to-run spread (P 1.3 ms vs 7.1 ms vs 14 ms for identical files) is larger than any model difference. Schema check plus symbol index check; no O(N) work unless the symbol index must be rebuilt (a migration case).

### 7.5 Cold cache and memory during search (fresh process, one query)

| Query | Model | Cold | Warm | Peak RSS | Bytes read cold |
|---|---|---|---|---|---|
| 4x `(` token grain | A | 1,241 ms | 827 ms | 372 MB | 549 MB |
| | P all | 121 ms | 80 ms | 39 MB | 30 MB |
| 4x `(` file grain | A | 942 ms | 755 ms | 299 MB | 549 MB |
| | P all | 36 ms | 13.5 ms | 7 MB | 15 MB |
| 4x `Rebus` token grain (4,600 rows) | A | 923 ms | 31 ms | 32 MB | 53 MB |
| | P all | 96 ms | 28 ms | 13 MB | 24 MB |
| 41x `(` token grain | A | 7,650 ms | 12,743 ms | 1,901 MB | 5,321 MB |
| | P all | 3,393 ms | 858 ms | 353 MB | 197 MB |
| 41x `(` org grain | A | 6,885 ms | 7,506 ms | 1,208 MB | 5,317 MB |
| | P all | 308 ms | 17.6 ms | 13 MB | 23 MB |
| 41x `Rebus` org grain | A | 7,486 ms | 245 ms | 250 MB | 515 MB |
| | P all | 168 ms | 13 ms | 9 MB | 15 MB |

- The "warm" A rows at 41x still read 5 GB because the 6.4 GB file does not stay in the OS cache next to the other data on this machine.
- A cold single-term search reads 50-500 MB (random 4 KiB pages of scattered JSON nodes); P reads 15-24 MB (the dictionary path, posting rows, the streams of the hit files). The 15 MB floor in P is the entity table plus dictionary pages.
- The p256/p100k prototypes are excluded from process-level rows: the spike re-computes the stop-list at startup, which would add ~30 ms.
- Search memory in both models is O(rows returned): a `BTreeMap` of result rows (800 k token rows = 353 MB in P, 1.9 GB in A, where JSON `Hit` strings are cloned per row). Neither the model nor the redb cache is the limit; result streaming with `--limit` push-down is (see ADR).

## 8. Complexity of `describe`, roll-up, containment in P (design notes, not measured)

- **Containment path** org > repo > file > symbol > token: file row -> repo row -> org row by `parent` (unchanged); symbol rows keep `parent` (symbol or file); a token's parent is *derived*: the innermost symbol whose span contains the token start, found by the same stack sweep the ingest uses (`ingest_into`), or by binary search over the file's symbol list (read as one key range).
- **Token identity:** a token is `(file id, ordinal)`; pack into a `u64` if the public `NodeId` must stay one number. `parent(token)` costs one file-symbol range read.

## 9. Not measured / limits of this spike

- The prototype is a spike: no `Store` API integration, no schema/version handling, error paths, overlap checks (`InvalidSpan`), `origin`, `has_errors` handling on read, or `prune`.
- `E` (binary per-token nodes) was measured for size and ingest only; its search latency was not built (**[E]**: about A's minus JSON decoding, i.e. same shape, maybe 1.5-2x faster for node-walking cases).
- Single run for 4x/41x ingest; A at 41x searches use 2 repetitions.
- Synthetic scaling (rare identifiers renamed per copy, uniform distribution across copies; the distinct-text ratio is understated for real multi-repo data because real repos add vocabulary, while the rare-identifier part may be overstated). Real distinct-text ratios vary by language mix; 4-5% (corpus and both scaled sets) is only what we can say for this corpus.
- Cold-cache uses `dd oflag=nocache` (page-cache eviction of that file), not a drop of all caches; btrfs compression is not enabled on this volume, so sizes are logical file sizes.
- The corpus is mostly C#/Java/TypeScript; only Rust gets symbols (741), so symbol-grain behavior with dense symbol data (e.g. 100 k symbols) is untested. In P symbols are rows in the same table with per-file ranges, so I expect no surprise, but it is unmeasured.
- Multi-writer, concurrent readers during writes, and crash safety were not tested (redb provides the transaction semantics in every model).
- Query results were compared for equality between A and P on this corpus only.

## 10. Decisions taken after this spike (by the user)

Recorded in [ADR 0003](../adr/0003-data-model.md): scale above 100 M tokens and horizontal scaling must be supported (so packed dictionary and block postings are core, and the store must be shardable); pre-1.0 re-index is acceptable but migration must exist and be tested before 1.0; readers get point-in-time snapshots. Consequence for this spike: its [E] estimates for 100 M and for the packed dictionary/block postings are now on the critical path and must be replaced by measurements (ADR stories 5 and 6).
