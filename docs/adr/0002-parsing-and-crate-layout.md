# ADR 0002: Tokenizing, extractors and crate layout

**Status:** Accepted

> **In plain words**
> - **Problem:** we must read code in many languages and split it into [tokens](../glossary.md#token) and [symbols](../glossary.md#symbol), without tying the database to any one language.
> - **Choice:** split the code into three [crates](../glossary.md#crate) (core, store, cli). Language readers ([extractors](../glossary.md#extractor)) only report what they see and where ([spans](../glossary.md#span)). The store works out what contains what. A simple [fallback tokenizer](../glossary.md#fallback-tokenizer) handles every other language.
> - **Why:** any language works from day one, and the database stays language-agnostic. A CI check keeps the project [pure Rust](../glossary.md#c-dependency--pure-rust).
> - **Cost:** the fallback finds tokens but no symbols. Richer readers must be written language by language. Python is blocked for now.

**TL;DR:** three crates (core, store, cli); extractors return spans, the store derives the hierarchy; fallback tokenizer for any language; pure-Rust gate in CI.

## Decisions

- `graph-core`: the schema, the `Extractor` trait, and the fallback tokenizer. It has no storage dependency.
- `graph-store`: [redb](../glossary.md#redb) storage and search. It is language-agnostic.
- `graph-cli`: the `memory-graph` command-line program. It is the only place that maps file extensions to language names.
- Extractors return `SymbolDecl` (kind, name, span) and `TokenDecl`. The store works out the hierarchy from span containment (an inner span sits inside an outer one). So extractors do not need to track parents.
- Any language works through the fallback tokenizer.
- Rich extractors add symbols. Rust comes first (epic story 9). [NDJSON](../glossary.md#ndjson) ingest also adds symbols (epic story 13). See [story numbering](../glossary.md#adr-story--epic-story-story-numbering).
- `graph-lang-rust`: the Rust extractor. It uses `syn` for symbols and the generic tokenizer for tokens. You register it on a `Store` with `Store::register`. Unregistered languages use the fallback.
- Pure Rust is enforced in CI by `scripts/check-no-c-deps.py`.
  - It fails on native-linking `-sys` crates and on C build scripts.
  - Exceptions live in `scripts/c-deps-exceptions.txt`.
  - See [C dependency](../glossary.md#c-dependency--pure-rust).

**Open:** the Python parser is blocked on the ruff MSRV (the minimum Rust version ruff needs). See the [parser spike](../spikes/parser.md).
