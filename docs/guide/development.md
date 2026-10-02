# Development

Building, testing and embedding memory-graph. Contributors should also read [CLAUDE.md](../../CLAUDE.md) (architecture summary and invariants) and [docs/testing.md](../testing.md).

## Building and testing

```sh
cargo build --release                                    # the memory-graph binary in target/release/
cargo fmt --all --check                                  # formatting (CI)
cargo clippy --workspace --all-targets -- -D warnings     # lints (CI, zero warnings)
cargo test --workspace                                    # unit + integration tests
cargo test -p graph-cli --test corpus                      # public-repo corpus: exact spans, cross-repo links
cargo test -p graph-cli --test e2e                          # CLI end-to-end
python3 scripts/test_gate.py                               # CI's extra gate (includes the doc link check)
python3 scripts/check-no-c-deps.py                          # pure-Rust gate: fails on any C build script, native link or deny-listed crate, on any shipped target
python3 scripts/check-doc-links.py                          # relative links and #anchors in README.md and docs/
docker build -t memory-graph .                              # the container image
```

CI runs all of the above on every push and pull request, plus a real disk-full run on tmpfs, the machine probes on ubuntu, macOS and Windows, and the Docker image's smoke test ([docs/testing.md](../testing.md)).

## Test corpus

`testdata/corpus/` vendors real public code (MIT/Apache-2.0 only, pinned commits, see each folder's `UPSTREAM.md`) as distinct repos grouped into applications by `corpus.json`:

- **messaging**: `rebus` + `rebus-rabbitmq` + `rebus-sqlserver` (transports implementing Rebus)
- **conduit**: `conduit-ui` (Angular) → `conduit-api` (Spring) → `conduit-data-access` (MyBatis) → `conduit-sql`
- **rust-library**: `anyhow`

`cargo test -p graph-cli --test corpus` checks the manifest (public, licensed), that every cross-repo link resolves, that every token of every file is parsed with exact spans, and that the graph stores exactly those tokens. Re-vendor with `scripts/vendor-corpus.py`.

## Using it as a library

The CLI depends on the object-safe `graph_store::Store` / `StoreRead` traits, not on redb. `open_store(path, extractors)` returns a `Box<dyn Store>` over `V2Store`, the one storage format (`V2Store::open(path)` gives the concrete type); bring the traits into scope (`use graph_store::{Store, StoreRead}`) to call methods on it.

- Indexing is split into `Store::prepare` (pure, callable from many threads) and `Store::index_prepared` (the commit); `Store::index_batch` reports a file's `InvalidSpan` in that file's result slot and returns `Err` only for storage errors. When only the symbols are invalid, `prepare` keeps the tokens, drops the symbols, and sets `IngestStats::span_warning` (#203). `ingest` with a caller-supplied extraction still rejects.
- `Extractor` requires `Send + Sync`.
- `graph_store::conformance::run_all` is a reusable test suite for any `Store` implementation.

See the [architecture diagrams](../architecture-diagrams.md) and [ADR 0002](../adr/0002-parsing-and-crate-layout.md) (parsing and crate layout).
