# Spike: how much of a read is decode? (read cache phase 0)

ADR 0008 (read cache), epic story 45. Before we build a cross-query read
cache, measure whether decoding stored bytes is a large enough share of
query time to be worth caching. The ADR's gate: go on to phase 2 only if
decode share is >= 25% at warm single-reader, or >= 25% at any of 8, 16 or
32 readers, where decode share is the sum of a reader's decode timers over
the sum of its query wall time, median across readers.

**Delivery: partial, see #233.** This spike delivers the in-process counters,
the benchmark and the numbers. The graph-server metrics export, a
store-side query wall-time counter, decode byte counters and the cache-bypass
rate (meaningless until a cache exists) are tracked in #233.

## Method

- **Counters.** `graph_store::read_stats` counts:
  - reverse-dictionary block decodes made by queries (`dict_rev_lookup`'s
    read call site; the write side, such as `dict_rev_append` extending a
    block, is not counted);
  - per-query term-text memo hits and misses;
  - lazy stream-header decodes (`codec::decode_lazy`) and symbol-section
    decodes (`Lazy::symbols`);
  - full stream decodes (`codec::decode`, counted once rather than also as
    lazy plus symbols);
  - read transactions opened by `StoreRead` calls on a `V2Store`.

  Decodes done by writes and maintenance (removing a file's content,
  `vacuum`) are not counted. Reads through a `V2Snapshot` count their decodes
  but **not** a read transaction, because the snapshot opened its
  transaction once, up front; `read_txns` therefore undercounts a server
  that serves reads from snapshot handles.
- **Two views.** `read_stats::snapshot()` is process-wide: every store and
  thread adds to it, so it is not attributable to one store.
  `read_stats::thread_snapshot()` is the calling thread's own totals; tests
  use it to assert exact counts under the parallel test runner, and the
  benchmark uses it to get each reader's own decode share.
- **Timing.** Optional per-category nanoseconds, turned on with
  `read_stats::set_timing(true)`, a process-global toggle. Timing is off by
  default, so the production cost is one relaxed atomic add, one
  thread-local add and two thread-local loads per event.
- **Benchmark.** An `#[ignore]` test, `crates/graph-cli/tests/readbench.rs`.
  It replaced the `readbench` example
  (`cargo run --release -p graph-cli --example readbench -- <workdir>
  testdata/corpus 20 1048576`), which took the numbers below with its cold
  phase in the same process. How to run:

  ```sh
  # the vendored corpus (testdata/corpus)
  cargo test --release -p graph-cli --test readbench measure_reads -- --ignored --nocapture
  # the fetched big corpus (scripts/fetch-bench-corpus.py): index once ...
  MG_READBENCH_CORPUS=D:/tmp/bench-corpus MG_READBENCH_WORKDIR=D:/tmp/a5-readbench \
    cargo test --release -p graph-cli --test readbench measure_reads -- --ignored --nocapture
  # ... then re-measure without re-indexing
  MG_READBENCH_REUSE=1 MG_READBENCH_WORKDIR=D:/tmp/a5-readbench \
    cargo test --release -p graph-cli --test readbench measure_reads -- --ignored --nocapture
  ```

  - Other inputs: `MG_READBENCH_WORKDIR` (default a temp dir, removed
    afterwards), `MG_READBENCH_REPS` (default 20, at least 1),
    `MG_READBENCH_COLD_CACHE` (bytes, default 1 MiB), and
    `MG_READBENCH_MAX_FILES` / `MG_READBENCH_MAX_BYTES` to stop indexing
    early. `MG_READBENCH_REUSE=1` needs a fixed `MG_READBENCH_WORKDIR` and
    ignores the caps; if the database or workload is missing it warns and
    indexes afresh.
  - The walk matches `index_dir`'s (`graph_cli::dir_walker`: the repo's
    `.gitignore` rules, `.git` skipped, non-UTF-8 paths skipped, the same
    binary check and size cap). The only difference: it also skips
    `target` and `node_modules` directories. It streams the corpus in
    batches of 256 files / 32 MiB.
  - Columns added since the numbers below: separate `lazy` and `sym`
    decode shares, `store ms/q` (wall time inside `StoreRead` calls),
    `KiB/q` (encoded bytes decoded per query) and `bypass`. Timing columns
    read `n/a` in the timing-off row.
  - The workload's words and `file_tokens` paths come from the first 8 MiB
    of each repo, so on a big tree a "cold" term is rare in the sample, not
    necessarily in the corpus. The workload is saved next to the database,
    so a reuse run and the cold child measure the same queries.
  - The cold phase runs in a **fresh process** (the test binary re-run with
    `--exact readbench_cold_phase`), so the redb cache starts empty. The OS
    page cache stays warm unless it is dropped by hand (Linux:
    `sync; echo 3 > /proc/sys/vm/drop_caches`); the test does not do it.
  - The `bypass` column stays 0 until phase 2 adds a cache.
  - Indexes the vendored corpus with **every shipped extractor**
    (`graph_cli::shipped_extractors()`): 795 files, 335k tokens, a 16.6 MiB
    db.
  - Runs 153 queries per pass:
    - 6 hot terms (the most frequent identifiers) and 18 cold terms
      (frequency 3), each searched at token, symbol, method, class and file
      grain with `limit 50`, plus a `term*` `search_symbols`;
    - 8 `file_tokens` calls;
    - 1 `describe`.
  - Phases:
    - **Cold redb cache (OS cache warm):** a fresh open with a 1 MiB redb
      cache, one pass. The OS file cache is still warm, so this is not a
      cold disk.
    - **Warm:** 20 repeated passes after a warm-up pass, once with timing
      off and once with it on.
    - **Concurrent:** 1, 8, 16 and 32 reader threads on one store, 20 passes
      each.
  - **Decode share** is computed per reader (its decode nanoseconds over the
    sum of its per-query latencies) and reported as the median across
    readers, as the ADR defines it. The category columns are over all
    readers.
  - The share is measured **with timing on**, so the `Instant::now` calls
    around each decode are inside the decode nanoseconds and may raise the
    share slightly (the overhead range below bounds the effect).
  - **Timing overhead** is measured over 5 alternating off/on warm runs and
    reported as a range.

