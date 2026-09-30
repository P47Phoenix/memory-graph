# Languages

Languages are detected per file from the extension, the filename (`Makefile`) or a `#!` line, so a polyglot repo needs no flags. Every file gets tokens from the generic tokenizer; these languages also get symbols:

| Language | Extensions | Extractor |
|---|---|---|
| Rust | `rs` | `syn` (full parse) |
| C# | `cs`, `csx` | token-stream scanner |
| JavaScript | `js`, `mjs`, `cjs`, `jsx` | token-stream scanner |
| TypeScript | `ts`, `tsx`, `mts`, `cts` | the JavaScript scanner plus interfaces, type aliases, enums, namespaces, abstract classes and typed class fields |
| Python | `py`, `pyw`, `pyi` | indentation scanner (classes, functions, methods, lambdas, module constants); a file with unbalanced brackets or broken indentation is flagged `has_errors` and gets tokens only |
| Java | `java` | token-stream scanner (package, classes, interfaces, enums, records, annotation types, methods, fields, constants) |
| HTML | `html`, `htm`, `xhtml` | element scanner |
| ASP.NET markup | `aspx`, `ascx`, `master` | HTML scanner plus directives, server controls, code blocks and bindings; C# symbols in `<script runat="server">` |
| SQL | `sql` | `CREATE` statement scanner (ANSI, T-SQL, PL/pgSQL, PL/SQL, MySQL) |
| Shell | `sh`, `bash`, `zsh`, `ksh` (and `#!` lines) | token-stream scanner: functions, `export` / `readonly` |
| R | `r`, `rmd`, `qmd` | token-stream scanner (R chunks only in R Markdown / Quarto) |
| F# | `fs`, `fsi`, `fsx` | layout scanner: namespaces, modules, types, `let`, members |
| Haskell | `hs`, `lhs` | layout scanner: module, data/newtype/type/class/instance, functions (signature and equations grouped); literate bird-track and `\begin{code}` |
| Elixir | `ex`, `exs` | `do`/`end` scanner: `defmodule` (a type), `def`/`defp`/`defmacro`..., `defstruct`, `defprotocol`/`defimpl` |
| GDScript | `gd` | layout scanner: `class_name` (the file class), inner classes, `func`, `signal`, `enum`, `const`, `var` |
| C | `c`, `h` | token-stream scanner (`lang-c` feature) |
| C++ | `cpp`, `cc`, `cxx`, `hpp`, `hh`, `hxx`, `ipp` | token-stream scanner (`lang-c` feature, shared with C; a `.h` stays language `c` but is scanned with the C++ rules when it contains `class`/`namespace`/`template`/`public:`) |
| Go | `go` | token-stream scanner (a receiver method is a sibling of its type, with the receiver type as its `owner`, so it rolls up under `--grain class` when the type is declared in the same file) |
| Scala | `scala`, `sc` | token-stream scanner (brace and indentation syntax; a `def` in an `object` is a function). `.sc` is also SuperCollider's extension; such files are scanned as Scala and get few or odd symbols |
| COBOL | `cbl`, `cob`, `cpy` | sentence scanner: programs, divisions, sections, paragraphs, level-01/77 items (fixed and free format) |
| RPG IV / RPGLE | `rpgle`, `sqlrpgle`, `rpgleinc`, `rpg` | token-stream scanner: procedures, subroutines, prototypes, interfaces, data structures, standalone fields, constants, tags (`**FREE`, mixed and fixed form) |
| Assembly | `asm`, `s` (not `inc`: PHP, Pascal and POV-Ray use it too) | line scanner (NASM, MASM, GNU as; x86 and ARM): labels, procs, macros, sections, segments, structs, constants |

Everything else (YAML, ...) is tokenized with exact spans and no symbols; the same happens to a Rust file if the Rust extractor is not registered (a library build without it). Language names are lowercased, a UTF-8 BOM is ignored, and paths are normalized (`./a.rs` = `a.rs`).

## Tokenizer dialects

The generic tokenizer treats `r"a\"b"` as an identifier `r` and a string with Python-style escapes. The Rust extractor uses the `rust_literals` dialect, where raw strings (`r"..."`, `r#"..."#`, `br#"..."#`) and byte literals (`b"..."`, `b'x'`) are single literal tokens and an unterminated raw string runs to end of input.

## Adding a language

Implement the `Extractor` trait in its own crate: see [docs/adding-a-language.md](../adding-a-language.md) and [examples/toy-extractor](../../examples/toy-extractor).
