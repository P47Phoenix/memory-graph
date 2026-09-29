//! F# extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer (`FSHARP` dialect). F# is
//! layout-sensitive: a declaration runs through its indented block
//! (`graph_core::scan::indent_block`).
//!
//! | F# | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `namespace` | Module | `namespace` |
//! | `module X` (top-level) / `module X =` (nested) | Module | `module` |
//! | `type` (and `and` after a type) | Type | `record` `union` `class` `interface` `struct` `delegate` `type` |
//! | `exception` | Type | `exception` |
//! | `let` with parameters | Function (Method in a type) | `let` |
//! | `let` without parameters | Variable (Constant with `[<Literal>]`) | `let` |
//! | `member` `override` `default` `abstract` `static member` `new` | Method | the keyword(s) |
//! | `val`, `member val` | Variable | `val` / `member val` |
//!
//! A namespace, or a top-level `module X` without `=`, runs to the next
//! namespace or the end of the file. A declaration's span starts at its
//! attributes (`[<...>]`) and runs through its indented block. A type's
//! kind comes from its body: `{` is a record, `|` or `X of ...` a union,
//! `class`/`interface`/`struct`/`delegate` as written, only `abstract`
//! members an interface, other members (or a primary constructor) a class,
//! anything else (an abbreviation) `type`. Member names drop the
//! self-identifier (`this.Area` is `Area`). Function bodies are not scanned.
//!
//! Known limits: `let` patterns (`let a, b = ...`) and backtick names
//! (` ``a b`` `) are not symbols; members inside an object expression
//! (`{ new IDisposable with ... }`) at a type's top level are reported as
//! members of that type; light-off (`#light "off"`) syntax is not handled.
use graph_core::scan::{code_index, indent_block, matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct FSharpExtractor;

/// Tokenizer dialect used for F#.
pub const FSHARP_TOKENIZER: TokenizerOptions = TokenizerOptions::FSHARP;

impl Extractor for FSharpExtractor {
    fn language(&self) -> &str {
        "fsharp"
    }

    fn extensions(&self) -> &[&str] {
        &["fs", "fsi", "fsx"]
    }

    fn version(&self) -> String {
        format!("fsharp-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, FSHARP_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

const SKIP: &[TokenClass] = &[TokenClass::Comment];

const ACCESS: &[&str] = &["private", "internal", "public"];

/// Words that start a member of a type.
const MEMBER_WORDS: &[&str] = &[
    "member", "override", "default", "abstract", "static", "new", "val",
];

/// Symbols in F# tokens (as produced with [`FSHARP_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    // `#` directives (`#r`, `#load`, `#if`) are dropped with their line.
    let mut code = Vec::with_capacity(tokens.len());
    let mut skip_line = None;
    let mut prev_line = 0;
    for i in code_index(tokens, SKIP) {
        let t = &tokens[i];
        if skip_line == Some(t.span.start_line) {
            continue;
        }
        let first_on_line = code.is_empty() || t.span.start_line != prev_line;
        prev_line = t.span.end_line;
        if t.text == "#" && first_on_line {
            skip_line = Some(t.span.start_line);
            continue;
        }
        code.push(i);
    }
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
    };
    s.decls(0, code.len(), Ctx::Top);
    s.out
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    /// File level: namespaces and top-level modules allowed.
    Top,
    /// Namespace or module body.
    Module,
    /// Type body: members too.
    Type,
}

#[derive(Clone, Copy, PartialEq)]
enum Prev {
    None,
    Type,
    Let,
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

    fn is_ident(&self, c: usize, hi: usize) -> bool {
        c < hi && self.tok(c).class == TokenClass::Identifier
    }

    fn line_start(&self, c: usize) -> bool {
        c == 0 || self.tok(c).span.start_line > self.tok(c - 1).span.end_line
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        self.tok(a).span.end == self.tok(b).span.start
    }

    fn close_of(&self, c: usize) -> Option<usize> {
        let close = matching_close(self.tokens, self.code[c])?;
        self.code.binary_search(&close).ok()
    }

    fn block_end(&self, c: usize, hi: usize) -> usize {
        let last = indent_block(self.tokens, self.code[c], SKIP);
        let pos = self.code.partition_point(|&i| i <= last).saturating_sub(1);
        pos.clamp(c, hi - 1)
    }

    /// Dotted name at `c`: (name, last position).
    fn dotted(&self, c: usize, hi: usize) -> Option<(String, usize)> {
        if !self.is_ident(c, hi) {
            return None;
        }
        let mut name = self.text(c).to_string();
        let mut last = c;
        while last + 2 < hi && self.text(last + 1) == "." && self.is_ident(last + 2, hi) {
            name.push('.');
            name.push_str(self.text(last + 2));
            last += 2;
        }
        Some((name, last))
    }

    /// First position at or after `c` (up to `to`) that is not one of `words`.
    fn skip_words(&self, mut c: usize, to: usize, words: &[&str]) -> usize {
        while c <= to && words.contains(&self.text(c)) {
            c += 1;
        }
        c
    }

    /// Position of `=` at bracket depth 0 in `[from, to]`, not part of `==`,
    /// `<=`, `>=`, `<>` or `->`-like operators.
    fn find_eq(&self, from: usize, to: usize) -> Option<usize> {
        let mut depth = 0usize;
        for k in from..=to {
            match self.text(k) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                "=" if depth == 0 => {
                    let joined_before = k > from
                        && self.tok(k - 1).class == TokenClass::Operator
                        && self.adjacent(k - 1, k);
                    let joined_after = k < to
                        && self.tok(k + 1).class == TokenClass::Operator
                        && self.adjacent(k, k + 1);
                    if !joined_before && !joined_after {
                        return Some(k);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The last position before the next line-start `namespace` in
    /// `(c, hi)`, or `hi - 1`.
    fn until_namespace(&self, c: usize, hi: usize) -> usize {
        (c + 1..hi)
            .find(|&k| self.text(k) == "namespace" && self.line_start(k))
            .map_or(hi - 1, |k| k - 1)
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
    fn decls(&mut self, lo: usize, hi: usize, ctx: Ctx) {
        let Some(level) = (lo..hi).find(|&c| self.line_start(c)) else {
            return;
        };
        let col = self.tok(level).span.start_col;
        let mut prev = Prev::None;
        let mut c = level;
        while c < hi {
            if !self.line_start(c) {
                c += 1;
                continue;
            }
            let start = c;
            // Attributes `[< ... >]`, possibly on lines of their own.
            let mut literal = false;
            while c + 1 < hi && self.text(c) == "[" && self.text(c + 1) == "<" {
                match self.close_of(c) {
                    Some(cl) if cl < hi => {
                        literal |= (c..cl).any(|k| self.text(k) == "Literal");
                        c = cl + 1;
                    }
                    _ => break,
                }
            }
            if c >= hi {
                break;
            }
            let at_level = self.tok(start).span.start_col <= col;
            let kw = self.text(c).to_string();
            let next = match kw.as_str() {
                "namespace" if ctx == Ctx::Top => {
                    prev = Prev::None;
                    Some(self.namespace(start, c, hi))
                }
                "module" if at_level && ctx != Ctx::Type => {
                    prev = Prev::None;
                    self.module(start, c, hi, ctx)
                }
                "type" if at_level && ctx != Ctx::Type => {
                    prev = Prev::Type;
                    self.type_decl(start, c, hi)
                }
                "and" if at_level && prev == Prev::Type => self.type_decl(start, c, hi),
                "exception" if at_level && ctx != Ctx::Type => {
                    prev = Prev::None;
                    self.simple(start, c, hi, SymbolKind::Type, "exception")
                }
                "let" if at_level => {
                    prev = Prev::Let;
                    self.let_decl(start, c, hi, ctx == Ctx::Type, literal)
                }
                "and" if at_level && prev == Prev::Let => {
                    self.let_decl(start, c, hi, ctx == Ctx::Type, literal)
                }
                k if ctx == Ctx::Type && MEMBER_WORDS.contains(&k) => self.member(start, c, hi),
                _ => None,
            };
            c = match next {
                Some(end) => end + 1,
                None => c.max(start + 1),
            };
        }
    }

    fn namespace(&mut self, start: usize, c: usize, hi: usize) -> usize {
        let end = self.until_namespace(c, hi);
        let n = self.skip_words(c + 1, end, &["rec", "global"]);
        let (name, name_end) = self
            .dotted(n, end + 1)
            .unwrap_or_else(|| ("global".into(), c));
        self.emit(name, SymbolKind::Module, "namespace", start, end);
        self.decls(name_end + 1, end + 1, Ctx::Module);
        end
    }

    fn module(&mut self, start: usize, c: usize, hi: usize, ctx: Ctx) -> Option<usize> {
        let block = self.block_end(c, hi);
        let mut words = ACCESS.to_vec();
        words.push("rec");
        let n = self.skip_words(c + 1, block, &words);
        let (name, name_end) = self.dotted(n, hi)?;
        let eq = (name_end < block && self.text(name_end + 1) == "=").then_some(name_end + 1);
        match eq {
            Some(eq) => {
                self.emit(name, SymbolKind::Module, "module", start, block);
                self.decls(eq + 1, block + 1, Ctx::Module);
                Some(block)
            }
            None => {
                // Top-level module: the rest of the file or namespace.
                let end = if ctx == Ctx::Top {
                    self.until_namespace(c, hi)
                } else {
                    hi - 1
                };
                self.emit(name, SymbolKind::Module, "module", start, end);
                self.decls(name_end + 1, end + 1, Ctx::Module);
                Some(end)
            }
        }
    }

    fn type_decl(&mut self, start: usize, c: usize, hi: usize) -> Option<usize> {
        let end = self.block_end(c, hi);
        let mut n = c + 1;
        while n < end && self.text(n) == "[" && self.text(n + 1) == "<" {
            n = self.close_of(n)? + 1;
        }
        let mut words = ACCESS.to_vec();
        words.push("rec");
        let n = self.skip_words(n, end, &words);
        if !self.is_ident(n, end + 1) {
            return Some(end);
        }
        let name = self.text(n).to_string();
        // The header: through its line (or an augmentation's `with`).
        let header_to = (n + 1..=end)
            .find(|&k| self.line_start(k) || self.text(k) == "with")
            .map_or(end, |k| k - 1);
        let with = (header_to < end && self.text(header_to + 1) == "with").then_some(header_to + 1);
        let eq = self.find_eq(n + 1, header_to).or(with);
        let primary_ctor = (n + 1..=end.min(n + 8))
            .take_while(|&k| Some(k) != eq)
            .any(|k| self.text(k) == "(");
        let body = eq.map_or(n + 1, |e| e + 1);
        let lk = self.type_kind(body, end, primary_ctor);
        self.emit(name, SymbolKind::Type, lk, start, end);
        if body <= end {
            self.decls(body, end + 1, Ctx::Type);
        }
        Some(end)
    }

    fn type_kind(&self, body: usize, end: usize, primary_ctor: bool) -> &'static str {
        if body > end {
            return if primary_ctor { "class" } else { "type" };
        }
        let first = self.skip_words(body, end, ACCESS);
        if first > end {
            return "type";
        }
        match self.text(first) {
            "{" => return "record",
            "|" => return "union",
            "class" | "interface" | "struct" | "delegate" => {
                return match self.text(first) {
                    "class" => "class",
                    "interface" => "interface",
                    "struct" => "struct",
                    _ => "delegate",
                }
            }
            _ => {}
        }
        let mut depth = 0usize;
        let (mut abstracts, mut others) = (false, false);
        for k in first..=end {
            match self.text(k) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                "|" if depth == 0 && !others => return "union",
                "of" if depth == 0 && k == first + 1 && !others => return "union",
                "abstract" if self.line_start(k) => abstracts = true,
                "member" | "override" | "default" | "new" | "let" | "val" | "static"
                    if self.line_start(k) =>
                {
                    others = true
                }
                _ => {}
            }
        }
        if others || primary_ctor {
            "class"
        } else if abstracts {
            "interface"
        } else {
            "type"
        }
    }

    fn simple(
        &mut self,
        start: usize,
        c: usize,
        hi: usize,
        kind: SymbolKind,
        lk: &str,
    ) -> Option<usize> {
        let end = self.block_end(c, hi);
        if self.is_ident(c + 1, end + 1) {
            let name = self.text(c + 1).to_string();
            self.emit(name, kind, lk, start, end);
        }
        Some(end)
    }

    fn let_decl(
        &mut self,
        start: usize,
        c: usize,
        hi: usize,
        in_type: bool,
        literal: bool,
    ) -> Option<usize> {
        let end = self.block_end(c, hi);
        let mut words = ACCESS.to_vec();
        words.extend(["rec", "inline", "mutable", "static"]);
        let n = self.skip_words(c + 1, end, &words);
        if n > end {
            return Some(end);
        }
        let (name, mut after) = if self.is_ident(n, end + 1) {
            (self.text(n).to_string(), n + 1)
        } else if self.text(n) == "(" {
            // An operator: `let (+.) a b = ...`.
            let Some(cl) = self.close_of(n).filter(|&cl| cl <= end && cl > n + 1) else {
                return Some(end);
            };
            let is_op = |k: usize| {
                matches!(
                    self.tok(k).class,
                    TokenClass::Operator | TokenClass::Punctuation
                ) && !matches!(self.text(k), "(" | ")" | "[" | "]" | "{" | "}" | ",")
            };
            if !(n + 1..cl).all(is_op) {
                return Some(end);
            }
            let op: String = (n + 1..cl).map(|k| self.text(k)).collect();
            (format!("({op})"), cl + 1)
        } else {
            return Some(end);
        };
        // Generic parameters `<'T>` directly after the name.
        if after <= end && self.text(after) == "<" && self.adjacent(after - 1, after) {
            if let Some(gt) = (after..=end).find(|&k| self.text(k) == ">") {
                after = gt + 1;
            }
        }
        if after <= end && self.text(after) == "," {
            // A tuple pattern: not a symbol.
            return Some(end);
        }
        let is_fn = after <= end && !matches!(self.text(after), "=" | ":");
        let (kind, lk) = match (is_fn, in_type) {
            (true, true) => (SymbolKind::Method, "let"),
            (true, false) => (SymbolKind::Function, "let"),
            (false, _) if literal => (SymbolKind::Constant, "let"),
            (false, _) => (SymbolKind::Variable, "let"),
        };
        self.emit(name, kind, lk, start, end);
        Some(end)
    }

    fn member(&mut self, start: usize, c: usize, hi: usize) -> Option<usize> {
        let end = self.block_end(c, hi);
        let is_static = self.text(c) == "static";
        let kw_at = if is_static { c + 1 } else { c };
        if kw_at > end {
            return Some(end);
        }
        if is_static && self.text(kw_at) == "let" {
            return self.let_decl(start, kw_at, hi, true, false);
        }
        let kw = self.text(kw_at).to_string();
        let mut lk = if is_static {
            format!("static {kw}")
        } else {
            kw.clone()
        };
        let mut n = kw_at + 1;
        let mut kind = SymbolKind::Method;
        if kw == "val" {
            kind = SymbolKind::Variable;
        } else if kw == "new" {
            self.emit("new".into(), kind, &lk, start, end);
            return Some(end);
        } else if (kw == "member" || kw == "abstract") && n <= end && self.text(n) == "val" {
            kind = SymbolKind::Variable;
            lk = format!("{lk} val");
            n += 1;
        }
        let mut words = ACCESS.to_vec();
        words.extend(["member", "mutable", "inline", "override", "abstract"]);
        let n = self.skip_words(n, end, &words);
        if !self.is_ident(n, end + 1) {
            return Some(end);
        }
        // `this.Name`: drop the self-identifier.
        let name_at = if n + 2 <= end
            && self.text(n + 1) == "."
            && self.is_ident(n + 2, end + 1)
            && kind == SymbolKind::Method
        {
            n + 2
        } else {
            n
        };
        let name = self.text(name_at).to_string();
        self.emit(name, kind, &lk, start, end);
        Some(end)
    }
}

#[cfg(test)]
mod tests;
