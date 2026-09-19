# ADR 0002: Tokenizing, extractors and crate layout

**Status:** Accepted

**Decisions**
- `graph-core`: schema, `Extractor` trait, fallback tokenizer. No storage dependency.
- `graph-store`: redb persistence and search, language-agnostic.
- `graph-cli`: `memory-graph` binary; the only place file extensions map to language names.
- Extractors return `SymbolDecl` (kind, name, span) and `TokenDecl`; the store derives the hierarchy from span containment, so extractors need not track parents.
- Any language works through the fallback tokenizer. Rich extractors (Rust first, story 9) and NDJSON ingest (story 16) add symbols.
- Pure Rust is enforced by `scripts/check-no-c-deps.py` in CI (fails on native-linking `-sys` crates and C build scripts; exceptions in `scripts/c-deps-exceptions.txt`).

- `graph-lang-rust`: Rust extractor (`syn` for symbols, generic tokenizer for tokens). Registered on a `Store` with `Store::register`; unregistered languages use the fallback.

**Open:** Python parser blocked on the ruff MSRV (docs/spikes/parser.md).
