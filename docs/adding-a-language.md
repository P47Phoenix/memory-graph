# Adding a language

Every file gets exact-span tokens from the generic tokenizer with no extra
work. A language **extractor** adds symbols (types, functions, sections, ...)
on top. Extractors are ordinary Rust crates that depend only on `graph-core`;
nothing in the storage or query layers changes when you add one.

A complete, tested example lives in [`examples/toy-extractor`](../examples/toy-extractor)
(INI files: sections and keys, about 100 lines).

## The plugin API

These `graph-core` items are the stable surface for extractors:

| Item | Role |
|---|---|
| `Extractor` | `language()`, `extract(src)`, `version()`, `extensions()` |
| `Extraction` | `symbols`, `tokens`, `has_errors` |
| `SymbolDecl` | `name`, `kind: SymbolKind`, `lang_kind: Option<String>`, `span` |
| `TokenDecl`, `Span`, `TokenClass`, `SymbolKind` | schema types |
| `tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION}` | the shared tokenizer |
| `scan::{Cursor, matching_close, span_between}` | helpers for token-stream scanners |

### `Extractor`

- `language()` — the name stored on File nodes (stored lowercased).
- `extensions()` — extensions this extractor claims, without the dot, any case.
  Claims are checked **before** the built-in extension table, so you can add a
  new language (`toy`) or take over an existing extension (`rs`). If two
  extractors claim the same extension or language, the one registered last
  wins. Files whose extension nobody claims use the built-in table, well-known
  file names (`Makefile`, `Dockerfile`) and the `#!` line; a claim beats all
  three. An extension is what follows the file name's last dot, so a dotted
  claim such as `d.ts` is ignored and a dotfile such as `.ini` is never
  claimed.
- `extract(src)` — returns tokens and symbols. It should never panic, and
  should set `has_errors` only when the source is truly unparseable for your
  extractor — a scanner that finds fewer symbols on odd input is not an error.
- `version()` — part of every file's fingerprint. **Bump it whenever
  `extract`'s output could change**, and include `TOKENIZER_VERSION`, e.g.
  `format!("ini-scan-1+tok{TOKENIZER_VERSION}")`. Files indexed by an older
  version are re-indexed; unchanged files are skipped.

### Symbols and spans

- `SymbolDecl` is flat. Parents are derived from **span containment**: a symbol
  whose span lies inside another's becomes its child (that is how `server::port`
  gets its qualified name).
- Spans must be exact byte/line/col ranges of the source (lines and columns are
  1-based; columns count chars). Build them from token spans with
  `scan::span_between(&first.span, &last.span)` rather than by hand.
- Two symbol spans must either nest or be disjoint. **Partially overlapping
  spans are rejected** by the store. Equal spans are allowed.
- `SymbolKind` is a closed set (`module`, `type`, `function`, `method`,
  `variable`, `constant`, `other`). Put your language's own vocabulary in
  `lang_kind` (`section`, `property`, `control`, ...); `describe` shows both as
  `generic/lang_kind`.

### Tokens

Use `tokenizer::tokenize_with` so tokens match everything else in the graph.
`TokenizerOptions` selects dialect switches; its default is the
language-agnostic fallback. Tokens must cover the source in order with exact
spans; the store validates this.

### Scanner helpers (`graph_core::scan`)

Extractors here are **token-stream scanners, not parsers** (no C-based parser
generators such as tree-sitter: the pure-Rust gate forbids them).

- `Cursor` — forward cursor with `peek_code`/`next_code`/`eat` that skip
  comments, and `skip_balanced` to jump over a `(...)`, `[...]` or `{...}` group.
- `matching_close(tokens, i)` — index of the delimiter closing `tokens[i]`,
  ignoring delimiters inside literals and comments; `None` if unbalanced.
- `span_between(a, b)` — the span from the start of `a` to the end of `b`.

## Registering

Library use: pass your extractor to `open_store`:

```rust
let store = graph_store::open_store(path, vec![Box::new(toy_extractor::IniExtractor)])?;
```

The `memory-graph` CLI registers `graph_cli::shipped_extractors()`, one per
enabled `lang-*` Cargo feature (all on by default). To ship a language with the
CLI, add an optional dependency and a `lang-<name>` feature in
`crates/graph-cli/Cargo.toml` and push it in `shipped_extractors`.

Dynamic loading (`.so`/`.dll` plugins) is deliberately not supported: Rust has
no stable ABI, and loaders bind the platform's C `dl` library. Compile-time
registration is the plugin model.

Note: a binary built **without** a language's feature indexes those files
tokens-only, and because the extractor version is part of the fingerprint it
re-indexes files that a full build indexed with symbols. Use the same feature
set for every binary that writes to a database.

## Test checklist

- One test per symbol kind, asserting the exact source text each span covers.
- BOM and non-ASCII input: line/col still exact.
- Malformed input (unclosed braces or tags, truncated files): no panic, no
  partially overlapping spans, `has_errors` stays false.
- A property test feeding random token soup through `extract` and asserting
  every pair of symbol spans nests or is disjoint.
- An end-to-end test registering the extractor through `open_store` on both
  backends (see `examples/toy-extractor/tests/store.rs`).
