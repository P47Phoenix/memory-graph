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
use graph_core::scan::{
    close_table, code_close_table, mark_keywords, matching_close, span_between, NestedEnds, Step,
};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub struct JavaScriptExtractor;

/// JavaScript's reserved words (ECMAScript, always reserved). Words that are
/// valid identifiers in some code (`let`, `static`, `yield`, `await`,
/// `async`, `of`, `get`, `set`, and the strict-mode-only `implements`,
/// `package`, ...) stay identifiers.
pub const KEYWORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
];

/// What a `{` opens, as far as keyword classing needs to know.
#[derive(Clone, Copy, PartialEq)]
enum Brace {
    /// `switch (...) {`: `default:` there is the keyword.
    Switch,
    /// A class/interface body or an object/type literal: members live here.
    Members,
    /// Anything else (a block).
    Block,
}

/// Classes [`KEYWORDS`] as `keyword` in JavaScript/TypeScript tokens, except
/// where a reserved word is a name:
/// - a property: after `.` / `?.` or a private `#`;
/// - a key or member (`{ default: 1 }`, `{ a; new?: T }`): after `{`, `,`
///   or `;` and before `:` or `?:`, unless the `{` is a switch body;
/// - a method (`delete() {}`, `static get default() {}`, TS
///   `new(): T {}`): in a class body or object literal, after `{ ; } ,`,
///   `static`, `get`, `set`, `async` or `*`, and before `(...)` then `{`
///   (or `:` and a return type, then `{`);
/// - a JSX attribute (`<label for="x" class={c}>`): after an identifier,
///   keyword or string and before a lone `=`.
///
/// Known limits: a brace's kind is guessed from the tokens just before it,
/// so unusual layouts can misclass a member name either way.
pub fn mark_js_keywords(tokens: &mut [TokenDecl]) {
    let closes = close_table(tokens);
    let code: Vec<usize> = (0..tokens.len())
        .filter(|&i| tokens[i].class != TokenClass::Comment)
        .collect();
    let mut pos = vec![usize::MAX; tokens.len()];
    for (k, &i) in code.iter().enumerate() {
        pos[i] = k;
    }
    let mut opens = vec![None; tokens.len()];
    for (o, c) in closes.iter().enumerate() {
        if let Some(c) = *c {
            opens[c] = Some(o);
        }
    }
    fn text<'a>(t: &'a [TokenDecl], code: &[usize], k: usize) -> &'a str {
        code.get(k).map_or("", |&i| t[i].text.as_str())
    }
    // `encl[k]`: kind of the innermost `{` enclosing code token `k`.
    let mut encl = vec![Brace::Block; code.len()];
    let mut stack: Vec<Brace> = Vec::new();
    for (k, e) in encl.iter_mut().enumerate() {
        *e = stack.last().copied().unwrap_or(Brace::Block);
        match text(tokens, &code, k) {
            "{" => {
                let kind = brace_kind(tokens, &code, &pos, &opens, k, *e);
                stack.push(kind);
            }
            "}" => {
                stack.pop();
            }
            _ => {}
        }
    }
    mark_keywords(tokens, KEYWORDS, |t, i| {
        let k = pos[i];
        let prev = if k == 0 { "" } else { text(t, &code, k - 1) };
        let (next, next2) = (text(t, &code, k + 1), text(t, &code, k + 2));
        match prev {
            "." | "?." => return true,
            "#" if t[i - 1].span.end == t[i].span.start => return true,
            _ => {}
        }
        if matches!(prev, "{" | "," | ";")
            && (next == ":" || next == "?" && next2 == ":")
            && encl[k] != Brace::Switch
        {
            return true;
        }
        if encl[k] == Brace::Members
            && matches!(
                prev,
                "{" | ";" | "}" | "," | "static" | "get" | "set" | "async" | "*"
            )
            && next == "("
            && method_body_follows(t, &code, &pos, &closes, k + 1)
        {
            return true;
        }
        let attr_prev = k > 0 && {
            let p = &t[code[k - 1]];
            matches!(p.class, TokenClass::Identifier | TokenClass::Keyword)
                || p.class == TokenClass::Literal && p.text.starts_with(['"', '\''])
        };
        attr_prev && next == "=" && next2 != "=" && next2 != ">"
    });
}

/// Kind of the `{` at code position `k`, whose own enclosing brace is `outer`.
fn brace_kind(
    t: &[TokenDecl],
    code: &[usize],
    pos: &[usize],
    opens: &[Option<usize>],
    k: usize,
    outer: Brace,
) -> Brace {
    let Some(p) = k.checked_sub(1) else {
        return Brace::Block;
    };
    let prev = t[code[p]].text.as_str();
    if prev == ")" {
        // `switch (...) {`: find the `(` matching this `)`.
        if let Some(o) = opens[code[p]] {
            let before = pos[o].checked_sub(1).map(|b| t[code[b]].text.as_str());
            if before == Some("switch") {
                return Brace::Switch;
            }
        }
        return Brace::Block;
    }
    if matches!(
        prev,
        "=" | "(" | "," | "[" | "?" | "return" | "||" | "&&" | "??"
    ) || prev == ":" && outer != Brace::Switch
    {
        return Brace::Members;
    }
    // `class A extends B<C> implements D {`, `interface I<T> {`: a short
    // walk back to the declaring word.
    for b in (p.saturating_sub(32)..=p).rev() {
        match t[code[b]].text.as_str() {
            "class" | "interface" => return Brace::Members,
            ";" | "{" | "}" | "(" | ")" | "=" | "=>" => break,
            _ => {}
        }
    }
    Brace::Block
}

