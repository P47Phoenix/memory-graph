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
| `scan::{Cursor, matching_close, close_table, code_close_table, span_between, code_index, keyword_block, indent_block, line_iter}` | helpers for token-stream scanners |

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

Every switch is off by default; write a dialect as
`TokenizerOptions { flag: true, ..TokenizerOptions::DEFAULT }` (`DEFAULT`
equals `default()` but works in `const` items). Always end a dialect with
`..TokenizerOptions::DEFAULT` rather than listing every field: new switches
are added over time (the struct is deliberately not `#[non_exhaustive]`,
which would forbid that struct-update syntax outside `graph-core`), and a
dialect written that way keeps compiling with the new switch off. The
switches:

| Flag | Effect |
|---|---|
| `rust_literals` | Rust raw/byte strings (`r#"..."#`, `b'x'`) are one Literal |
| `single_quote_strings` | `'...'` is a Literal (backslash escapes, one line) |
| `csharp_strings` | C# `@"..."`, `$"..."`, `$@"..."` |
| `markup` / `aspx` | HTML/XML tags and comments; ASP.NET server tags |
| `regex_literals` | JavaScript `/.../flags` where a regex can start |
| `no_line_slash_comments` / `no_block_slash_comments` | `//` / `/* */` are not comments (Python `//`, shell `/tmp/*`) |
| `hash_comments` | `#` to end of line; with `shell_words` only at a word start |
| `hash_comments_line_start_only` | ...and only as the first token on a line (ARM `#1` immediates stay code) |
| `dash_comments` | `--` to end of line (SQL); with `haskell_block_comments`, only when no symbol character touches the dash run (`-->`, `--|`, `|--` are operators) |
| `haskell_block_comments` | nested `{- ... -}` |
| `prime_idents` | `'` continues an identifier (`foldl'`, `x'`), Haskell and F# |
| `ml_block_comments` | nested `(* ... *)`; `(*)` stays an operator |
| `semicolon_comments` | `;` to end of line (NASM, MASM) |
| `triple_quote_strings` | `"""..."""` / `'''...'''` one Literal, spanning lines, backslash escapes |
| `triple_quote_raw` | ...with no backslash escapes (Scala, F#) |
| `doubled_single_quotes` | `'it''s'` one Literal, no backslash escapes, one line (COBOL, RPG, NASM/MASM) |
| `raw_backtick_strings` | `` `...` `` one Literal, no escapes, spanning lines (Go) |
| `sql_strings` | `'it''s'` Literal; `"name"` and `[name]` Identifier tokens |
| `shell_words` | `$x`, `$1`, `${...}` one Identifier; raw `'...'`; ANSI-C `$'...'` one Literal; heredoc `<<EOF` / `<<-EOF` / `<<'EOF'` / `<<\EOF` marker one Operator and body one Literal (not inside arithmetic `((...))`) |
| `hyphen_idents` | `-` joins a name before a letter/digit/`_` (`WORKING-STORAGE`, `dcl-proc`) |
| `fixed_columns: Option<FixedLayout>` | `Cobol` (cols 1-6 sequence and 73+ are Comments, `*`/`/` in col 7 is a comment line, `*>` inline comments) or `Rpg` (cols 1-5 sequence, col 6 form type its own token, `*` in col 7 comment line, 81+ Comment; a first line `**FREE` turns column handling off) |

Ready-made dialects are associated consts: `DEFAULT`, `RUST`, `CSHARP`,
`JAVASCRIPT`, `TYPESCRIPT`, `HTML`, `ASPX`, `C`, `CPP`, `JAVA`, `GO`,
`SCALA`, `PYTHON`, `GDSCRIPT`, `SHELL`, `R`, `SQL`, `HASKELL`, `FSHARP`,
`ELIXIR`, `ASM`, `COBOL`, `RPG` (see their doc comments for the exact
combinations). Any change to tokens for an existing input needs a
`TOKENIZER_VERSION` bump; new flags are pinned by `DIALECT_GOLDENS` /
`LANG_GOLDENS` and the exact-span proptests in `tokenizer.rs`.

`ASM` limits: `;` always starts a comment (GNU as uses it as a statement
separator on some targets); `#` is a comment only as the first token on a
line, so an AT&T trailing `# comment` after code is missed; ARM32 `@`
comments are not recognised (MASM uses `@@:` labels at line start, so a
line-start `@` rule would be wrong there).

### Scanner helpers (`graph_core::scan`)

Extractors here are **token-stream scanners, not parsers** (no C-based parser
generators such as tree-sitter: the pure-Rust gate forbids them).

- `Cursor` — forward cursor with `peek_code`/`next_code`/`eat` that skip
  comments, and `skip_balanced` to jump over a `(...)`, `[...]` or `{...}` group.
- `matching_close(tokens, i)` — index of the delimiter closing `tokens[i]`,
  ignoring delimiters inside literals and comments; `None` if unbalanced.
  It scans forward from `i`, so calling it for every opener is quadratic on
  long unbalanced runs (100k `(` took ~22 s): scanners that look up closers
  in a loop should build a table once instead.
- `close_table(tokens)` — `matching_close` for every token in one linear
  pass (`close_table(tokens)[i] == matching_close(tokens, i)`, proptested).
- `code_close_table(tokens, code)` — the same, indexed by code position for
  scanners that walk a `code` index (from `code_index` or their own filter):
  entry `c` is the code position closing `tokens[code[c]]`, or `None` if it
  does not close or its closer is not in `code`. Build it once and store it
  in the scanner; this is what the C#, Go, Scala, F#, GDScript, R, SQL,
  JavaScript/TypeScript and Java scanners do.
- `span_between(a, b)` — the span from the start of `a` to the end of `b`.
- `code_index(tokens, skip)` — indices of tokens whose class is not in
  `skip` (e.g. `&[TokenClass::Comment]`).
- `keyword_block(tokens, open, pairs, ignore_case)` — index of the keyword
  closing the block opened at `open`, for `(opener, closer)` pairs such as
  `do`/`end`, `if`/`fi`, `case`/`esac`, `do`/`done`, `BEGIN`/`END`,
  `PROC`/`ENDP`, `MACRO`/`ENDM`, `dcl-proc`/`end-proc`; nests, skips
  comments and literals, `None` on a mismatch or no close.
- `indent_block(tokens, header, skip)` — for layout languages, the last
  token of the block headed by `tokens[header]`: everything on later lines
  indented deeper than the header's line (tokens in `skip` never end it, nor
  do line starts inside open brackets, so split headers and multi-line calls
  stay in the block).
- `line_iter(tokens)` — `(line, index range)` for each run of tokens
  starting on the same line.

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
