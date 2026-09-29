//! Go extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer's Go dialect (raw `` `...` ``
//! strings). Only file-level declarations are symbols:
//!
//! | Go | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `package` clause | Module | `package` |
//! | `type X struct {...}` / `interface {...}` | Type | `struct` / `interface` |
//! | other `type` specs (`type X int`, `type X = Y`) | Type | `type` |
//! | `func` | Function | `func` |
//! | `func (r T) M()` (a method with a receiver) | Method | `method` |
//! | `const` / `var` specs (grouped or not) | Constant / Variable | `const` / `var` |
//!
//! The package symbol spans from `package` to the end of the file, so every
//! other symbol nests in it. A method is not nested under its receiver
//! type (like a Rust `impl` method, it is a sibling of the type) and is
//! named without the receiver (`Area`, not `Rect.Area`), so methods do not
//! roll up under their type with `--grain class`: `--grain method` finds
//! them, and the `func (r Rect)` receiver is in the span text. A
//! declaration runs from its keyword (or, inside a `( ... )` group, from its
//! first name) through its closing `}` or the last token before Go's
//! automatic semicolon. Function bodies, struct fields and interface
//! methods are not scanned. A multi-name spec (`var a, b = 1, 2`) yields one
//! symbol named by its first name. Odd input never sets `has_errors`.
use graph_core::scan::{code_index, matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct GoExtractor;

impl Extractor for GoExtractor {
    fn language(&self) -> &str {
        "go"
    }

    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn version(&self) -> String {
        format!("go-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, TokenizerOptions::GO);
        let code = code_index(&tokens, &[TokenClass::Comment]);
        let mut s = Scanner {
            tokens: &tokens,
            code: &code,
            out: Vec::new(),
        };
        s.file();
        let symbols = s.out;
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    out: Vec<SymbolDecl>,
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

    fn push(&mut self, n: usize, kind: SymbolKind, lang: &str, first: usize, last: usize) {
        self.out.push(SymbolDecl {
            name: self.text(n).to_string(),
            kind,
            lang_kind: Some(lang.to_string()),
            span: span_between(&self.tok(first).span, &self.tok(last).span),
        });
    }

    /// Whether Go inserts a semicolon after the token at `c` if a newline
    /// follows it (identifiers and keywords, literals, closing brackets).
    fn ends_statement(&self, c: usize) -> bool {
        let t = self.tok(c);
        matches!(t.class, TokenClass::Identifier | TokenClass::Literal)
            || matches!(t.text.as_str(), ")" | "]" | "}")
            || (matches!(t.text.as_str(), "+" | "-")
                && c > 0
                && self.text(c - 1) == t.text
                && self.tok(c - 1).span.end == t.span.start)
    }

    /// Last code position of the statement starting at `c`, within `[c, hi)`.
    /// With `body`, the first `{` not following `struct`/`interface` is a
    /// function body that ends the statement.
    fn statement_end(&self, mut c: usize, hi: usize, body: bool) -> usize {
        loop {
            let e = match self.text(c) {
                "{" if body && c > 0 && !matches!(self.text(c - 1), "struct" | "interface") => {
                    return self.close_of(c).filter(|&x| x < hi).unwrap_or(hi - 1);
                }
                "(" | "[" | "{" => match self.close_of(c) {
                    Some(x) if x < hi => x,
                    _ => return hi - 1,
                },
                _ => c,
            };
            if e + 1 >= hi || self.text(e + 1) == ";" {
                return e;
            }
            if self.tok(e + 1).span.start_line > self.tok(e).span.end_line && self.ends_statement(e)
            {
                return e;
            }
            c = e + 1;
        }
    }

    fn file(&mut self) {
        let hi = self.code.len();
        let mut c = 0;
        while c < hi {
            match self.text(c) {
                "package" if c + 1 < hi && self.is_ident(c + 1) => {
                    self.push(c + 1, SymbolKind::Module, "package", c, hi - 1);
                    c += 2;
                }
                "func" if c + 1 < hi => {
                    let end = self.statement_end(c + 1, hi, true);
                    let mut n = c + 1;
                    let mut method = false;
                    if self.text(n) == "(" {
                        n = self.close_of(n).map_or(hi, |x| x + 1);
                        method = true;
                    }
                    if n <= end && self.is_ident(n) {
                        let (kind, lang) = if method {
                            (SymbolKind::Method, "method")
                        } else {
                            (SymbolKind::Function, "func")
                        };
                        self.push(n, kind, lang, c, end);
                    }
                    c = end + 1;
                }
                kw @ ("type" | "const" | "var" | "import") if c + 1 < hi => {
                    let kw = kw.to_string();
                    if self.text(c + 1) == "(" {
                        let close = self.close_of(c + 1).unwrap_or(hi);
                        let mut s = c + 2;
                        while s < close {
                            if self.text(s) == ";" {
                                s += 1;
                                continue;
                            }
                            let end = self.statement_end(s, close, false);
                            self.spec(&kw, s, s, end);
                            s = end + 1;
                        }
                        c = close + 1;
                    } else {
                        let end = self.statement_end(c + 1, hi, false);
                        self.spec(&kw, c, c + 1, end);
                        c = end + 1;
                    }
                }
                "(" | "[" | "{" => c = self.close_of(c).map_or(c + 1, |x| x + 1),
                _ => c += 1,
            }
        }
    }

    /// A `type`/`const`/`var` spec whose name is at `n`, spanning `first..=last`.
    fn spec(&mut self, kw: &str, first: usize, n: usize, last: usize) {
        if !self.is_ident(n) || n > last {
            return;
        }
        match kw {
            "type" => {
                // Skip type parameters `[T any]` to find `struct`/`interface`.
                let mut k = n + 1;
                if k <= last && self.text(k) == "[" {
                    k = self.close_of(k).map_or(last + 1, |x| x + 1);
                }
                let lang = match (k <= last).then(|| self.text(k)) {
                    Some("struct") => "struct",
                    Some("interface") => "interface",
                    _ => "type",
                };
                self.push(n, SymbolKind::Type, lang, first, last);
            }
            "const" => self.push(n, SymbolKind::Constant, "const", first, last),
            "var" => self.push(n, SymbolKind::Variable, "var", first, last),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
