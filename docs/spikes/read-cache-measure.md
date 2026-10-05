# Spike: how much of a read is decode? (read cache phase 0)

ADR 0008 (read cache), epic story 45. Before we build a cross-query read
cache, measure whether decoding stored bytes is a large enough share of
query time to be worth caching. The ADR's gate is that decode should take
at least about 25% of warm query time.

## Method

- **Counters.** `graph_store::read_stats()` returns process-wide relaxed
  atomic counters. They count reverse-dictionary block decodes
  (`dict_rev_lookup`), per-query term-text memo hits and misses, lazy
  stream-header decodes (`codec::decode_lazy`), symbol-section decodes
  (`Lazy::symbols`) and full stream decodes (`codec::decode`, which counts
  once rather than also as lazy plus symbols). They also count the read
  transactions opened by `StoreRead` calls on a `V2Store` (the `store_read!`
  macro).
- **Timing.** Optional per-category nanoseconds, turned on with
  `set_read_timing(true)`. Timing is off by default, so the production cost is
  one relaxed add per event plus one relaxed load.
- **Benchmark.** `cargo run --release -p graph-store --example readbench --
  <workdir> testdata/corpus 20 1048576`
  - Indexes the vendored corpus: 795 files, 372k tokens, a 16.6 MiB db. Only
    the Rust extractor is registered, because the example depends only on
    graph-store's dev-dependencies. Other languages get fallback tokens and
    no symbols.
  - Runs 153 queries per pass:
    - 6 hot terms (the most frequent identifiers) and 18 cold terms
      (frequency 3), each searched at token, symbol, method, class and file
      grain with `limit 50`, plus a `term*` `search_symbols`;
    - 8 `file_tokens` calls;
    - 1 `describe`.
  - Phases:
    - **Cold:** a fresh open with a 1 MiB redb cache. The OS file cache is
      still warm.
    - **Warm:** 20 repeated passes, once with timing off and once with it on.
    - **Concurrent:** 1, 8 and 32 threads on one store, 5 passes each.
- "Decode share" is the decode nanoseconds divided by the sum of per-query
  latencies.

## Numbers

These come from one Windows 11 desktop, memory-constrained (other builds were
running), release build.

| phase | p50 ms | p95 ms | qps | decode share | dict | lazy+sym | full | dict blocks/q | lazy/q | sym/q | txns/q |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cold, 1 MiB cache | 0.028 | 3.04 | 1630 | 10.4% | 9.0% | 1.2% | 0.2% | 7.4 | 5.7 | 5.7 | 1.00 |
| warm x20 | 0.020 | 1.82 | 2754 | 19.1% | 16.7% | 2.0% | 0.3% | 7.4 | 5.7 | 5.7 | 1.00 |
| 1 thread | 0.020 | 2.06 | 2711 | 17.2% | 14.7% | 2.2% | 0.2% | 7.4 | 5.7 | 5.7 | 1.00 |
| 8 threads | 0.022 | 1.92 | 17823 | 19.7% | 17.8% | 1.7% | 0.3% | 7.4 | 5.7 | 5.7 | 1.00 |
| 32 threads | 0.027 | 2.57 | 16966 | 20.3% | 18.6% | 1.4% | 0.3% | 7.4 | 5.7 | 5.7 | 1.00 |

- **Overhead.** Turning timing on cost +3.3% of warm wall time, which is
  within run-to-run noise. One relaxed counter bump measured 2.5 ns. At about
  20 to 25 counted events per query, against a mean query time of about
  0.35 ms, counting costs well under 0.1%.
- **Results.** No query result changed: the graph-store test suite passes
  with the counters in place.

## Conclusion

- **Gate: not met on this corpus.** Measured decode is about 17-20% of warm
  query time, below the ~25% gate. It is also lower when cold (10%), where
  redb page reads dominate.
- **The dominant hot spot is the dictionary.** Hot spot 1, `dict_rev_lookup`,
  decodes a whole reverse-dictionary block, up to 64 KiB, to read one id. It
  accounts for about 85-90% of all decode time, at about 7 block decodes per
  query, one per memo miss.
- **The other decodes are small.** Lazy and symbol decodes (hot spot 2) are
  about 2%, and full decodes (hot spot 4) are under 0.5%. Read transactions
  (hot spot 3) are exactly one per call. Their cost is not timed here; it
  sits in the non-decode remainder.
- **Implication for ADR 0008.** A general decoded-stream cache is not
  justified by these numbers. The cheap, targeted win is the dictionary: a
  cross-query id-to-text cache, or a block format that can seek to one entry
  without decoding the whole block. Either one would remove most of the
  measured decode time without the invalidation cost of a stream cache.

## Caveats

- **The corpus is small.** It is 16.6 MiB, so everything fits in RAM, and
  only Rust files have symbols. A large multi-language store with symbols
  everywhere would do more symbol-section decodes per query. The run should
  be repeated on a real 10 GB-class index before the ADR closes.
- **Token-stream walks are not counted.** These are `Lazy::tokens` and
  `tokens_at`. Their timing would include the per-token query callback, so
  the true decode share for token-grain queries is somewhat higher than shown.
- **One machine, one run.** Treat the shares as rough figures, not precise
  ones.
