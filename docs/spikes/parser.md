# Spike: pure-Rust parsing (story 1)

**Status: deferred.** The MVP needs no parser: the hand-written fallback tokenizer (`graph-core::tokenizer`) covers every language with zero dependencies. `syn`/`proc-macro2`, `logos` and `ruff_python_parser` were **not** evaluated yet.

## Findings from the fallback tokenizer
- Byte, line and column spans are exact for every token (property-tested against an independent computation).
- Never panics or fails on malformed input; unterminated strings/comments emit the remainder.
- Cannot distinguish keywords from identifiers or find symbols. That is what per-language extractors add.

## To do before story 9
Run the story 1 acceptance criteria (spans, error recovery, purity, license, compile time) for the three candidates. The `Extractor` trait is designed so a `syn`-based Rust extractor only returns `SymbolDecl`s and `TokenDecl`s; symbol nesting is derived from span containment.
