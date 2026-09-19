# Spike: pure-Rust parsing (story 1)

Measured on rustc 1.94.1 with a scratch crate (2026-09-19).

| Candidate | Pure Rust | Spans | Error recovery | Notes |
|---|---|---|---|---|
| `syn` 2 + `proc-macro2` (`span-locations`) | Yes (0 `-sys` in `cargo tree`), MIT/Apache-2.0 | Byte range and line/col for every item (`Span::byte_range`); tokens have line/col | **None.** `parse_file` returns `Err` on any syntax error, and proc-macro2 fails to lex unbalanced delimiters, so no tokens either | Drops comments. Strips a leading BOM, so byte ranges start after it (the extractor compensates). Clean build about 2 s. |
| `logos` 0.15 | Yes, MIT/Apache-2.0 | Byte ranges from the lexer | Lexer-level errors per token | Builds fine; **not evaluated further**: the hand-written fallback tokenizer already covers lexing for all languages, so a per-language `logos` lexer is only worth it if a language needs keyword classification. |
| `ruff_python_parser` 0.0.5-0.0.14 | Yes (Ruff crates) | Text ranges | Designed to recover (`parse_unchecked` returns errors + AST) | **Blocked on this toolchain**: 0.0.5 requires rustc 1.95 and 0.0.14 requires 1.96; we have 1.94.1. Not run. Retest after a toolchain bump, or use a vendored fork. |

## Decision
- **Rust extractor (story 9): hybrid.** Symbols from `syn`; tokens always from `graph_core::tokenizer` (exact spans, keeps comments, never fails). On a parse error, tokens only, and the File is flagged `has_errors`.
- **Python extractor (story 16 area):** blocked on the ruff MSRV; revisit after a toolchain bump. Until then Python uses the fallback tokenizer, or agents supply structure via NDJSON ingest.
- Estimates: story 9 (8 pts) holds. Story 6 was completed inside the MVP.
