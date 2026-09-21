# Learnings

Short, durable. Each item links to the detail.

## Measured facts (numbers are in the spike; [M] measured, [E] estimated; see [labels](glossary.md#labels-m-and-e))
- Token text is 2.2% of a JSON token node; the cost is the record around it (the envelope): ~525 B of pages per token today (~248 B JSON + B-tree [page slack](glossary.md#page-slack) + two secondary entries). [M] ([data-model spike](spikes/data-model.md), section 3)
- 47.7% of distinct texts occur once; the top 100 texts are 70.9% of occurrences. [M]
- A [dictionary](glossary.md#dictionary) + [stream](glossary.md#stream) + [postings](glossary.md#inverted-index--postings) prototype was ~20 B/token (order of magnitude ~25x smaller) with exact spans; ingest ~3-5x faster (the prototype omitted validation, so expect the low end); [roll-ups](glossary.md#grain--roll-up) of very common terms 20-370x faster. [M] ([ADR 0003](adr/0003-data-model.md), "Honest numbers")
- 134 ms of every ~215-261 ms CLI `search`/`symbols` call is an O(tokens) `describe` in `validate_filters`. [M]
- 100 M-token figures and the 8-12 B/token target are [E] until built.

## Constraints and gates
- [Pure Rust](glossary.md#c-dependency--pure-rust): `python3 scripts/check-no-c-deps.py` gates on native-linking `-sys` crates and C build scripts, not on crate names; pure-Rust `-sys` crates are allowed ([ADR 0002](adr/0002-parsing-and-crate-layout.md)). Stored data must represent any language.
- [redb](glossary.md#redb) holds an exclusive file lock (only one process can open the file); verified in redb 2.6.3 (`flock(LOCK_EX|LOCK_NB)`, immediate `DatabaseAlreadyOpen`, no waiting); [snapshot isolation](glossary.md#snapshot--snapshot-isolation) is in-process only and a process holding the file blocks other processes' readers (ADR 0003, Q5; decided 2026-09-20: an owning daemon, `memory-graph serve`, with direct-open and jittered retry as the fallback).
- [Fingerprints](glossary.md#content-hash--fingerprint) are `sha256:<hex>|lang|extractor+tokN|format`; changing extraction semantics must bump the version so unchanged-file skipping cannot serve stale data.

## Review pitfalls found
- Quote the right data set and say which one (1x vs 4x vs 41x); headline multiples mixed them.
- Prototype ingest speed excludes validation, `origin`, `has_errors` and prune; do not quote it as the real gain.
- [Write amplification](glossary.md#write-amplification) measured via `wchar` (bytes the program asked to write) is not real disk I/O.
- [Spans](glossary.md#span) "exactly as given" (overlapping, out of order, zero-length) must round-trip or be rejected before any write.
- Ids emitted in JSON must be strings above 2^53.
- Keep spike code outside the workspace build: the spike's extra dev-dependency `miniz_oxide` must not be added to the workspace (`sha2` is already a `graph-store` dependency; `Cargo.lock` is gitignored).

## Process
- Every PR: watch CI, and get independent developer and QA reviews.
