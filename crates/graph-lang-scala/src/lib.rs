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
//! | `given name: T with { ... }` | Type | `given` |
//! | `def` inside a class/trait/enum/given | Method | `def` |
//! | `def` elsewhere (top level, in an object) | Function | `def` |
//! | `val` / `var` | Variable | `val` / `var` |
//! | `case X` in an `enum` body | Constant | `case` |
//!
//! Objects are modules, not types, so a `def` in an `object` is a
//! Function (it does not roll up under the `class` grain).
//!
//! A declaration's span starts at its first modifier or annotation and
//! runs through the last token before a `;`, the enclosing closer, or the
//! next line that starts at or left of the declaration's column outside
//! brackets. Bodies of `def`/`val`/`var` are not scanned. Odd input never
//! sets `has_errors`.
//!
//! Known limits: pattern definitions (`val (a, b) = ...`), anonymous
//! `given`s and `given` aliases' bodies are not symbols; a `val x, y = 1`
//! yields `x` only; in `case A, B` the first case spans from `case`, the
//! others are just their name; an `extension` method is a top-level
//! function; a declaration whose continuation lines are indented less than
//! its first token ends early. Bodies nested deeper than 64 levels are not
//! scanned.
use graph_core::scan::{code_close_table, code_index, mark_keywords, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct ScalaExtractor;

/// Scala 2 and 3 hard keywords. Soft keywords (`using`, `extension`,
/// `inline`, `opaque`, `open`, `end`, `derives`, ...) stay identifiers.
const KEYWORDS: &[&str] = &[
    "abstract",
    "case",
    "catch",
    "class",
    "def",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "for",
    "forSome",
    "given",
    "if",
    "implicit",
    "import",
    "lazy",
    "macro",
    "match",
    "new",
    "null",
    "object",
    "override",
    "package",
    "private",
    "protected",
    "return",
    "sealed",
    "super",
    "then",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "val",
    "var",
    "while",
    "with",
    "yield",
];

impl Extractor for ScalaExtractor {
    fn language(&self) -> &str {
        "scala"
    }

    fn extensions(&self) -> &[&str] {
        &["scala", "sc"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("scala-scan-1+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, TokenizerOptions::SCALA);
        let code = code_index(&tokens, &[TokenClass::Comment]);
        let mut s = Scanner {
            tokens: &tokens,
            code: &code,
            closes: code_close_table(&tokens, &code),
            out: Vec::new(),
            depth: 0,
            modifier_walks: RefCell::default(),
        };
        s.body(0, code.len(), Ctx::Other);
        let symbols = s.out;
        // After the symbol scan. A backticked name (`type`) is a single
        // token, so it never matches the list.
        mark_keywords(&mut tokens, KEYWORDS, |_, _| false);
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
    "package", "object", "class", "trait", "enum", "def", "val", "var", "type", "given",
];

/// Nesting depth past which bodies are not scanned (deeper symbols are
/// dropped), so that pathological input cannot overflow the stack.
const MAX_DEPTH: usize = 64;

/// What encloses a body.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    /// File, package or object: `def` is a Function.
    Other,
    /// Class, trait or given: `def` is a Method.
    Type,
    /// Enum: like `Type`, and `case X` is a constant.
    Enum,
}

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    /// [`code_close_table`] of `code`: closers found in one linear pass.
    closes: Vec<Option<usize>>,
    out: Vec<SymbolDecl>,
    depth: usize,
    /// `(c, hi)` -> [`Scanner::keyword_after_modifiers`] from `c`.
    modifier_walks: RefCell<HashMap<(usize, usize), Option<usize>>>,
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
        self.closes[c]
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        self.tok(a).span.end == self.tok(b).span.start
    }

    fn new_line(&self, c: usize) -> bool {
        c > 0 && self.tok(c).span.start_line > self.tok(c - 1).span.end_line
    }

    /// Declarations in code positions `[lo, hi)`; `in_type` for the body
    /// of a class, trait or enum.
    fn body(&mut self, lo: usize, hi: usize, ctx: Ctx) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        self.body_inner(lo, hi, ctx);
        self.depth -= 1;
    }

    fn body_inner(&mut self, lo: usize, hi: usize, ctx: Ctx) {
        let mut c = lo;
        while c < hi {
            if c > lo && self.text(c - 1) == "." {
                c += 1;
                continue;
            }
            if ctx == Ctx::Enum
                && self.text(c) == "case"
                && c + 1 < hi
                && self.is_ident(c + 1)
                && !matches!(self.text(c + 1), "class" | "object")
            {
                c = self.enum_case(c, hi) + 1;
                continue;
            }
            if let Some(kw) = self.keyword_after_modifiers(c, hi) {
                c = self.declaration(c, kw, hi, ctx) + 1;
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
    ///
    /// The walk depends only on the position, so every position it passes
    /// gets the same answer; remembering them keeps a long run of modifiers
    /// (`case case ...`) linear instead of one walk per token.
    fn keyword_after_modifiers(&self, c: usize, hi: usize) -> Option<usize> {
        let mut visited = Vec::new();
        let r = self.keyword_after_modifiers_walk(c, hi, &mut visited);
        let mut memo = self.modifier_walks.borrow_mut();
        for v in visited {
            memo.insert((v, hi), r);
        }
        r
    }

    fn keyword_after_modifiers_walk(
        &self,
        c: usize,
        hi: usize,
        visited: &mut Vec<usize>,
    ) -> Option<usize> {
        let mut k = c;
        while k < hi {
            if let Some(&r) = self.modifier_walks.borrow().get(&(k, hi)) {
                return r;
            }
            visited.push(k);
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
            owner: None,
            name,
            kind,
            lang_kind: Some(lang.to_string()),
            span: span_between(&self.tok(first).span, &self.tok(last).span),
        });
    }

    /// Record the declaration starting at `start` with keyword `kw`; returns
    /// its last code position.
    /// `case A, B` or `case X(..) extends E` in an enum body; returns its
    /// last code position.
    fn enum_case(&mut self, start: usize, hi: usize) -> usize {
        let end = self.decl_end(start, start, hi);
        let n = start + 1;
        let single = n + 1 > end || self.text(n + 1) != ",";
        let name = self.text(n).to_string();
        self.push(
            name,
            SymbolKind::Constant,
            "case",
            start,
            if single { end } else { n },
        );
        if !single {
            let mut k = n + 1;
            while k < end && self.text(k) == "," && self.is_ident(k + 1) {
                let name = self.text(k + 1).to_string();
                self.push(name, SymbolKind::Constant, "case", k + 1, k + 1);
                k += 2;
            }
        }
        end
    }

    fn declaration(&mut self, start: usize, kw: usize, hi: usize, ctx: Ctx) -> usize {
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
            // `given name: T with ...`; anonymous givens are not symbols.
            "given" => (self.is_ident(n) && n < end && matches!(self.text(n + 1), ":" | "[" | "("))
                .then(|| self.text(n).to_string()),
            _ if self.is_ident(n) => Some(self.text(n).to_string()),
            _ => None,
        };
        let Some(name) = name else {
            return end;
        };
        let (kind, body) = match self.text(kw) {
            "object" => (SymbolKind::Module, Some(Ctx::Other)),
            "class" | "trait" | "given" => (SymbolKind::Type, Some(Ctx::Type)),
            "enum" => (SymbolKind::Type, Some(Ctx::Enum)),
            "type" => (SymbolKind::Type, None),
            "def" if ctx != Ctx::Other => (SymbolKind::Method, None),
            "def" => (SymbolKind::Function, None),
            _ => (SymbolKind::Variable, None),
        };
        self.push(name, kind, &lang, start, end);
        if let Some(inner) = body {
            if let Some((lo, hi)) = self.template_body(n + 1, end) {
                self.body(lo, hi, inner);
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
            self.body(k + 1, close.unwrap_or(hi), Ctx::Other);
            return last;
        }
        self.push(name, SymbolKind::Module, "package", start, hi - 1);
        self.body(k, hi, Ctx::Other);
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
                ":" | "with" if k == end || self.new_line(k + 1) => return Some((k + 1, end + 1)),
                _ => k += 1,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