## Numbers

From one Windows 11 desktop, release build, two runs (the second in
brackets where it differs by more than a point).

| phase | p50 ms | p95 ms | qps | decode share (median) | dict | lazy+sym | full | dict blocks/q | lazy/q | sym/q | txns/q |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cold redb cache (OS cache warm), 1 MiB | 0.080 | 2.04 | 2515 | 23.2% [26.1%] | 21.7% [24.5%] | 1.3% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |
| warm x20, timing on | 0.050 | 1.19 | 4118 | 37.4% | 35.2% | 2.0% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |
| 1 thread x20 | 0.049 | 1.20 | 4132 | 36.7% | 34.4% | 2.0% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |
| 8 threads x20 | 0.067 | 1.89 | 21644 | 36.8% | 34.4% | 1.8% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |
| 16 threads x20 | 0.139 | 3.24 | 24496 | 35.9% | 33.7% | 1.7% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |
| 32 threads x20 | 0.211 | 4.98 | 31141 | 38.1% | 36.3% | 1.5% | 0.2% | 22.6 | 3.4 | 3.4 | 1.00 |

- **Overhead.** Turning timing on cost -1.1% to +1.2% of warm wall time
  over 5 trials (median +0.4%; the second run: -0.8% to +1.2%, median
  +0.1%), which is within noise. Earlier single-trial runs saw anything from
  +3.3% to -23.3%, which is why the range is now measured. One relaxed
  counter bump measured 1.4 ns.
- **Results unchanged.** Answers with timing on equal those with timing off
  (`read_stats_tests::timing_fills_the_nanos_only_when_on_and_never_changes_answers`),
  and the graph-store suite passes with the counters in place.
- **Why this differs from the first, Rust-only run** (17-20%, 7.4 dict
  blocks per query): with every extractor registered, most corpus files
  have symbols, so symbol, method and class grain queries resolve far more
  names (22.6 dictionary block decodes per query instead of 7.4).

## Conclusion

- **Gate: met on this corpus.** Warm single-reader decode share is about
  37%, and 36-38% at 8, 16 and 32 readers, all above the 25% gate. Cold
  (small redb cache) is 23-26%, around the gate, because redb page reads
  take a larger part.
- **Downward biases** (the true share is, if anything, higher):
  - token-stream walks (`Lazy::tokens`, `tokens_at`) are not timed, because
    their timing would include the per-token query callback;
  - the corpus is small (16.6 MiB, fits in RAM).
- **Upward risk:** the corpus is small enough that redb's page cache holds
  all of it; a 10 GB-class index would spend more time in page reads, which
  lowers the decode share. Re-run on a large multi-language index before
  closing phase 2, and again after phase 1, as the ADR requires.
- **The dominant hot spot is the dictionary.** Hot spot 1, `dict_rev_lookup`,
  decodes a whole reverse-dictionary block, up to 64 KiB, to read one id. It
  accounts for about 94% of all decode time in this run (85-90% in the
  Rust-only run), one block decode per memo miss.
- **The other decodes are small.** Lazy and symbol decodes (hot spot 2) are
  about 2%, and full decodes (hot spot 4) are under 0.5%. Read transactions
  (hot spot 3) are exactly one per call. Their cost is not timed here; it
  sits in the non-decode remainder.
- **Recommendation for ADR 0008.** Phase 1's dictionary work (decode only the
  one string a lookup needs, story 46) is the target: it removes most of the
  measured decode time without the invalidation cost of a stream cache. The
  gate must then be re-evaluated with this benchmark, because phase 1
  removes most of what made it pass here; a general decoded-object cache
  (phase 2) is justified only if the share is still >= 25% after that.

