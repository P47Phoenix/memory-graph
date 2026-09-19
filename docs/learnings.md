# Learnings

Short, durable. Each item links to the detail.

## Measured facts (numbers are in the spike; [M] measured, [E] estimated)
- Token text is 2.2% of a JSON token node; the cost is the envelope: ~525 B of pages per token today (~248 B JSON + B-tree slack + two secondary entries). [M] ([data-model spike](spikes/data-model.md), section 3)
- 47.7% of distinct texts occur once; the top 100 texts are 70.9% of occurrences. [M]
- A dictionary + stream + postings prototype was ~20 B/token (order of magnitude ~25x smaller) with exact spans; ingest ~3-5x faster (the prototype omitted validation, so expect the low end); roll-ups of very common terms 20-370x faster. [M] ([ADR 0003](adr/0003-data-model.md), "Honest numbers")
- 134 ms of every ~215-261 ms CLI `search`/`symbols` call is an O(tokens) `describe` in `validate_filters`. [M]
- 100 M-token figures and the 8-12 B/token target are [E] until built.

## Constraints and gates
- Pure Rust: `python3 scripts/check-no-c-deps.py` gates on native-linking `-sys` crates and C build scripts, not on crate names; pure-Rust `-sys` crates are allowed ([ADR 0002](adr/0002-parsing-and-crate-layout.md)). Stored data must represent any language.
- redb holds an exclusive file lock; snapshot isolation is in-process only (ADR 0003, Q5, to be verified).
- Fingerprints are `sha256:<hex>|lang|extractor+tokN|format`; changing extraction semantics must bump the version so unchanged-file skipping cannot serve stale data.

## Review pitfalls found
- Quote the right data set and say which one (1x vs 4x vs 41x); headline multiples mixed them.
- Prototype ingest speed excludes validation, `origin`, `has_errors` and prune; do not quote it as the real gain.
- Write amplification via `wchar` is not device I/O.
- Spans "exactly as given" (overlapping, out of order, zero-length) must round-trip or be rejected before any write.
- Ids emitted in JSON must be strings above 2^53.
- Keep spike code outside the workspace build: extra dev-dependencies (`sha2`, `miniz_oxide`) must not enter `Cargo.lock`.

## Process
- Every PR: watch CI, and get independent developer and QA reviews.
