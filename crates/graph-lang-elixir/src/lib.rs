//! Elixir extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer (`ELIXIR` dialect). Blocks are
//! `do`...`end` (and `fn`...`end`), matched with
//! `graph_core::scan::keyword_block`; keyword-list forms (`do:`), atoms
//! (`:do`) and field access (`x.end`) are not block keywords.
//!
//! | Elixir | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `defmodule` | Type | `defmodule` |
//! | `defprotocol` / `defimpl` | Type | the keyword |
//! | `defstruct` / `defexception` | Type | the keyword |
//! | `def` `defp` `defmacro` `defmacrop` `defguard` `defguardp` `defdelegate` | Method (Function outside a module) | the keyword |
//!
//! `defmodule` is a Type, not a Module: Elixir modules are where functions
//! live, so they play the part of a class, and the `class` search grain
//! rolls a function up to its module only when the module is a Type.
//! `defimpl Proto, for: T` is named `Proto for T`; `defstruct` and
//! `defexception` are named `%Module{}` after the enclosing module.
//!
//! A declaration spans from its keyword through its `end`, or, without a
//! `do` block (`def f(x), do: x`, bodiless heads), through the last token
//! of its indented block. Each clause of a multi-clause function is its own
//! symbol. Function bodies are not scanned.
//!
//! Known limits: sigil contents are lexed as code, so a `do` or `end` word
//! inside `~s(...)` can unbalance a block (the block then ends early or is
//! found by indentation); declarations are recognised only at the start of
//! a line (or after `;`).
use graph_core::scan::{code_index, indent_block, keyword_block, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct ElixirExtractor;

/// Tokenizer dialect used for Elixir.
pub const ELIXIR_TOKENIZER: TokenizerOptions = TokenizerOptions::ELIXIR;

impl Extractor for ElixirExtractor {
    fn language(&self) -> &str {
        "elixir"
    }

    fn extensions(&self) -> &[&str] {
        &["ex", "exs"]
    }

    fn version(&self) -> String {
        format!("elixir-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, ELIXIR_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

const PAIRS: &[(&str, &str)] = &[("do", "end"), ("fn", "end")];

const FUNCS: &[&str] = &[
    "def",
    "defp",
    "defmacro",
    "defmacrop",
    "defguard",
    "defguardp",
    "defdelegate",
];

/// Symbols in Elixir tokens (as produced with [`ELIXIR_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, &[TokenClass::Comment]);
    // Drop block words that are keyword keys (`do:`), atoms (`:end`) or
    // field names (`x.end`), so `keyword_block` sees only real blocks.
    let adj = |a: usize, b: usize| tokens[a].span.end == tokens[b].span.start;
    let keep: Vec<usize> = code
        .iter()
        .enumerate()
        .filter(|&(k, &i)| {
            if !matches!(tokens[i].text.as_str(), "do" | "end" | "fn") {
                return true;
            }
            let key = i + 1 < tokens.len() && tokens[i + 1].text == ":" && adj(i, i + 1);
            let atom = k > 0 && {
                let p = code[k - 1];
                matches!(tokens[p].text.as_str(), ":" | ".") && adj(p, i)
            };
            !(key || atom)
        })
        .map(|(_, &i)| i)
        .collect();
    let toks: Vec<TokenDecl> = keep.iter().map(|&i| tokens[i].clone()).collect();
    let mut s = Scanner {
        toks: &toks,
        out: Vec::new(),
    };
    s.body(0, toks.len(), None);
    s.out
}

struct Scanner<'a> {
    /// Code tokens with non-block `do`/`end`/`fn` removed.
    toks: &'a [TokenDecl],
    out: Vec<SymbolDecl>,
}

impl Scanner<'_> {
    fn text(&self, p: usize) -> &str {
        &self.toks[p].text
    }

    fn line_start(&self, p: usize) -> bool {
        p == 0
            || self.toks[p].span.start_line > self.toks[p - 1].span.end_line
            || self.toks[p - 1].text == ";"
    }

    /// `A.B.C` starting at `p`: (name, last position).
    fn alias(&self, p: usize, hi: usize) -> Option<(String, usize)> {
        if p >= hi || self.toks[p].class != TokenClass::Identifier {
            return None;
        }
        let mut name = self.toks[p].text.clone();
        let mut last = p;
        while last + 2 < hi
            && self.text(last + 1) == "."
            && self.toks[last + 2].class == TokenClass::Identifier
            && self.toks[last].span.end == self.toks[last + 1].span.start
            && self.toks[last + 1].span.end == self.toks[last + 2].span.start
        {
            name.push('.');
            name.push_str(self.text(last + 2));
            last += 2;
        }
        Some((name, last))
    }

    /// Function name at `p`, with an adjacent `?` or `!` suffix.
    fn fn_name(&self, p: usize, hi: usize) -> Option<String> {
        if p >= hi || self.toks[p].class != TokenClass::Identifier {
            return None;
        }
        let mut name = self.toks[p].text.clone();
        if p + 1 < hi
            && matches!(self.text(p + 1), "?" | "!")
            && self.toks[p].span.end == self.toks[p + 1].span.start
        {
            name.push_str(self.text(p + 1));
        }
        Some(name)
    }

    /// The `do` block of the declaration at `p`: (`do`, `end`) positions.
    /// Searched at bracket depth 0 until a line starting at or left of the
    /// keyword's column.
    fn do_block(&self, p: usize, hi: usize) -> Option<(usize, usize)> {
        let col = self.toks[p].span.start_col;
        let mut depth = 0usize;
        for q in p + 1..hi {
            if depth == 0 && self.line_start(q) && self.toks[q].span.start_col <= col {
                return None;
            }
            match self.text(q) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                "do" if depth == 0 => {
                    let close = keyword_block(self.toks, q, PAIRS, false)?;
                    return (close < hi).then_some((q, close));
                }
                _ => {}
            }
        }
        None
    }

    /// Last position of the indented block headed by `p`, capped at `hi - 1`.
    fn stmt_end(&self, p: usize, hi: usize) -> usize {
        indent_block(self.toks, p, &[]).clamp(p, hi - 1)
    }

    fn emit(&mut self, name: String, kind: SymbolKind, lk: &str, first: usize, last: usize) {
        self.out.push(SymbolDecl {
            name,
            kind,
            lang_kind: Some(lk.to_string()),
            span: span_between(&self.toks[first].span, &self.toks[last].span),
        });
    }

    /// Declarations in positions `[lo, hi)`; `module` is the enclosing
    /// module's name.
    fn body(&mut self, lo: usize, hi: usize, module: Option<&str>) {
        let mut p = lo;
        while p < hi {
            if !self.line_start(p) {
                p += 1;
                continue;
            }
            let kw = self.text(p).to_string();
            match kw.as_str() {
                "defmodule" | "defprotocol" | "defimpl" => {
                    let Some((mut name, name_end)) = self.alias(p + 1, hi) else {
                        p += 1;
                        continue;
                    };
                    if kw == "defimpl" {
                        let for_at = (name_end + 1..hi.min(name_end + 4)).find(|&q| {
                            self.text(q) == "for" && q + 1 < hi && self.text(q + 1) == ":"
                        });
                        if let Some((target, _)) = for_at.and_then(|q| self.alias(q + 2, hi)) {
                            name = format!("{name} for {target}");
                        }
                    }
                    if let Some((open, close)) = self.do_block(p, hi) {
                        self.emit(name.clone(), SymbolKind::Type, &kw, p, close);
                        self.body(open + 1, close, Some(&name));
                        p = close + 1;
                    } else {
                        let end = self.stmt_end(p, hi);
                        self.emit(name, SymbolKind::Type, &kw, p, end);
                        p = end + 1;
                    }
                }
                k if FUNCS.contains(&k) => {
                    let end = match self.do_block(p, hi) {
                        Some((_, close)) => close,
                        None => self.stmt_end(p, hi),
                    };
                    if let Some(name) = self.fn_name(p + 1, hi) {
                        let kind = if module.is_some() {
                            SymbolKind::Method
                        } else {
                            SymbolKind::Function
                        };
                        self.emit(name, kind, &kw, p, end);
                    }
                    p = end + 1;
                }
                "defstruct" | "defexception" => {
                    let end = self.stmt_end(p, hi);
                    let name = format!("%{}{{}}", module.unwrap_or(""));
                    self.emit(name, SymbolKind::Type, &kw, p, end);
                    p = end + 1;
                }
                _ => p += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests;
