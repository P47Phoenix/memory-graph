//! Scala extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer's Scala dialect (raw `"""..."""`
//! strings). Declarations are found at file, package and template level,
//! in both brace syntax and Scala 3 indentation syntax:
//!
//! | Scala | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `package a.b` (braced or to end of file) | Module | `package` |
//! | `object` / `case object` / `package object` | Module | `object` / `case object` / `package object` |
//! | `class` / `case class` / `trait` / `enum` | Type | `class` / `case class` / `trait` / `enum` |
//! | `type` alias or abstract type | Type | `type` |
//! | `def` inside a class/trait/enum | Method | `def` |
//! | `def` elsewhere (top level, in an object) | Function | `def` |
//! | `val` / `var` | Variable | `val` / `var` |
//!
//! A declaration's span starts at its first modifier or annotation and
//! runs through the last token before a `;`, the enclosing closer, or the
//! next line that starts at or left of the declaration's column outside
//! brackets. Bodies of `def`/`val`/`var` are not scanned. Odd input never
//! sets `has_errors`.
//!
//! Known limits: pattern definitions (`val (a, b) = ...`) and `given`
//! instances are not symbols; a `val x, y = 1` yields `x` only; an
//! `extension` method is a top-level function; a declaration whose
//! continuation lines are indented less than its first token ends early.
use graph_core::scan::{code_index, matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct ScalaExtractor;

impl Extractor for ScalaExtractor {
    fn language(&self) -> &str {
        "scala"
    }

    fn extensions(&self) -> &[&str] {
        &["scala", "sc"]
    }

    fn version(&self) -> String {
        format!("scala-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, TokenizerOptions::SCALA);
        let code = code_index(&tokens, &[TokenClass::Comment]);
        let mut s = Scanner {
            tokens: &tokens,
            code: &code,
            out: Vec::new(),
        };
        s.body(0, code.len(), false);
        let symbols = s.out;
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

const MODIFIERS: &[&str] = &[
    "private",
    "protected",
    "final",
    "sealed",
    "abstract",
    "implicit",
    "lazy",
    "override",
    "inline",
    "opaque",
    "open",
    "transparent",
    "infix",
    "erased",
    "case",
];

const DECL_KEYWORDS: &[&str] = &[
    "package", "object", "class", "trait", "enum", "def", "val", "var", "type",
];

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

    fn adjacent(&self, a: usize, b: usize) -> bool {
        self.tok(a).span.end == self.tok(b).span.start
    }

    fn new_line(&self, c: usize) -> bool {
        c > 0 && self.tok(c).span.start_line > self.tok(c - 1).span.end_line
    }

    /// Declarations in code positions `[lo, hi)`; `in_type` for the body
    /// of a class, trait or enum.
    fn body(&mut self, lo: usize, hi: usize, in_type: bool) {
        let mut c = lo;
        while c < hi {
            if c > lo && self.text(c - 1) == "." {
                c += 1;
                continue;
            }
            if let Some(kw) = self.keyword_after_modifiers(c, hi) {
                c = self.declaration(c, kw, hi, in_type) + 1;
                continue;
            }
            c = match self.text(c) {
                "(" | "[" | "{" => match self.close_of(c) {
                    Some(x) if x < hi => x + 1,
                    _ => c + 1,
                },
                _ => c + 1,
            };
        }
    }

    /// If modifiers and annotations starting at `c` lead to a declaration
    /// keyword, its position.
    fn keyword_after_modifiers(&self, c: usize, hi: usize) -> Option<usize> {
        let mut k = c;
        while k < hi {
            let t = self.text(k);
            if DECL_KEYWORDS.contains(&t) {
                return Some(k);
            }
            if MODIFIERS.contains(&t) {
                k += 1;
                // `private[pkg]`
                if k < hi && self.text(k) == "[" && self.adjacent(k - 1, k) {
                    k = self.close_of(k)? + 1;
                }
            } else if t == "@" && k + 1 < hi && self.is_ident(k + 1) {
                k += 2;
                while k + 1 < hi && self.text(k) == "." && self.is_ident(k + 1) {
                    k += 2;
                }
                // Annotation arguments: `@deprecated("x")`, `@throws[E]`.
                while k < hi && matches!(self.text(k), "(" | "[") && self.adjacent(k - 1, k) {
                    k = self.close_of(k)? + 1;
                }
            } else {
                return None;
            }
        }
        None
    }

    /// Last code position of the declaration whose first token is `start`
    /// and keyword `kw`, within `[start, hi)`.
    fn decl_end(&self, start: usize, kw: usize, hi: usize) -> usize {
        let col = self.tok(start).span.start_col;
        let mut depth = 0usize;
        let mut last = kw;
        for k in kw + 1..hi {
            let t = self.text(k);
            if depth == 0 {
                if t == ";" {
                    break;
                }
                if self.new_line(k) && self.tok(k).span.start_col <= col {
                    break;
                }
            }
            match t {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            last = k;
        }
        last
    }

    fn push(&mut self, name: String, kind: SymbolKind, lang: &str, first: usize, last: usize) {
        self.out.push(SymbolDecl {
            name,
            kind,
            lang_kind: Some(lang.to_string()),
            span: span_between(&self.tok(first).span, &self.tok(last).span),
        });
    }

    /// Record the declaration starting at `start` with keyword `kw`; returns
    /// its last code position.
    fn declaration(&mut self, start: usize, kw: usize, hi: usize, in_type: bool) -> usize {
        let case = (start..kw).any(|c| self.text(c) == "case");
        let mut kw = kw;
        let mut lang = self.text(kw).to_string();
        if lang == "package" && kw + 1 < hi && self.text(kw + 1) == "object" {
            kw += 1;
            lang = "package object".into();
        } else if case {
            lang = format!("case {lang}");
        }
        if self.text(kw) == "package" {
            return self.package(start, kw, hi);
        }
        let end = self.decl_end(start, kw, hi);
        let n = kw + 1;
        if n > end {
            return end;
        }
        let name = match self.text(kw) {
            "def" => self.def_name(n, end),
            _ if self.is_ident(n) => Some(self.text(n).to_string()),
            _ => None,
        };
        let Some(name) = name else {
            return end;
        };
        let (kind, body) = match self.text(kw) {
            "object" => (SymbolKind::Module, Some(false)),
            "class" | "trait" | "enum" => (SymbolKind::Type, Some(true)),
            "type" => (SymbolKind::Type, None),
            "def" if in_type => (SymbolKind::Method, None),
            "def" => (SymbolKind::Function, None),
            _ => (SymbolKind::Variable, None),
        };
        self.push(name, kind, &lang, start, end);
        if let Some(in_type) = body {
            if let Some((lo, hi)) = self.template_body(n + 1, end) {
                self.body(lo, hi, in_type);
            }
        }
        end
    }

    /// `package a.b { ... }` or `package a.b` running to the end of `hi`.
    fn package(&mut self, start: usize, kw: usize, hi: usize) -> usize {
        let mut k = kw + 1;
        let mut name = String::new();
        while k < hi && !self.new_line(k) && (self.is_ident(k) || self.text(k) == ".") {
            name.push_str(self.text(k));
            k += 1;
        }
        if name.is_empty() {
            return kw;
        }
        if k < hi && self.text(k) == "{" {
            let close = self.close_of(k).filter(|&x| x < hi);
            let last = close.unwrap_or(hi - 1);
            self.push(name, SymbolKind::Module, "package", start, last);
            self.body(k + 1, close.unwrap_or(hi), false);
            return last;
        }
        self.push(name, SymbolKind::Module, "package", start, hi - 1);
        self.body(k, hi, false);
        hi - 1
    }

    /// A `def` name: an identifier, a backquoted name or an operator.
    fn def_name(&self, n: usize, end: usize) -> Option<String> {
        let mut name = String::new();
        if self.text(n).len() > 1 && self.text(n).starts_with('`') {
            return Some(self.text(n).to_string());
        }
        if self.text(n) == "`" {
            let mut k = n;
            while k <= end {
                name.push_str(self.text(k));
                if k > n && self.text(k) == "`" {
                    break;
                }
                k += 1;
            }
            return Some(name);
        }
        let is_op = |k: usize| {
            let t = self.tok(k);
            t.class == TokenClass::Operator || t.text == ":"
        };
        // `name`, `unary_!`, `foo_=`, or an operator name `+`, `::`, `<=`.
        let first_ident = self.is_ident(n);
        if !first_ident && !is_op(n) {
            return None;
        }
        name.push_str(self.text(n));
        let mut k = n + 1;
        while k <= end
            && self.adjacent(k - 1, k)
            && is_op(k)
            && (!first_ident || name.ends_with('_') || k > n + 1)
        {
            name.push_str(self.text(k));
            k += 1;
        }
        Some(name)
    }
    /// The body of a class/object/trait/enum whose header starts at `from`:
    /// a `{ ... }` or a Scala 3 `:` ending the header line.
    fn template_body(&self, from: usize, end: usize) -> Option<(usize, usize)> {
        let mut k = from;
        while k <= end {
            match self.text(k) {
                "{" => {
                    let close = self.close_of(k).filter(|&x| x <= end).unwrap_or(end + 1);
                    return Some((k + 1, close));
                }
                "(" | "[" => k = self.close_of(k).filter(|&x| x <= end)? + 1,
                "=" => return None,
                ":" if k == end || self.new_line(k + 1) => return Some((k + 1, end + 1)),
                _ => k += 1,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