/// Whether the `(` at code position `k` closes and is followed by a method
/// body `{`, directly or after a `:` return type.
fn method_body_follows(
    t: &[TokenDecl],
    code: &[usize],
    pos: &[usize],
    closes: &[Option<usize>],
    k: usize,
) -> bool {
    let Some(close) = code.get(k).and_then(|&i| closes[i]) else {
        return false;
    };
    let after = pos[close] + 1;
    match code.get(after).map(|&i| t[i].text.as_str()) {
        Some("{") => true,
        Some(":") => {
            code[after + 1..]
                .iter()
                .take(64)
                .map(|&i| t[i].text.as_str())
                .find(|s| matches!(*s, "{" | ";" | "}" | "=>"))
                == Some("{")
        }
        _ => false,
    }
}

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
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("javascript-scan-1+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, JS_TOKENIZER);
        let symbols = symbols(&tokens);
        // After the symbol scan, which reads identifiers as it always has.
        mark_js_keywords(&mut tokens);
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
        angles: NestedEnds::new(),
        body_braces: RefCell::default(),
    }
    .type_end(c, hi)
}

/// [`type_end`] for many lookups over the same tokens: closers come from a
/// [`code_close_table`] and generic-list ends are remembered between calls,
/// so calling [`TypeEnds::type_end`] once per declaration stays linear on
/// long unbalanced input (a fresh [`type_end`] call scans forward each
/// time: `type a = (` x 40k took over a minute, #142).
pub struct TypeEnds<'a> {
    scanner: Scanner<'a>,
}

impl<'a> TypeEnds<'a> {
    /// `closes` must be [`code_close_table`]`(tokens, code)`.
    pub fn new(tokens: &'a [TokenDecl], code: &'a [usize], closes: &'a [Option<usize>]) -> Self {
        debug_assert_eq!(closes.len(), code.len());
        Self {
            scanner: Scanner {
                tokens,
                code,
                ts: true,
                depth: Cell::new(0),
                closes: Some(closes),
                angles: NestedEnds::new(),
                body_braces: RefCell::default(),
            },
        }
    }

    /// See [`type_end`].
    pub fn type_end(&self, c: usize, hi: usize) -> usize {
        self.scanner.type_end(c, hi)
    }
}