## Phase 1 re-run (story 46, 2026-10-05)

Phase 1 replaced the whole-block decode in `dict_rev_lookup` with an
in-place scan (`codec::dict_block_find`): every entry is still walked and
validated (so a corrupt block is refused exactly as before), but only the
matching string is allocated. A new counter, `dict_strings_decoded`, rises by
exactly one per uncached lookup; `dict_block_decodes` now counts block scans.
The encoding is unchanged (no schema bump).

Same machine, same command, both builds run back to back in one session
(before = `read-cache-phase0-measure` at 18b81a8, after = this branch). The
benchmark opens the store with an explicit size (`None` = redb's 1 GiB for
the warm rows, 1 MiB for the cold row), so the new derived page-cache
default does not affect these numbers.

| phase | p50 ms before | p50 ms after | qps before | qps after | decode share before | decode share after | dict share before | dict share after |
|---|---|---|---|---|---|---|---|---|
| cold redb cache, 1 MiB | 0.076 | 0.047 | 2756 | 3432 | 25.3% | 14.3% | 23.7% | 12.0% |
| warm x20, timing on | 0.049 | 0.029 | 4243 | 6282 | 37.4% | 23.5% | 35.2% | 20.3% |
| 1 thread x20 | 0.050 | 0.028 | 4212 | 6237 | 37.4% | 23.3% | 35.2% | 20.0% |
| 8 threads x20 | 0.068 | 0.036 | 23318 | 36521 | 37.3% | 18.7% | 35.1% | 15.7% |
| 16 threads x20 | 0.149 | 0.076 | 25111 | 39006 | 36.2% | 12.1% | 34.0% | 9.6% |
| 32 threads x20 | 0.235 | 0.111 | 31784 | 52878 | 37.9% | 11.4% | 36.0% | 9.2% |

- Warm single-reader throughput rose about 48% (4243 to 6282 qps) and p95
  fell from 1.14 to 0.80 ms; 32 readers went from 31.8k to 52.9k qps.
- Counts per query are unchanged (22.6 dictionary lookups, 3.4 lazy and
  symbol decodes, 1 read transaction), as they must be: answers did not
  change, only the cost of each lookup.
- Timing overhead after: +0.2% to +3.0% (median +1.2%) over 5 trials.

