//! JavaScript extractor: a token-stream scanner, not a parser.
//!
//! | JavaScript | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `function name(...) {...}` (any depth, incl. `async`, generators) | Function | `function` |
//! | `class Name {...}` | Type | `class` |
//! | methods, getters, setters, constructors in a class body | Method | `method` `get` `set` `constructor` |
//! | `const/let/var name = function ...` | Function | `fn_expr` |
//! | `const/let/var name = (...) => ...` / `x => ...` | Function | `arrow_fn` |
//!
//! Spans run from the first keyword (`export`, `async`, `const`, ...) through
//! the closing `}` (or, for an arrow with an expression body, its `;`).
//! Regex literals are not recognized by the shared tokenizer, so a regex
//! containing an unbalanced brace can cut a scan short: the extractor then
//! returns fewer symbols, never invalid spans or `has_errors`.
use graph_core::scan::{matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct JavaScriptExtractor;

/// Tokenizer dialect used for JavaScript.
pub const JS_TOKENIZER: TokenizerOptions = TokenizerOptions {
    rust_literals: false,
    single_quote_strings: true,
    csharp_strings: false,
    markup: false,
    aspx: false,
};

impl Extractor for JavaScriptExtractor {
    fn language(&self) -> &str {
        "javascript"
    }

    fn extensions(&self) -> &[&str] {
        &["js", "mjs", "cjs", "jsx"]
    }

    fn version(&self) -> String {
        format!("javascript-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, JS_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in JavaScript tokens (as produced with [`JS_TOKENIZER`]); reusable
/// for script embedded in other files.
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.class != TokenClass::Comment)
        .map(|(i, _)| i)
        .collect();
    let s = Scanner {
        tokens,
        code: &code,
    };
    let mut out = Vec::new();
    s.scan(0, code.len(), &mut out);
    out
}

const PREFIXES: &[&str] = &["export", "default", "async", "declare"];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    fn is_ident(&self, c: usize) -> bool {
        self.tok(c).class == TokenClass::Identifier
    }

    fn close_of(&self, c: usize) -> Option<usize> {
        let close = matching_close(self.tokens, self.code[c])?;
        self.code.binary_search(&close).ok()
    }

    /// `=` directly followed by `>`.
    fn is_arrow(&self, c: usize, hi: usize) -> bool {
        c + 1 < hi
            && self.text(c) == "="
            && self.text(c + 1) == ">"
            && self.tok(c).span.end == self.tok(c + 1).span.start
    }

    /// Start of a declaration ending just before `c`: back over prefixes.
    fn start_of(&self, mut c: usize, lo: usize) -> usize {
        while c > lo && PREFIXES.contains(&self.text(c - 1)) {
            c -= 1;
        }
        c
    }

    fn push(
        &self,
        out: &mut Vec<SymbolDecl>,
        name: usize,
        kind: SymbolKind,
        lang: &str,
        span: (usize, usize),
    ) {
        out.push(SymbolDecl {
            name: self.text(name).to_string(),
            kind,
            lang_kind: Some(lang.into()),
            span: span_between(&self.tok(span.0).span, &self.tok(span.1).span),
        });
    }

    /// Scan code positions `[lo, hi)` for declarations, at any depth.
    fn scan(&self, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let mut c = lo;
        while c < hi {
            let next = match self.text(c) {
                "function" => self.function(c, lo, hi, out),
                "class" => self.class(c, lo, hi, out),
                "const" | "let" | "var" => self.binding(c, lo, hi, out),
                _ => None,
            };
            c = next.unwrap_or(c + 1);
        }
    }

    /// `function [*] name (...) {...}`; returns where to continue (inside
    /// the body, so nested functions are found too).
    fn function(
        &self,
        kw: usize,
        lo: usize,
        hi: usize,
        out: &mut Vec<SymbolDecl>,
    ) -> Option<usize> {
        let mut c = kw + 1;
        if c < hi && self.text(c) == "*" {
            c += 1;
        }
        if c + 1 >= hi || !self.is_ident(c) || self.text(c + 1) != "(" {
            return None;
        }
        let name = c;
        let params_close = self.close_of(c + 1).filter(|&p| p < hi)?;
        let open = params_close + 1;
        if open >= hi || self.text(open) != "{" {
            return None;
        }
        let close = self.close_of(open).filter(|&p| p < hi)?;
        let start = self.start_of(kw, lo);
        self.push(out, name, SymbolKind::Function, "function", (start, close));
        Some(open + 1)
    }

    /// `class Name [extends X] {...}` and its methods.
    fn class(&self, kw: usize, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) -> Option<usize> {
        let name = kw + 1;
        if name >= hi || !self.is_ident(name) {
            return None;
        }
        let open = (name + 1..hi).find(|&c| matches!(self.text(c), "{" | ";" | "}"))?;
        if self.text(open) != "{" {
            return None;
        }
        let close = self.close_of(open).filter(|&p| p < hi)?;
        let start = self.start_of(kw, lo);
        self.push(out, name, SymbolKind::Type, "class", (start, close));
        self.class_body(open + 1, close, out);
        Some(close + 1)
    }

    fn class_body(&self, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let mut c = lo;
        while c < hi {
            // Member start: modifiers, then a name, then `(`.
            let start = c;
            let mut lang = "method";
            let mut n = c;
            while n < hi && matches!(self.text(n), "static" | "async" | "get" | "set" | "*" | "#") {
                if matches!(self.text(n), "get" | "set") && n + 1 < hi && self.text(n + 1) != "(" {
                    lang = if self.text(n) == "get" { "get" } else { "set" };
                }
                n += 1;
            }
            if n + 1 < hi && self.is_ident(n) && self.text(n + 1) == "(" {
                if let Some(params) = self.close_of(n + 1).filter(|&p| p + 1 < hi) {
                    if self.text(params + 1) == "{" {
                        if let Some(close) = self.close_of(params + 1).filter(|&p| p < hi) {
                            if self.text(n) == "constructor" {
                                lang = "constructor";
                            }
                            self.push(out, n, SymbolKind::Method, lang, (start, close));
                            self.scan(params + 2, close, out);
                            c = close + 1;
                            continue;
                        }
                    }
                }
            }
            // Skip anything else (fields, stray tokens), whole groups at a time.
            c = match self.text(c) {
                "(" | "[" | "{" => self.close_of(c).map_or(hi, |p| p + 1),
                _ => c + 1,
            };
        }
    }

    /// `const name = function ... {}` or `const name = (...) => ...`.
    fn binding(&self, kw: usize, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) -> Option<usize> {
        let name = kw + 1;
        if name + 2 >= hi
            || !self.is_ident(name)
            || self.text(name + 1) != "="
            || self.is_arrow(name + 1, hi)
        {
            return None;
        }
        let mut v = name + 2;
        if self.text(v) == "async" {
            v += 1;
        }
        if v >= hi {
            return None;
        }
        let start = self.start_of(kw, lo);
        if self.text(v) == "function" {
            let mut p = v + 1;
            if p < hi && self.text(p) == "*" {
                p += 1;
            }
            if p < hi && self.is_ident(p) {
                p += 1;
            }
            if p >= hi || self.text(p) != "(" {
                return None;
            }
            let params = self.close_of(p).filter(|&x| x + 1 < hi)?;
            if self.text(params + 1) != "{" {
                return None;
            }
            let close = self.close_of(params + 1).filter(|&x| x < hi)?;
            let end = self.semi_after(close, hi);
            self.push(out, name, SymbolKind::Function, "fn_expr", (start, end));
            // Continue at `function` so a named function expression is found too.
            return Some(v);
        }
        // Arrow: `(...) =>` or `x =>`.
        let arrow = if self.text(v) == "(" {
            self.close_of(v).map(|p| p + 1)?
        } else if self.is_ident(v) {
            v + 1
        } else {
            return None;
        };
        if !self.is_arrow(arrow, hi) {
            return None;
        }
        let body = arrow + 2;
        if body >= hi {
            return None;
        }
        let end = if self.text(body) == "{" {
            let close = self.close_of(body).filter(|&x| x < hi)?;
            self.semi_after(close, hi)
        } else {
            self.expression_end(body, hi)?
        };
        self.push(out, name, SymbolKind::Function, "arrow_fn", (start, end));
        Some(body)
    }

    /// `close`, or the `;` right after it.
    fn semi_after(&self, close: usize, hi: usize) -> usize {
        if close + 1 < hi && self.text(close + 1) == ";" {
            close + 1
        } else {
            close
        }
    }

    /// Last token of an arrow's expression body starting at `c`: up to a `;`
    /// (included) or `,` at this level, a closer of the enclosing group, or
    /// the end of the line the expression ends on.
    fn expression_end(&self, mut c: usize, hi: usize) -> Option<usize> {
        let mut last = c;
        while c < hi {
            match self.text(c) {
                ";" => return Some(c),
                "," | ")" | "]" | "}" => return Some(last),
                "(" | "[" | "{" => {
                    let close = self.close_of(c).filter(|&x| x < hi)?;
                    last = close;
                    c = close + 1;
                    continue;
                }
                _ => {}
            }
            if c > last && self.tok(c).span.start_line > self.tok(last).span.end_line {
                return Some(last);
            }
            last = c;
            c += 1;
        }
        Some(last)
    }
}

#[cfg(test)]
mod tests;
