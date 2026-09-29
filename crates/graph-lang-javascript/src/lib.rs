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
//! Private members keep their `#` (`#p`). Declarations found inside an arrow
//! body always lie within the arrow's span.
//!
//! Regex literals are lexed by the tokenizer's `regex_literals` heuristic (a
//! `/` where an operand is expected); a regex it misjudges, or a template
//! literal nesting backticks, can still cut a scan short: the extractor then
//! returns fewer symbols, never invalid spans or `has_errors`. Not symbols:
//! computed members (`[Symbol.iterator]() {}`), class-field arrows
//! (`x = () => {}`), object-literal methods, anonymous
//! `module.exports = function () {}`.
use graph_core::scan::{code_close_table, matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::Cell;

pub struct JavaScriptExtractor;

/// Tokenizer dialect used for JavaScript.
pub const JS_TOKENIZER: TokenizerOptions = TokenizerOptions {
    single_quote_strings: true,
    regex_literals: true,
    ..TokenizerOptions::DEFAULT
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
    scan_tokens(tokens, false)
}

/// The same scan, tolerant of TypeScript syntax: type annotations, generic
/// parameters and return types are skipped; `abstract`/`declare` prefixes,
/// accessibility and `readonly` modifiers, decorators, bodiless signatures
/// (`abstract m(): void;`, `declare function f(): T;`) and typed class fields
/// are understood. Class fields are Variable (`field`), abstract classes have
/// `lang_kind` `abstract_class`. TypeScript-only declarations (interfaces,
/// type aliases, enums, namespaces) are the TypeScript extractor's job.
pub fn typescript_symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    scan_tokens(tokens, true)
}

/// Code position just past a TypeScript type starting at code position `c`
/// (`code` indexes `tokens`, e.g. the non-comment tokens), stopping at `hi`:
/// the first `;`, `,`, `=` (not part of a function type's `=>`), closer, or
/// token that cannot continue the type (a word after a complete type, or a
/// `{` after one). Returns `c` if no type starts there.
pub fn type_end(tokens: &[TokenDecl], code: &[usize], c: usize, hi: usize) -> usize {
    Scanner {
        tokens,
        code,
        ts: true,
        depth: Cell::new(0),
        closes: None,
    }
    .type_end(c, hi)
}

fn scan_tokens(tokens: &[TokenDecl], ts: bool) -> Vec<SymbolDecl> {
    let code: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.class != TokenClass::Comment)
        .map(|(i, _)| i)
        .collect();
    let s = Scanner {
        tokens,
        code: &code,
        ts,
        depth: Cell::new(0),
        closes: Some(code_close_table(tokens, &code)),
    };
    let mut out = Vec::new();
    s.scan(0, code.len(), &mut out);
    out
}

const PREFIXES: &[&str] = &["export", "default", "async"];
const TS_PREFIXES: &[&str] = &["abstract", "declare"];
/// Class member modifiers in TypeScript mode (JavaScript's plus TypeScript's).
const TS_MODIFIERS: &[&str] = &[
    "static",
    "async",
    "get",
    "set",
    "public",
    "private",
    "protected",
    "readonly",
    "abstract",
    "override",
    "declare",
    "accessor",
];
/// Words that continue a type after a complete operand (`A extends B`).
const TYPE_WORDS: &[&str] = &[
    "keyof", "typeof", "infer", "is", "extends", "readonly", "unique", "asserts", "new", "in", "as",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    /// TypeScript mode.
    ts: bool,
    /// Nesting of `scan` calls, capped at [`MAX_DEPTH`].
    depth: Cell<usize>,
    /// [`code_close_table`], when built (else each lookup scans forward).
    closes: Option<Vec<Option<usize>>>,
}