fn scan_tokens(tokens: &[TokenDecl], ts: bool) -> Vec<SymbolDecl> {
    let code: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.class != TokenClass::Comment)
        .map(|(i, _)| i)
        .collect();
    let closes = code_close_table(tokens, &code);
    let s = Scanner {
        tokens,
        code: &code,
        ts,
        depth: Cell::new(0),
        closes: Some(&closes),
        angles: NestedEnds::new(),
        body_braces: RefCell::default(),
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

/// Reserved words that start a statement and are never part of a type.
const STATEMENT_WORDS: &[&str] = &["export", "import", "default", "const", "let", "var"];

/// TypeScript's contextual words that start a declaration when a name (or a
/// module string) follows them: `declare module "m"`, `namespace N`.
const DECLARATION_WORDS: &[&str] = &[
    "declare",
    "abstract",
    "namespace",
    "module",
    "type",
    "async",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    /// TypeScript mode.
    ts: bool,
    /// Nesting of `scan` calls, capped at [`MAX_DEPTH`].
    depth: Cell<usize>,
    /// [`code_close_table`], when built (else each lookup scans forward).
    closes: Option<&'a [Option<usize>]>,
    /// `(open, hi)` -> [`Scanner::angle_close`], filled as scans go.
    angles: NestedEnds,
    /// `(c, hi)` -> [`Scanner::body_brace`] from `c`.
    body_braces: RefCell<HashMap<(usize, usize), Option<usize>>>,
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
        if let Some(closes) = self.closes {
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

    /// The first `{` at or after `c`, skipping `(`/`[` groups; `None` at
    /// `;`, `}`, an unmatched group or `hi`. Every position the walk passes
    /// has the same answer, so they are all remembered: a long run of
    /// `class A ` stays linear instead of one walk to the end per `class`.
    fn body_brace(&self, mut c: usize, hi: usize) -> Option<usize> {
        let mut visited = Vec::new();
        let r = loop {
            if c >= hi {
                break None;
            }
            if let Some(&r) = self
                .body_braces
                .borrow()
                .get(&(c, hi))
                .filter(|_| memo_on())
            {
                break r;
            }
            visited.push(c);
            match self.text(c) {
                "{" => break Some(c),
                ";" | "}" => break None,
                "(" | "[" => match self.close_of(c) {
                    Some(p) => c = p + 1,
                    None => break None,
                },
                _ => c += 1,
            }
        };
        let mut memo = self.body_braces.borrow_mut();
        for v in visited {
            memo.insert((v, hi), r);
        }
        r
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
        let open = self.body_brace(open, hi)?;
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
        if body >= hi
            || matches!(self.text(body), ";" | "," | ")" | "]" | "}")
            || self.ends_expression(body, hi)
        {
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
    ///
    /// Memoized ([`NestedEnds`]), so looking up every `<` of a long
    /// unclosed run (`type a = <` x 100k) stays linear.
    fn angle_close(&self, open: usize, hi: usize) -> Option<usize> {
        self.angles.find(open, hi, |c| match self.text(c) {
            "<" => Step::Open,
            ">" if !(c > 0 && self.is_arrow(c - 1, hi)) => Step::Close,
            "(" | "[" | "{" => Step::Skip(self.close_of(c).filter(|&p| p < hi)),
            ";" | ")" | "]" | "}" => Step::Stop,
            _ => Step::Other,
        })
    }

    /// Whether the code token after `c` (before `hi`) is a word or a literal.
    fn word_follows(&self, c: usize, hi: usize) -> bool {
        c + 1 < hi
            && matches!(
                self.tok(c + 1).class,
                TokenClass::Identifier | TokenClass::Keyword | TokenClass::Literal
            )
    }

    /// Whether the word at `c` opens a statement rather than continuing a
    /// type: a statement word (not `import(...)`, an import type), or a
    /// contextual declaration word followed by a name. Never after `.`
    /// (`React.default`, `x.const` are property names).
    fn starts_statement(&self, c: usize, hi: usize) -> bool {
        let t = self.text(c);
        if c > 0 && matches!(self.text(c - 1), "." | "?.") {
            return false;
        }
        if STATEMENT_WORDS.contains(&t) {
            return !(t == "import" && c + 1 < hi && self.text(c + 1) == "(");
        }
        // `abstract new () => X` is a constructor type, not a declaration.
        if t == "abstract" && c + 1 < hi && self.text(c + 1) == "new" {
            return false;
        }
        DECLARATION_WORDS.contains(&t) && self.word_follows(c, hi)
    }

    /// Whether an expression (an arrow's expression body, a field
    /// initializer) cannot continue at `c` in TypeScript mode: a decorator,
    /// or a word that opens the next statement (`async` aside, which can
    /// start an async arrow). Stopping there keeps the expression's span out
    /// of the declaration the TypeScript pass starts at that word (#213).
    fn ends_expression(&self, c: usize, hi: usize) -> bool {
        self.ts
            && (self.text(c) == "@" || (self.text(c) != "async" && self.starts_statement(c, hi)))
    }

    /// See [`type_end`]. Only TypeScript mode calls it (every caller checks
    /// `self.ts`), so changes here leave the JavaScript extractor's output
    /// and version unchanged.
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
                // A generic argument list (`A<T>`) completes an operand; a
                // generic parameter list where an operand is expected
                // (`<const T>(x: T) => T`) leaves one still to come.
                "<" => match self.angle_close(c, hi) {
                    Some(p) => c = p + 1,
                    None => return c,
                },
                ";" | "," | ")" | "]" | "}" | "=" | ">" => return c,
                // A decorator opens the next declaration (#213).
                "@" => return c,
                // Reserved words that start a declaration, never a type: a
                // type stops before them, so it cannot end inside the
                // declaration (`type A = class B {}` must not give a type
                // span that partially overlaps the class, #145).
                "class" | "function" | "enum" | "interface" => return c,
                // Likewise a word that opens the next statement, which the
                // TypeScript pass folds into the next declaration's span
                // (`type A = B |\nexport class C {}`, #203).
                _ if self.starts_statement(c, hi) => return c,
                // An import type, `import("./m")`: the call's group is the
                // operand.
                "import" if operand && c + 1 < hi && self.text(c + 1) == "(" => {
                    c += 1;
                    continue;
                }
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
                // `type A = ...` / `type A<T> = ...` (not valid in a class,
                // but the TypeScript pass reads it as a type alias at any
                // depth): not a field `A`, whose initializer could end
                // somewhere other than the alias's type (#213).
                "type"
                    if c + 2 < hi
                        && self.is_ident(c + 1)
                        && matches!(self.text(c + 2), "=" | "<") =>
                {
                    c += 2;
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
                _ if self.ends_expression(c, hi) => return last,
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
                _ if self.ends_expression(c, hi) => return Some(last),
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

/// Whether the scan memos are consulted. Tests switch them off to compare
/// against the plain forward scans; always on outside tests.
fn memo_on() -> bool {
    #[cfg(test)]
    return tests::MEMO.with(std::cell::Cell::get);
    #[cfg(not(test))]
    true
}

#[cfg(test)]
mod tests;
