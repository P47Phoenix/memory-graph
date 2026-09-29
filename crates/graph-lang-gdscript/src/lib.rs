//! GDScript extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer (`GDSCRIPT` dialect, which is the
//! Python one). Blocks follow indentation (`graph_core::scan::indent_block`).
//!
//! | GDScript | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `class_name X` (the file's class, spanning the whole file) | Type | `class_name` |
//! | `class X:` (inner class) | Type | `class` |
//! | `enum X { ... }` | Type | `enum` |
//! | `func` / `static func` | Method | `func` / `static func` |
//! | `signal x(...)` | Other | `signal` |
//! | `const X` | Constant | `const` |
//! | `var x` (`@export var`, `@onready var`, `static var`) | Variable | `var` |
//!
//! A GDScript file is a class, so `func` is always a Method, even in a file
//! with no `class_name`. A declaration's span runs from its first token
//! (annotations such as `@export` and `static` included) through the last
//! token of its indented block. Function bodies are not scanned.
//!
//! Known limits: a file with `extends` but no `class_name` gets no file
//! Type (it has no name of its own); anonymous `enum { ... }` is not a
//! symbol; a statement after `;` on a block header line belongs to that
//! block.
use graph_core::scan::{code_index, indent_block, matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct GdscriptExtractor;

/// Tokenizer dialect used for GDScript.
pub const GDSCRIPT_TOKENIZER: TokenizerOptions = TokenizerOptions::GDSCRIPT;

impl Extractor for GdscriptExtractor {
    fn language(&self) -> &str {
        "gdscript"
    }

    fn extensions(&self) -> &[&str] {
        &["gd"]
    }

    fn version(&self) -> String {
        format!("gdscript-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, GDSCRIPT_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

const SKIP: &[TokenClass] = &[TokenClass::Comment];

/// Symbols in GDScript tokens (as produced with [`GDSCRIPT_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, SKIP);
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
        class_name: None,
    };
    s.body(0, code.len(), true);
    let mut out = Vec::with_capacity(s.out.len() + 1);
    if let (Some(name), Some(&first), Some(&last)) = (s.class_name, code.first(), code.last()) {
        out.push(SymbolDecl {
            name,
            kind: SymbolKind::Type,
            lang_kind: Some("class_name".into()),
            span: span_between(&tokens[first].span, &tokens[last].span),
        });
    }
    out.extend(s.out);
    out
}

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    out: Vec<SymbolDecl>,
    class_name: Option<String>,
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    fn ident(&self, c: usize, hi: usize) -> Option<String> {
        (c < hi && self.tok(c).class == TokenClass::Identifier).then(|| self.text(c).to_string())
    }

    /// Code position of the last token of the indented block headed by `c`,
    /// capped at `hi - 1`.
    fn block_end(&self, c: usize, hi: usize) -> usize {
        let last = indent_block(self.tokens, self.code[c], SKIP);
        let pos = self.code.partition_point(|&i| i <= last).saturating_sub(1);
        pos.clamp(c, hi - 1)
    }

    /// End of a simple statement at `c`: its block, cut before a `;` at
    /// bracket depth 0. Returns (last symbol token, last consumed token).
    fn stmt_end(&self, c: usize, hi: usize) -> (usize, usize) {
        let end = self.block_end(c, hi);
        let mut depth = 0usize;
        for k in c..=end {
            match self.text(k) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                ";" if depth == 0 && k > c => return (k - 1, k),
                _ => {}
            }
        }
        (end, end)
    }

    fn emit(&mut self, name: String, kind: SymbolKind, lk: &str, first: usize, last: usize) {
        let span = span_between(&self.tok(first).span, &self.tok(last).span);
        self.out.push(SymbolDecl {
            name,
            kind,
            lang_kind: Some(lk.to_string()),
            span,
        });
    }

    /// Declarations in code positions `[lo, hi)`.
    fn body(&mut self, lo: usize, hi: usize, top: bool) {
        let mut c = lo;
        while c < hi {
            if self.text(c) == ";" {
                c += 1;
                continue;
            }
            let start = c;
            // Annotations: `@name` or `@name(...)`, possibly on earlier lines.
            while c + 1 < hi
                && self.text(c) == "@"
                && self.tok(c + 1).class == TokenClass::Identifier
            {
                c += 2;
                if c < hi && self.text(c) == "(" {
                    match matching_close(self.tokens, self.code[c])
                        .and_then(|cl| self.code.binary_search(&cl).ok())
                    {
                        Some(cl) if cl < hi => c = cl + 1,
                        _ => break,
                    }
                }
            }
            if c >= hi {
                break;
            }
            let is_static = self.text(c) == "static" && c + 1 < hi;
            let kw = if is_static { c + 1 } else { c };
            let (last, next) = match self.text(kw) {
                "func" | "class" => {
                    let e = self.block_end(kw, hi);
                    (e, e)
                }
                _ => self.stmt_end(kw, hi),
            };
            let name = self.ident(kw + 1, hi);
            match (self.text(kw), name) {
                ("func", Some(n)) => {
                    let lk = if is_static { "static func" } else { "func" };
                    self.emit(n, SymbolKind::Method, lk, start, last);
                }
                ("class", Some(n)) => {
                    self.emit(n, SymbolKind::Type, "class", start, last);
                    // The body starts after the header's `:`.
                    if let Some(colon) = (kw + 2..=last).find(|&k| self.text(k) == ":") {
                        self.body(colon + 1, last + 1, false);
                    }
                }
                ("class_name", Some(n)) if top && self.class_name.is_none() => {
                    self.class_name = Some(n);
                }
                ("enum", Some(n)) => self.emit(n, SymbolKind::Type, "enum", start, last),
                ("signal", Some(n)) => self.emit(n, SymbolKind::Other, "signal", start, last),
                ("const", Some(n)) => self.emit(n, SymbolKind::Constant, "const", start, last),
                ("var", Some(n)) => self.emit(n, SymbolKind::Variable, "var", start, last),
                _ => {}
            }
            c = next + 1;
        }
    }
}

#[cfg(test)]
mod tests;