/// Deepest nesting of declarations scanned (classes in functions in
/// classes ...). Deeper declarations are not reported, their enclosing ones
/// are: this bounds the recursion so adversarial input cannot overflow the
/// indexer thread's stack.
pub const MAX_DEPTH: usize = 256;

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
        if let Some(closes) = &self.closes {
            return closes[c];
        }
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
        while c > lo
            && (PREFIXES.contains(&self.text(c - 1))
                || (self.ts && TS_PREFIXES.contains(&self.text(c - 1))))
        {
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
        // Private members keep their `#`: `#p` and `p` are different names.
        let private = name > 0
            && self.text(name - 1) == "#"
            && self.tok(name - 1).span.end == self.tok(name).span.start;
        let prefix = if private { "#" } else { "" };
        out.push(SymbolDecl {
            owner: None,
            name: format!("{prefix}{}", self.text(name)),
            kind,
            lang_kind: Some(lang.into()),
            span: span_between(&self.tok(span.0).span, &self.tok(span.1).span),
        });
    }

    /// Scan code positions `[lo, hi)` for declarations, at any depth.
    fn scan(&self, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let depth = self.depth.get();
        if depth >= MAX_DEPTH {
            return;
        }
        self.depth.set(depth + 1);
        self.scan_level(lo, hi, out);
        self.depth.set(depth);
    }

    fn scan_level(&self, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
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
        if c + 1 >= hi || !self.is_ident(c) {
            return None;
        }
        let name = c;
        let mut paren = c + 1;
        if self.ts && self.text(paren) == "<" {
            paren = self.angle_close(paren, hi)? + 1;
        }
        if paren >= hi || self.text(paren) != "(" {
            return None;
        }
        let params_close = self.close_of(paren).filter(|&p| p < hi)?;
        let mut open = params_close + 1;
        if self.ts && open < hi && self.text(open) == ":" {
            open = self.type_end(open + 1, hi);
        }
        let start = self.start_of(kw, lo);
        if self.ts && open < hi && self.text(open) == ";" {
            // `declare function f(): T;` or an overload signature.
            self.push(out, name, SymbolKind::Function, "function", (start, open));
            return Some(open + 1);
        }
        if open >= hi || self.text(open) != "{" {
            return None;
        }
        let close = self.close_of(open).filter(|&p| p < hi)?;
        self.push(out, name, SymbolKind::Function, "function", (start, close));
        Some(open + 1)
    }

    /// `class Name [extends X] {...}` and its methods.
    fn class(&self, kw: usize, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) -> Option<usize> {
        let name = kw + 1;
        if name >= hi || !self.is_ident(name) {
            return None;
        }
        // The body is the first `{` after the heritage clause, skipping whole
        // groups (`extends mix({ a() {} })`).
        let mut open = name + 1;
        if self.ts && open < hi && self.text(open) == "<" {
            open = self.angle_close(open, hi)? + 1;
        }
        loop {
            if open >= hi {
                return None;
            }
            match self.text(open) {
                "{" => break,
                ";" | "}" => return None,
                "(" | "[" => open = self.close_of(open)? + 1,
                _ => open += 1,
            }
        }
        let close = self.close_of(open).filter(|&p| p < hi)?;
        let start = self.start_of(kw, lo);
        let lang = if self.ts && (start..kw).any(|c| self.text(c) == "abstract") {
            "abstract_class"
        } else {
            "class"
        };
        self.push(out, name, SymbolKind::Type, lang, (start, close));
        if self.ts {
            self.ts_class_body(open + 1, close, out);
        } else {
            self.class_body(open + 1, close, out);
        }
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
        if name + 2 >= hi || !self.is_ident(name) {
            return None;
        }
        let mut eq = name + 1;
        if self.ts && self.text(eq) == "!" {
            eq += 1;
        }
        if self.ts && eq < hi && self.text(eq) == ":" {
            eq = self.type_end(eq + 1, hi);
        }
        if eq + 1 >= hi || self.text(eq) != "=" || self.is_arrow(eq, hi) {
            return None;
        }
        let mut v = eq + 1;
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
            if self.ts && p < hi && self.text(p) == "<" {
                p = self.angle_close(p, hi)? + 1;
            }
            if p >= hi || self.text(p) != "(" {
                return None;
            }
            let params = self.close_of(p).filter(|&x| x + 1 < hi)?;
            let mut open = params + 1;
            if self.ts && self.text(open) == ":" {
                open = self.type_end(open + 1, hi);
            }
            if open >= hi || self.text(open) != "{" {
                return None;
            }
            let close = self.close_of(open).filter(|&x| x < hi)?;
            let end = self.semi_after(close, hi);
            self.push(out, name, SymbolKind::Function, "fn_expr", (start, end));
            // Continue at `function` so a named function expression is found too.
            return Some(v);
        }
        // Arrow: `(...) =>` or `x =>`.
        if self.ts && self.text(v) == "<" {
            v = self.angle_close(v, hi)? + 1;
            if v >= hi {
                return None;
            }
        }
        let mut arrow = if self.text(v) == "(" {
            self.close_of(v).map(|p| p + 1)?
        } else if self.is_ident(v) {
            v + 1
        } else {
            return None;
        };
        if self.ts && arrow < hi && self.text(arrow) == ":" && self.text(v) == "(" {
            arrow = self.type_end(arrow + 1, hi);
        }
        if !self.is_arrow(arrow, hi) {
            return None;
        }
        let body = arrow + 2;
        if body >= hi || matches!(self.text(body), ";" | "," | ")" | "]" | "}") {
            return None;
        }
        let end = if self.text(body) == "{" {
            let close = self.close_of(body).filter(|&x| x < hi)?;
            self.semi_after(close, hi)
        } else {
            self.expression_end(body, hi)?
        };
        self.push(out, name, SymbolKind::Function, "arrow_fn", (start, end));
        // Declarations inside the body must stay inside the arrow's span.
        self.scan(body, end + 1, out);
        Some(end + 1)
    }

    /// Code position of the `>` closing the generic list opened by `<` at
    /// `c` (groups skipped, the `>` of `=>` ignored); `None` at a `;`, a
    /// stray closer or the end of the range.
    fn angle_close(&self, mut c: usize, hi: usize) -> Option<usize> {
        let mut depth = 0usize;
        while c < hi {
            match self.text(c) {
                "<" => depth += 1,
                ">" if !(c > 0 && self.is_arrow(c - 1, hi)) => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(c);
                    }
                }
                "(" | "[" | "{" => {
                    c = self.close_of(c).filter(|&p| p < hi)?;
                }
                ";" | ")" | "]" | "}" => return None,
                _ => {}
            }
            c += 1;
        }
        None
    }

    /// See [`type_end`].
    fn type_end(&self, mut c: usize, hi: usize) -> usize {
        // Whether an operand is expected next, and whether the last operand
        // was a parenthesized group (so `=>` makes it a function type).
        let mut operand = true;
        let mut after_paren = false;
        while c < hi {
            if self.is_arrow(c, hi) {
                if !after_paren {
                    return c;
                }
                c += 2;
                operand = true;
                after_paren = false;
                continue;
            }
            let t = self.text(c);
            let new_line = c > 0 && self.tok(c).span.start_line > self.tok(c - 1).span.end_line;
            match t {
                "{" | "(" | "[" => {
                    if !operand && t != "[" {
                        return c;
                    }
                    match self.close_of(c).filter(|&p| p < hi) {
                        Some(p) => c = p + 1,
                        None => return c,
                    }
                    operand = false;
                    after_paren = t == "(";
                    continue;
                }
                "<" => match self.angle_close(c, hi) {
                    Some(p) => {
                        c = p + 1;
                        operand = false;
                    }
                    None => return c,
                },
                ";" | "," | ")" | "]" | "}" | "=" | ">" => return c,
                "|" | "&" | "?" | ":" | "." | "-" | "+" => {
                    c += 1;
                    operand = true;
                }
                _ if TYPE_WORDS.contains(&t) && !(new_line && !operand) => {
                    c += 1;
                    operand = true;
                }
                _ => {
                    if !operand {
                        return c;
                    }
                    c += 1;
                    operand = false;
                }
            }
            after_paren = false;
        }
        c
    }

    /// Class body in TypeScript mode: methods (with or without a body) and
    /// fields, skipping decorators.
    fn ts_class_body(&self, lo: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let mut c = lo;
        while c < hi {
            match self.text(c) {
                // Decorator: `@a.b(...)`.
                "@" => {
                    c += 1;
                    while c < hi && (self.is_ident(c) || self.text(c) == ".") {
                        c += 1;
                    }
                    if c < hi && self.text(c) == "(" {
                        c = self.close_of(c).map_or(hi, |p| p + 1);
                    }
                    continue;
                }
                ";" | "," => {
                    c += 1;
                    continue;
                }
                _ => {}
            }
            if let Some(next) = self.ts_member(c, hi, out) {
                c = next;
                continue;
            }
            c = match self.text(c) {
                "(" | "[" | "{" => self.close_of(c).map_or(hi, |p| p + 1),
                _ => c + 1,
            };
        }
    }

    /// One class member at `start`; returns where to continue.
    fn ts_member(&self, start: usize, hi: usize, out: &mut Vec<SymbolDecl>) -> Option<usize> {
        let mut lang = "method";
        let mut n = start;
        while n + 1 < hi {
            let w = self.text(n);
            if w == "*" || w == "#" {
                n += 1;
                continue;
            }
            // A modifier is followed by more of the member, not by what
            // follows a member name (`readonly: boolean` is a field).
            let ends_name = matches!(
                self.text(n + 1),
                "(" | ":" | "=" | ";" | "?" | "!" | "<" | "}" | ","
            ) || self.tok(n + 1).span.start_line > self.tok(n).span.end_line;
            if TS_MODIFIERS.contains(&w) && !ends_name {
                if matches!(w, "get" | "set") {
                    lang = w;
                }
                n += 1;
                continue;
            }
            break;
        }
        if n >= hi || !self.is_ident(n) {
            return None;
        }
        let mut p = n + 1;
        if p < hi && matches!(self.text(p), "?" | "!") {
            p += 1;
        }
        let new_line = p >= hi || self.tok(p).span.start_line > self.tok(p - 1).span.end_line;
        let generic = !new_line && self.text(p) == "<";
        if generic {
            p = self.angle_close(p, hi)? + 1;
        }
        if p < hi && self.text(p) == "(" {
            let params = self.close_of(p).filter(|&x| x < hi)?;
            let mut q = params + 1;
            if q < hi && self.text(q) == ":" {
                q = self.type_end(q + 1, hi);
            }
            if self.text(n) == "constructor" {
                lang = "constructor";
            }
            if q < hi && self.text(q) == "{" {
                let close = self.close_of(q).filter(|&x| x < hi)?;
                self.push(out, n, SymbolKind::Method, lang, (start, close));
                self.scan(q + 1, close, out);
                return Some(close + 1);
            }
            // A signature: abstract, overload or `declare`d.
            let end = if q < hi && self.text(q) == ";" {
                q
            } else {
                q - 1
            };
            self.push(out, n, SymbolKind::Method, lang, (start, end));
            return Some(end + 1);
        }
        if generic || !(new_line || matches!(self.text(p), ":" | "=" | ";" | "}")) {
            return None;
        }
        // A field.
        let mut q = p;
        if q < hi && self.text(q) == ":" {
            q = self.type_end(q + 1, hi);
        }
        let end = if q < hi && self.text(q) == "=" && !self.is_arrow(q, hi) {
            self.field_end(q, hi)
        } else if q < hi && self.text(q) == ";" {
            q
        } else {
            q - 1
        };
        self.push(out, n, SymbolKind::Variable, "field", (start, end));
        Some(end + 1)
    }

    /// Last token of a field initializer whose `=` is at `eq`: through a `;`
    /// at this level, or up to the end of the line it ends on (a closer of
    /// the class body ends it too).
    fn field_end(&self, eq: usize, hi: usize) -> usize {
        let mut last = eq;
        let mut c = eq + 1;
        while c < hi {
            match self.text(c) {
                ";" => return c,
                ")" | "]" | "}" => return last,
                "(" | "[" | "{" => {
                    let Some(close) = self.close_of(c).filter(|&x| x < hi) else {
                        return last;
                    };
                    last = close;
                    c = close + 1;
                    continue;
                }
                _ => {}
            }
            if c > eq + 1 && self.tok(c).span.start_line > self.tok(last).span.end_line {
                let t = self.tok(c);
                let continues = t.class == TokenClass::Operator
                    || t.text.starts_with(['.', '?', ':'])
                    || self.tok(last).class == TokenClass::Operator;
                if !continues {
                    return last;
                }
            }
            last = c;
            c += 1;
        }
        last
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
            // A line break ends the expression unless the next line continues
            // it (`.then(...)`, `? a : b`, `+ x`) or this line ends in an
            // operator.
            if c > last && self.tok(c).span.start_line > self.tok(last).span.end_line {
                let t = self.tok(c);
                let continues = t.class == TokenClass::Operator
                    || t.text.starts_with(['.', '?', ':'])
                    || self.tok(last).class == TokenClass::Operator;
                if !continues {
                    return Some(last);
                }
            }
            last = c;
            c += 1;
        }
        Some(last)
    }
}

#[cfg(test)]
mod tests;
