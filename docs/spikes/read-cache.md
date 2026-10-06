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
- **Benchmark.** `cargo run --release -p graph-cli --example readbench --
  <workdir> testdata/corpus 20 1048576`
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
- **Owner sign-off:** _pending_ (name, date).

## Caveats

- **One machine.** Treat the shares as rough figures; the two runs agreed
  within about a point on every warm and concurrent row.
- **Not yet measured:** query wall time inside the store, decode bytes, and
  the bypass rate with a concurrent indexer (#233).

## Large-index re-run (A4, issue #233)

The vendored `testdata/corpus` is too small to settle the decode share at scale, so the ADR 0008 re-run uses a large public corpus fetched by `scripts/fetch-bench-corpus.py`. Nothing is vendored: the script shallow-fetches 29 permissively licensed public repos (MIT, Apache-2.0, BSD, MPL, PostgreSQL), each pinned to an exact commit, into a directory outside the repo. Together they cover Rust, C, C++, Go, Java, Scala, C#, F#, JavaScript, TypeScript, Python, SQL, shell, R, Haskell, Elixir, GDScript, COBOL, assembly, HTML and Razor, about 7 GB checked out (an estimate; the script prints the measured bytes and files per language at the end).

```sh
python3 scripts/fetch-bench-corpus.py --list                         # the manifest: name, sha, licence, size, languages
python3 scripts/fetch-bench-corpus.py --dest /data/mg-bench           # fetch everything (resumable; repos already at their sha are skipped)
python3 scripts/fetch-bench-corpus.py --dest /data/mg-bench --only go,tokio --verify-licenses
```

To run the read benchmark against it, index the directory and point the benchmark at it with `MG_READBENCH_CORPUS=/data/mg-bench`. That env var is planned: it lands with the reworked benchmark (A3), and until then the benchmark reads only `testdata/corpus`.