**Gate after phase 1: not met on this corpus.** Warm single-reader decode
share is 23.3-23.5%, and 18.7%, 12.1% and 11.4% at 8, 16 and 32 readers, all
below the 25% gate. The remaining decode time is still mostly the dictionary
scan (one block walk per memo miss, validating every entry's UTF-8); lazy
and symbol decodes are 2-3%. On this evidence phase 2 (the decoded-object
cache, story 47) is a no-go unless the large-index re-run (#233) shows a
higher share; a cheaper next step, if one is wanted, is a per-block offset
index or skipping UTF-8 validation of non-matching entries, which stays
within phase 1's no-invalidation scope.
## Go / no-go

- **Phase 0 gate:** GO on the vendored corpus (warm single-reader 37%,
  8/16/32 readers 36-38%, gate 25%), subject to the large-index re-run in
  #233.
- **Re-evaluated after phase 1 (2026-10-05):** NOT MET (warm single-reader
  23.4%, 8/16/32 readers 18.7%/12.1%/11.4%, gate 25%); see the phase 1
  re-run above.
- **Large-index re-run (A5, 2026-10-06):** phase 2 NO-GO (decode share
  0.2%), phase 3 GO, conditional on real-usage confirmation (repeats 40.5-75.2%, synthetic session); see
  "A5 gate decision" below, which carries the sign-off line.

## Caveats

- **One machine.** Treat the shares as rough figures; the two runs agreed
  within about a point on every warm and concurrent row.
- **Not yet measured:** query wall time inside the store, decode bytes, and
  the bypass rate with a concurrent indexer (#233).

## Large-index re-run (A4, issue #233)

The vendored `testdata/corpus` is too small to settle the decode share at scale, so the ADR 0008 re-run uses a large public corpus fetched by `scripts/fetch-bench-corpus.py`. Nothing is vendored: the script shallow-fetches 31 permissively licensed public repos (MIT, Apache-2.0, BSD, PostgreSQL, Apache-2.0 with the LLVM exception), each pinned to an exact commit, into a directory outside the repo. Together they cover Rust, C, C++, Go, Java, Scala, C#, F#, JavaScript, TypeScript, Python, SQL, shell, R, Haskell, Elixir, GDScript, COBOL, RPG, assembly, HTML and Razor. RPG is covered only thinly (OSSILE and noxDB, a few MB of RPG); there is no large permissively licensed RPG codebase on GitHub that we found.

- **Disk:** about 7.2 GB of working tree (an estimate from the manifest) plus the shallow `.git` packs, which are roughly the same size again; budget 15 GB.
- **Submodules are not populated** (rust and dotnet-runtime have some), so those parts are absent.
- **Extractors:** Python, Java, Kotlin, TypeScript and Perl have no extractor, so they are indexed as tokens only (no symbols).
- The closing summary is on-disk bytes by file extension, not what gets indexed; the share under "other" is printed explicitly.
- Visibility and licence are checked with `gh api` before anything is fetched (fails closed, like `vendor-corpus.py`; `--skip-verify` opts out). A repo is skipped only when HEAD is at its pinned sha and `.git/mg-bench-ok` says so; anything else is wiped and re-fetched. A failed fetch is retried, reported, and makes the exit status non-zero, while the others continue.

```sh
python3 scripts/fetch-bench-corpus.py --list                         # the manifest: name, sha, licence, size, languages
python3 scripts/fetch-bench-corpus.py --dest D:/tmp/bench-corpus        # fetch everything (resumable)
python3 scripts/fetch-bench-corpus.py --dest D:/tmp/bench-corpus --only go,tokio
```

To run the read benchmark against it, index the directory and point the benchmark at it with `MG_READBENCH_CORPUS=D:/tmp/bench-corpus` (each direct subdirectory is one repo; see the benchmark how-to under Method, and the A5 re-run below).

## Large-index re-run (A5, 2026-10-06)

### Setup

- **Corpus:** the 31 repos of `scripts/fetch-bench-corpus.py` at their pinned commits, with licence verification on. All 31 fetched on the first run. 6.4 GB on disk including the shallow `.git` packs; 5.35 GB / 584k files of working tree. By extension (bytes): other 48%, C# 9.4%, C++ 9.0%, Go 5.7%, C 5.1%, assembly 4.2%, TypeScript 4.0%, C/C++ headers 2.8%, Rust 2.8%, Java 2.4%, Scala 2.1%, JavaScript 1.4%, Python 1.0%, F# 0.8%, and HTML, SQL, Elixir, Haskell, shell, R, Perl, COBOL, Razor, RPG, Kotlin and GDScript each under 0.5%.
  - The script's closing summary crashed on Windows on a path longer than MAX_PATH (in aspnetcore) after every repo had been fetched. It is fixed in this PR with a `\\?\` prefix.
- **Machine:** Windows 11 Pro, AMD Ryzen 9 7950X (16 cores / 32 threads), 63 GB RAM (about 38 GB free at the start), NVMe SSD (D:). Release build of origin/main at 0cb5ea2, built with `-j 2`.
- **Method:** the A3 benchmark (`readbench.rs`), unchanged, with no caps:
  1. Run 1 indexed the corpus and measured.
  2. Run 2 (`MG_READBENCH_REUSE=1`) re-measured the same database and workload.
- **Commands:**

  ```sh
  RUST_MIN_STACK=268435456 CARGO_TARGET_DIR=D:/tmp/a5-target MG_READBENCH_CORPUS=D:/tmp/bench-corpus \
    MG_READBENCH_WORKDIR=D:/tmp/a5-readbench2 MG_READBENCH_REUSE=1 \
    cargo test --release -p graph-cli --test readbench measure_reads -- --ignored --nocapture
  ```

  The runs used `D:\mg-bench-corpus` and `D:\mg-target-a5`. Since then the corpus has moved to `D:\tmp\bench-corpus` and the build dir has been deleted, per the D:\tmp rule; the command above uses the current paths.
- **Stack overflow:** the first indexing attempt died after 48 minutes with `STATUS_STACK_OVERFLOW` on the benchmark's indexing thread, which is the test harness's default 2 MiB thread. Setting `RUST_MIN_STACK` (256 MiB) got past it. Tracked in #245.
  - Fixed in #245. The trigger was rustc's `tests/ui/parser/survive-peano-lesson-queue.rs`: 2005 nested calls, which `syn` recursed into. `memory-graph index` hit it too, on its 2 MiB parse workers.
  - The Rust extractor now runs `syn` on an internal thread. The thread's stack is sized from the token count of the file's largest top-level item, at 12 KiB per token in release, measured with margin. So no input can overflow it, whatever its shape.
  - An item that would need more than 2 GiB of stack is stored tokens only, with a warning. A cheap pre-scan does the same earlier for nesting deeper than 256 or a chain longer than 1024 links. The CLI's parse workers also get 16 MiB stacks.
  - Re-run on 2026-10-07 with `RUST_MIN_STACK` unset: a release `memory-graph index` ran over all 31 repos, one run per repo into one db. It completed: 573,569 files, 877.3M tokens, 2,462 s, a 22,803 MiB db.
  - The internal parse thread cost no indexing throughput. testdata/corpus took 0.41 s with and without it, and the rust repo 44.7-46.6 s against 44.8-45.3 s, with identical symbols.
  - Rust files degraded by the depth guard: 4, all rustc stress tests. Spans already degraded files the same way through the #203 path: 6 in rust and 1 in llvm-project.
- **Index:**
  - 572,360 files indexed (4,790.8 MiB; 10,399 skipped), 877.1M tokens.
  - Took 2,741 s (45.7 min).
  - The db is 22,532 MiB: 4.7x the indexed source, 26.9 B/token.
- **Memory:** the benchmark's working set stayed around 1.3 GB while indexing. Nothing was killed and no caps were used.

### Numbers

Run 2 (reuse). Run 1 agreed within 2% on every timing column and exactly on every share and count.

| phase | p50 ms | p95 ms | qps | decode share (median) | dict | lazy | sym | full | store ms/q | KiB/q | dict blocks/q | dict strings/q | lazy/q | sym/q | txns/q |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| cold redb cache, fresh process (OS cache warm), 1 MiB (informational; not a gate row) | 0.450 | 519.2 | 14 | 0.1% | 0.1% | 0.0% | 0.0% | 0.0% | 72.9 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| warm x20, timing off | 0.068 | 206.0 | 33 | n/a | n/a | n/a | n/a | n/a | n/a | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| warm x20, timing on | 0.069 | 206.3 | 33 | 0.2% | 0.1% | 0.1% | 0.0% | 0.0% | 30.2 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| 1 thread x20 | 0.070 | 207.5 | 33 | 0.2% | 0.1% | 0.1% | 0.0% | 0.0% | 30.3 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| 8 threads x20 | 0.082 | 282.6 | 191 | 0.2% | 0.1% | 0.1% | 0.0% | 0.0% | 41.8 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| 16 threads x20 | 0.110 | 438.5 | 247 | 0.2% | 0.1% | 0.1% | 0.0% | 0.0% | 64.3 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |
| 32 threads x20 | 0.151 | 755.8 | 285 | 0.2% | 0.1% | 0.1% | 0.0% | 0.0% | 109.3 | 1028.1 | 18.9 | 18.9 | 9.1 | 9.1 | 1.00 |

- **Timing overhead:** -0.5% to +0.4% (median -0.1%) over 5 trials. In run 1: -0.6% to +0.3%, median +0.2%.
- **What dominates:** on 877M tokens the per-pass time is dominated by a few heavy queries. The six hot terms (`const`, `this`, `return`, ...) at token and file grain walk huge postings and token streams: p95 is about 200 ms against a p50 of 0.07 ms, and 1 MiB of encoded bytes is decoded per query on average.
  - The decode counts per query are close to the small corpus's: 18.9 dictionary lookups, 9.1 lazy and 9.1 symbol decodes.
  - But the store spends 30 ms per query, so their time is a rounding error.
  - Most of the time sits in what is not decode-timed: token-stream walks (`Lazy::tokens`, untimed by design, see above), posting reads and redb page access.
- **The decode share is uninformative at this scale.** Untimed posting and token-stream walks for the hot terms dominate the wall time (p95 about 206 ms), so any timed decode is a rounding error beside them. The real hot spot is those walks, tracked as #246 (hot-term posting/token walks).
- **What a cache would buy:** the gate's share is wall-time weighted, so the heavy queries set it. A decoded-object cache would not shorten them. It saves only the timed decodes, which are 0.2% of the time.

### Caveats

- One machine, one corpus. The OS page cache was warm, so the cold row is a cold redb cache, not a cold disk.
- The workload's terms are sampled from the first 8 MiB of each repo. Six hot terms over a corpus this size make a pass heavy-tailed; a workload of only rare terms would raise the share, but it would not be what the gate measures.
- `RUST_MIN_STACK` was raised for the benchmark process. It is no longer needed since #245; see the stack overflow note above.

## Scripted-session repeats (A5)

### Method

`scripts/agent-session-sim.py` drives `memory-graph --server <addr> mcp` over stdio like a coding agent and scrapes `mg_queries_total` and `mg_query_exact_repeats_total` before and after the session. Its seeding calls (listing repos, collecting symbol names) are excluded. The MCP tools on a `--server` target issue Store RPCs, so the server's counters see everything an agent's calls cost.

- **Run it on an idle server.** Seeding calls and any other traffic count as prior requests in the server's 60 s repeat window. Run the simulator against an otherwise idle server, at least 60 s after any earlier session. The script now waits `--settle` seconds (default 61) after seeding, before its first scrape. The measured runs below predate that wait, so in their first minute seeding calls could count as earlier requests. The effect is small against 7,811 RPCs, and it errs upward.
- **Wall-clock window.** The 60 s window is wall-clock, so the share also depends on latency: slower queries or longer think time push earlier calls out of the window.
- **Setup:** `memory-graph --db D:/tmp/a5-readbench2/readbench.redb serve --listen 127.0.0.1:7321 --metrics-listen 127.0.0.1:9321`, on the A5 index above.
- **Session:**
  - 3,000 MCP calls with seed 7.
  - Think time between calls is exponential, mean 0.3 s. The session took about 60 minutes, because hot searches on this index are slow, so the 60 s repeat window was exercised.
  - Calls are grouped into tasks of 6 to 25 calls on one repo and one seed symbol.
- **Mix of actions:**
  - 22% re-ask: a verbatim repeat of an earlier call, 80% from the task's last 8 calls, 20% from the session's last 300 calls (a re-ask drawn on a task's first step goes to the other actions in their mix);
  - 15% `find_symbols` (an exact name, or a `prefix*` 20% of the time; repo-scoped half the time);
  - 15% `file_outline`, which sometimes navigates on to a symbol in the outline;
  - 15% `file_tokens` on a 40/80/150-line window;
  - 25% `search` (token grain mostly, else symbol, method or file; refined with an org/repo filter 40% of the time);
  - 4% `list_files`;
  - 4% `describe`;
  - 10% of steps move to another file of the same repo.
- **Assumptions (synthetic):** the 22% re-ask rate and the action mix are guesses at agent behaviour, not measurements. A sensitivity run with no explicit re-asks (`--reask 0`, 1,000 calls, seed 11) is given beside the main run.

### Results

| RPC | queries | repeats | share | `--reask 0`: queries | repeats | share |
|---|---|---|---|---|---|---|
| Search | 944 | 278 | 29.4% | 295 | 53 | 18.0% |
| SearchSymbols | 1,247 | 543 | 43.5% | 388 | 146 | 37.6% |
| FileTokens | 538 | 285 | 53.0% | 204 | 114 | 55.9% |
| Describe | 1,936 | 1,780 | 91.9% | 619 | 569 | 91.9% |
| Children | 1,918 | 1,759 | 91.7% | 682 | 631 | 92.5% |
| Roots | 1,228 | 1,228 | 100% | 443 | 443 | 100% |
| **all read RPCs** | 7,811 | 5,873 | **75.2%** | 2,631 | 1,956 | **74.3%** |
| query RPCs only (Search, SearchSymbols, FileTokens) | 2,729 | 1,106 | **40.5%** | 887 | 313 | **35.3%** |

- **Client-side cross-check:** at the MCP-call level, 1,054 of 3,000 calls (35%) repeated a call made within 60 s. With `--reask 0` it was 275 of 1,000 (27.5%).
- **Lookup RPCs:** `Describe`, `Roots` and `Children` are lookups the MCP layer makes to resolve org, repo and path on nearly every call, and they repeat almost always. They are real RPCs that a result cache would answer, but they inflate the overall share. The query-RPC-only line is the conservative figure.
- **Natural repeats:** even with no explicit re-asks the session repeats itself. A task keeps returning to the same symbol and file, and different tools resolve the same names.

## A5 gate decision (owner to sign)

| Phase | Gate | Measured (A5) | Verdict |
|---|---|---|---|
| Phase 2: decoded-object cache (stories 47-48) | decode share >= 25% at warm single reader, or at any of 8/16/32 readers | 0.2% warm single reader; 0.2% at 8, 16 and 32 readers (0.1% cold, informational) | **NO-GO** |
| Phase 3: result cache (story 49) | exact repeats >= 20% on a representative agent workload | 75.2% of all read RPCs; 40.5% of query RPCs (35.3% with no explicit re-asks); synthetic scripted session | **GO (conditional)** |

- Phase 2 fails at both scales. The vendored corpus after phase 1 measured 23.4% warm, and this 877M-token index measures 0.2%.
- Phase 2's decode share is uninformative at this scale: untimed posting and token walks dominate (p95 about 206 ms). The cost worth attacking is those walks (#246), which a decoded-object cache would not remove.
- Phase 3 passes with margin even on the conservative figure, but on a synthetic session. **Condition:** before story 49 is built, confirm at least 20% query-RPC repeats on a real agent's server (`serve --metrics-listen`), or the owner explicitly waives the check. The 60 s window is wall-clock, so the share also depends on latency.

**Owner sign-off:** ____ (name, date)

### Directories used (A5)

Kept for the phase 3 real-usage confirmation and #246 profiling; delete them when no longer needed.

| Path | What | Size |
|---|---|---|
| `D:\tmp\bench-corpus` | the fetched corpus (fetched as `D:\mg-bench-corpus`, then renamed) | 6.4 GB |
| `D:\tmp\a5-readbench2` | the benchmark db (`readbench.redb`) and workload, plus the `serve --db` sidecars (`.raft.redb`, `.snapshots`, `.LOCK`) | 22.0 GiB |
| `D:\tmp\a5-logs` | fetch, benchmark, serve and session logs, and the session JSON | small |

Already deleted: `D:\mg-target-a5` (the release build), `D:\tmp\a5-readbench` (the partial db from the stack overflow) and `D:\tmp\a5-simtest` (a smoke-test db).

## Hot-term search walks (#246, 2026-10-07)

### What was measured

New read-path counters split `search` into three phases (`read_stats`: `search_postings`, `search_walk_files`, `search_walk_tokens` and, with timing on, `search_posting_nanos`, `search_ctx_nanos`, `search_walk_nanos`; exported as `mg_read_search_items_total{kind}` and `mg_read_search_seconds_total{kind}`). `readbench` prints them as a `search` row under each phase. Same machine, db and workload as A5 (`MG_READBENCH_REUSE=1`, `D:\tmp\a5-readbench2`, 153 queries per pass), release build, `-j 2`.

The baseline (origin/main at 91d6eb3 plus the counters) put the time in **ctx**, not in the walk: per warm query 24.5 ms resolving candidate files, against 1.4 ms in the posting scan and 0.8 ms in the walk. A hot term has about 14,000 candidate files per query on average. Each one's file row was decoded whole from JSON (origin, fingerprint and all) just to filter it and sort it by path, while the limit stops the walk after about 11 files.

### What changed

All of it is read side only: no format change, and results are unchanged.

1. **Slim file decode for filter and sort.** Candidates are filtered and sorted on a decode of only the row's `parent`, `name` and `language`, with the org and repo rows from the per-query entity cache. The full file node is decoded only for files the walk reaches.
2. **Lazy posting values.** The scan keeps only the file id, plus the count (the first varint) for a counts-only roll-up. A stream grain reads a posting's ordinals with a point lookup, and only for a file the walk reaches.
3. **Org and repo rows** were already cached per query (`entity`, read cache phase 1), so `ctx` needed no new cache.
4. **Token-grain early stop.** Inside a file, the token walk stops once `want - rows.len()` matches have passed the class filter (`Lazy::tokens_at_while`). Token rows are keyed (path, start, ordinal) and a stream stores tokens sorted by start, so later matches in that file sort past the cut-off. The symbol, method and class grains and the class-grain `past_cut` (#149) are untouched. On this workload it saves little: walk tokens per query went from 517.8 to 517.4.
5. **Validation note.** The full node decode, and with it the validation of the rest of the row (e.g. the encoding name), now runs only for walked files. So a corrupt file row that is filtered out or lies past the limit is no longer reported as an error. Results on a healthy store are unchanged.
6. The optional top-k partial sort was skipped. The sort is no longer the cost, and a partial sort would complicate equivalence.

### Numbers

| phase | p50 ms before | p50 ms after | p95 ms before | p95 ms after | qps before | qps after | store ms/q before | store ms/q after |
|---|---|---|---|---|---|---|---|---|
| cold redb cache (1 MiB, fresh process) | 0.432 | 0.424 | 510.6 | 360.5 | 14 | 19 | 72.1 | 53.0 |
| warm x20, timing off | 0.069 | 0.071 | 212.4 | 93.1 | 32 | 71 | n/a | n/a |
| warm x20, timing on | 0.069 | 0.073 | 208.9 | 93.3 | 33 | 71 | 30.7 | 14.1 |
| 8 threads x20 | 0.084 | 0.089 | 282.0 | 129.0 | 188 | 405 | 42.4 | 19.3 |
| 16 threads x20 | 0.115 | 0.133 | 455.0 | 201.9 | 233 | 529 | 68.1 | 30.0 |
| 32 threads x20 | 0.149 | 0.188 | 764.2 | 341.2 | 273 | 624 | 110.7 | 49.9 |

The search breakdown per query (warm, timing on):

| | posting ms | ctx ms | walk ms | postings | walk files | walk tokens |
|---|---|---|---|---|---|---|
| before | 1.364 | 24.460 | 0.799 | 13,962 | 10.7 | 517.8 |
| after | 1.113 | 11.936 | 0.603 | 13,962 | 10.7 | 517.4 |

- **Timing overhead:** -0.3% to +0.4% (median -0.2%) after, against -5.0% to +4.5% (median -0.5%) before.
- **What is left:** ctx is still most of the time, about 12 ms per query: about 14,000 point reads of file rows plus a JSON scan of each. Taking it further needs a compact per-file sort key (path and language) stored outside the JSON node, which is a format change, so it is not part of #246.

### Equivalence evidence

- **Conformance:** a new `hot_term_pages` case covers all seven grains, with no class filter, an identifier filter and a keyword filter. Six files sit in two orgs, two repos and a subdirectory. Every offset in {none, 0, 1, n/2, n, n+1} is crossed with every limit in {none, 0, 1, 2, n, n+1}, and each page must equal that slice of the unpaged answer. `run_all`, `run_differential` and `run_crash_rerun_differential` all pass (the graph-store suite: 192 tests).
- **Corpus differential:** testdata/corpus was indexed once, as 31 repos across two orgs. The origin/main binary and this branch's binary then ran the same 924 searches, all exiting 0:
  - 12 terms, hot (`the`, `return`, `(`, `{`, `if`, `self`, `int`, `=`, `;`) and cold (`cJSON_Parse`, `Rebus`, a missing term);
  - × 7 grains;
  - × 11 variants: no limit; limit 1, 7 or 100; offset 3 limit 5; offset 50 limit 20; offset 1000; an identifier filter with limit 30; a keyword filter; a language filter with limit 40; an org filter with limit 25.

  The `--json` output is byte-identical: 77.9 MB.

## Compact per-file sort key (epic story 61, #262, 2026-10-10)

### What changed

- **New derived table `file_keys`**: file id to a compact row, `varint(repo id)`, then `varint(0)` or `varint(len + 1)` plus the lowercased language, then the path bytes (`codec::encode_file_key`, pinned by `file_key_golden_bytes`). `search` filters and sorts candidates on it instead of on each candidate's JSON `nodes` row. It falls back to that row if a file has no key.
- **Upgrade path: a `derived_version`, not a `V2_SCHEMA_VERSION` bump.** `FILE_KEYS_DERIVED_VERSION = 1` is stored in `meta` under `derived_version_file_keys`. A file without the stamp or the table gets the table built on open, from the File nodes, in one transaction, with a one-line notice on stderr; after that a reopen writes nothing.
  - Older binaries still open the file and ignore the table.
  - If one of them writes, story 58's writer mark (or, for a removal, `file_keys` rows != `stream` rows) makes this binary rebuild the table on the next open.
- **Repos resolved once per query.** Each candidate's repo is resolved and filtered by org and repo once, and the sort compares a repo rank (repos ordered by org name, repo name) and then the path, not three strings for every pair.
- **The sort is lazy.** `LazySorted` selects and sorts only the prefix the walk reaches: chunks of 64, doubling. The first chunk is sorted inside the ctx phase, and later chunks count in the walk phase. The order is total (no two candidates share repo and path), so the output is exactly what a full sort gives.
- **Tried and dropped:** a sequential range scan of `file_keys` over the candidates' id range. At about 18% density it cost about 0.52 µs per candidate, against 0.37 µs for point lookups.

### Numbers (A5, 877M tokens, same host as #246)

Setup:
- `MG_READBENCH_REUSE=1` and `MG_READBENCH_REPS=5`: 5 reps per pass instead of 20, to keep 12 runs to a few hours.
- Per binary: one warm-up run discarded, then 5 measured runs; each figure is the median of the 5.
- "Before" is origin/main at 15f3a90 on `D:\tmp\a5-readbench2`. "After" is this branch on a copy of that file, upgraded on its first open.
- Other agents' builds shared the machine during both series.

| metric | before (main) | after | change |
|---|---|---|---|
| warm p95 ms (median of 5) | 106.7 | 45.8 | -57% |
| ctx ms/q (warm, timing on) | 13.44 | 4.83 | -64% (-59% against #246's 11.9) |
| walk ms/q | 0.71 | 0.79 | +0.08 (the lazy sort's later chunks) |

The five p95 values were 107.8, 106.7, 108.2, 99.1 and 101.6 before, and 46.0, 46.5, 45.8, 45.2 and 45.0 after. The ctx values were 13.85, 13.44, 13.86, 12.65 and 13.01 before, and 4.94, 4.83, 4.83, 4.77 and 4.77 after.

One run per binary (the third measured run):

| phase | p95 ms before | p95 ms after | qps before | qps after | store ms/q before | store ms/q after |
|---|---|---|---|---|---|---|
| cold redb cache (1 MiB, fresh process) | 390.6 | 196.5 | 17 | 30 | 59.4 | 33.2 |
| warm x5, timing on | 106.7 | 46.5 | 62 | 129 | 16.0 | 7.8 |
| 8 threads x5 | 162.2 | 78.8 | 323 | 638 | 24.5 | 12.0 |
| 16 threads x5 | 235.9 | 154.5 | 443 | 673 | 35.5 | 23.5 |
| 32 threads x5 | 369.3 | 247.8 | 554 | 817 | 54.0 | 38.3 |

### Equivalence evidence

- **Conformance:** a new case, `file_keys_follow_writes`, covers ingest, re-index with a new language, prune and vacuum, through the language, org and repo filters and the order. `run_all`, `run_differential` and `run_crash_rerun_differential` pass.
- **Other tests:**
  - `check_consistency` now requires `file_keys` to equal what the File nodes imply.
  - The story-58 old-writer tests now also require one `file_keys` rebuild and none on a later reopen.
  - New tests cover the upgrade of a file from before this story (built once, answers unchanged, no write on the next reopen), a stale stamp, a short table, the golden values, and `LazySorted` against a full sort.
- **Corpus differential:** the #246 recipe. testdata/corpus was indexed as 31 repos in two orgs, then the same 924 searches ran: the same 12 terms × 7 grains × 11 limit, offset and filter variants, with the language filter on `csharp`. The `--json` output was byte-identical in three ways, 82.7 MB each, with the same SHA-256:
  - origin/main;
  - this branch, on its own fresh index;
  - this branch, on the main-built file upgraded on open. The only extra bytes are the one-line upgrade notice on stderr.