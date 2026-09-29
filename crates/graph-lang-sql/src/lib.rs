//! SQL extractor: a token-stream scanner over `CREATE` statements, not a
//! parser. Keywords are case-insensitive.
//!
//! | SQL | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `CREATE TABLE` / `VIEW` / `TYPE` | Type | `table` `view` `type` |
//! | `CREATE TYPE BODY` (PL/SQL) | Type | `type body` |
//! | `CREATE PROCEDURE` / `PROC` / `FUNCTION` | Function | `procedure` `function` |
//! | `CREATE TRIGGER` / `INDEX` / `SEQUENCE` | Other | `trigger` `index` `sequence` |
//! | `CREATE SCHEMA` | Module | `schema` |
//! | `CREATE PACKAGE [BODY]` (PL/SQL) | Module | `package` `package body` |
//! | `PROCEDURE` / `FUNCTION` inside a package (type body) | Function (Method) | `procedure` `function` |
//!
//! `OR REPLACE`, `OR ALTER`, `IF NOT EXISTS` and modifiers between `CREATE`
//! and the object keyword (`UNIQUE`, `TEMPORARY`, MySQL `DEFINER=...`, ...)
//! are skipped. Names may be qualified and quoted (`[dbo].[t]`, `"s"."t"`,
//! `` `t` ``); the symbol name is the qualified name with the quotes removed.
//!
//! A statement ends at its `;`, a batch separator (a T-SQL `GO` line, a
//! PL/SQL `/` line, the delimiter set by a MySQL `DELIMITER` line; the
//! separator is not part of the span), or the next `CREATE`. Routine bodies
//! are skipped as a unit: PostgreSQL dollar quotes (`$$ ... $$`,
//! `$tag$ ... $tag$`), `BEGIN`/`CASE` ... `END` blocks, and PL/SQL
//! declaration sections (`IS x int; BEGIN ... END name;`). A T-SQL routine
//! without `BEGIN` runs to its `GO`.
//!
//! Known limits: a `CREATE` inside a T-SQL routine body that has no
//! `BEGIN ... END` (only statements up to `GO`) ends the routine early and is
//! reported as its own symbol; a dollar-quoted body is tokenized as SQL code,
//! so an unbalanced `'` inside it can swallow what follows (a tokenizer
//! limit); `ALTER` statements and T-SQL `#temp` names are not symbols.
use graph_core::scan::{code_close_table, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct SqlExtractor;

/// Tokenizer dialect used for SQL.
pub const SQL_TOKENIZER: TokenizerOptions = TokenizerOptions::SQL;

impl Extractor for SqlExtractor {
    fn language(&self) -> &str {
        "sql"
    }

    fn extensions(&self) -> &[&str] {
        &["sql"]
    }

    fn version(&self) -> String {
        format!("sql-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, SQL_TOKENIZER);
        let symbols = symbols(source, &tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in SQL tokens (as produced with [`SQL_TOKENIZER`] from `source`).
pub fn symbols(source: &str, tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.class != TokenClass::Comment)
        .map(|(i, _)| i)
        .collect();
    let boundary = boundaries(source, tokens, &code);
    let mut s = Scanner {
        tokens,
        code: &code,
        closes: code_close_table(tokens, &code),
        boundary: &boundary,
        out: Vec::new(),
    };
    let mut c = 0;
    while c < code.len() {
        if !s.boundary[c] && s.is(c, "create") {
            c = s.create(c);
        } else {
            c += 1;
        }
    }
    s.out
}

/// Marks code positions that separate statements: whole `GO` and `/` lines,
/// `DELIMITER` lines, and occurrences of a custom delimiter.
fn boundaries(source: &str, tokens: &[TokenDecl], code: &[usize]) -> Vec<bool> {
    let mut out = vec![false; code.len()];
    let mut delim: Option<String> = None;
    let mut i = 0;
    while i < code.len() {
        let line = tokens[code[i]].span.start_line;
        let mut j = i;
        while j < code.len() && tokens[code[j]].span.start_line == line {
            j += 1;
        }
        let words: Vec<&TokenDecl> = code[i..j].iter().map(|&k| &tokens[k]).collect();
        let first = words[0];
        let whole_line = if first.text.eq_ignore_ascii_case("delimiter")
            && first.class == TokenClass::Identifier
            && words.len() > 1
        {
            let last = words[words.len() - 1];
            let d = source[words[1].span.start as usize..last.span.end as usize].trim();
            delim = (d != ";").then(|| d.to_string());
            true
        } else {
            (first.text.eq_ignore_ascii_case("go")
                && first.class == TokenClass::Identifier
                && (words.len() == 1
                    || (words.len() == 2 && words[1].class == TokenClass::Literal)))
                || (words.len() == 1 && first.text == "/")
        };
        if whole_line {
            out[i..j].iter_mut().for_each(|b| *b = true);
        } else if let Some(d) = &delim {
            let mut k = i;
            while k < j {
                let t = &tokens[code[k]];
                if source[t.span.start as usize..].starts_with(d.as_str()) {
                    let end = t.span.start as usize + d.len();
                    while k < j && (tokens[code[k]].span.start as usize) < end {
                        out[k] = true;
                        k += 1;
                    }
                } else {
                    k += 1;
                }
            }
        }
        i = j;
    }
    out
}

/// Words allowed between `CREATE` and the object keyword; anything else
/// (`CREATE EXTENSION ... WITH SCHEMA`, `CREATE PUBLICATION ... FOR TABLE`)
/// means the statement makes something that is not a symbol.
const CREATE_MODIFIERS: &[&str] = &[
    "or",
    "replace",
    "alter",
    "unique",
    "clustered",
    "nonclustered",
    "columnstore",
    "fulltext",
    "spatial",
    "bitmap",
    "temp",
    "temporary",
    "global",
    "local",
    "unlogged",
    "materialized",
    "recursive",
    "editionable",
    "noneditionable",
    "editioning",
    "force",
    "noforce",
    "constraint",
    "definer",
    "algorithm",
    "sql",
    "security",
    "invoker",
    "aggregate",
    "external",
    "virtual",
    "secure",
    "transient",
    "volatile",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    /// [`code_close_table`] of `code`: closers found in one linear pass.
    closes: Vec<Option<usize>>,
    boundary: &'a [bool],
    out: Vec<SymbolDecl>,
}

/// What a `CREATE` makes and how its end is found.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Ends at `;` (outside parentheses).
    Plain,
    /// A routine: a body of blocks, dollar quotes and declarations.
    Routine,
    /// A PL/SQL package or type body: runs to its own `END [name];`.
    Container,
}

/// How the scan for a routine's end should treat block keywords.
enum Block {
    Begin,
    Case,
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    /// The code token at `c` is the (unquoted) word `w`, any case.
    fn is(&self, c: usize, w: &str) -> bool {
        c < self.code.len()
            && self.tok(c).class == TokenClass::Identifier
            && self.text(c).eq_ignore_ascii_case(w)
    }

    fn is_any(&self, c: usize, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.is(c, w))
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        b < self.code.len() && self.tok(a).span.end == self.tok(b).span.start
    }

    fn close_of(&self, c: usize) -> Option<usize> {
        self.closes[c]
    }

    /// A dollar-quote opener (`$$` or `$tag$`) at `c`: returns the code
    /// position of its last token.
    fn dollar_open(&self, c: usize) -> Option<usize> {
        if self.text(c) != "$" || self.boundary[c] {
            return None;
        }
        if self.adjacent(c, c + 1) && self.text(c + 1) == "$" {
            return Some(c + 1);
        }
        if self.adjacent(c, c + 1)
            && self.tok(c + 1).class == TokenClass::Identifier
            && self.adjacent(c + 1, c + 2)
            && self.text(c + 2) == "$"
        {
            return Some(c + 2);
        }
        None
    }

    /// From a dollar quote opening at `c`, the code position after its
    /// matching closer (or the end of input).
    fn skip_dollar(&self, c: usize, open_end: usize) -> usize {
        let tag: Vec<&str> = (c..=open_end).map(|k| self.text(k)).collect();
        let mut k = open_end + 1;
        while k < self.code.len() {
            if let Some(e) = self.dollar_open(k) {
                if (k..=e).map(|x| self.text(x)).eq(tag.iter().copied()) {
                    return e + 1;
                }
            }
            k += 1;
        }
        self.code.len()
    }

    /// `BEGIN` that opens a block (not `BEGIN TRAN`, `BEGIN;`).
    fn is_begin(&self, c: usize) -> bool {
        self.is(c, "begin")
            && !self.is_any(c + 1, &["tran", "transaction", "distributed", "work"])
            && !(c + 1 < self.code.len() && self.text(c + 1) == ";")
    }

    /// Handle `CREATE` at `c`; returns the code position to resume at.
    fn create(&mut self, c: usize) -> usize {
        let n = self.code.len();
        // Find the object keyword.
        let mut k = c + 1;
        let found = loop {
            if k >= n || k > c + 16 || self.boundary[k] || matches!(self.text(k), "(" | ";") {
                break None;
            }
            let hit = [
                ("table", SymbolKind::Type, "table", Shape::Plain),
                ("view", SymbolKind::Type, "view", Shape::Plain),
                ("type", SymbolKind::Type, "type", Shape::Plain),
                (
                    "procedure",
                    SymbolKind::Function,
                    "procedure",
                    Shape::Routine,
                ),
                ("proc", SymbolKind::Function, "procedure", Shape::Routine),
                ("function", SymbolKind::Function, "function", Shape::Routine),
                ("trigger", SymbolKind::Other, "trigger", Shape::Routine),
                ("index", SymbolKind::Other, "index", Shape::Plain),
                ("sequence", SymbolKind::Other, "sequence", Shape::Plain),
                ("schema", SymbolKind::Module, "schema", Shape::Plain),
                ("package", SymbolKind::Module, "package", Shape::Container),
            ]
            .into_iter()
            .find(|(w, ..)| self.is(k, w));
            if let Some(h) = hit {
                break Some(h);
            }
            if self.text(k) == "=" {
                // `DEFINER = user@host`, `ALGORITHM = MERGE`: skip the value.
                k += 2;
                while k + 1 < n && self.text(k) == "@" {
                    k += 2;
                }
                continue;
            }
            if !self.is_any(k, CREATE_MODIFIERS) {
                break None;
            }
            k += 1;
        };
        let Some((kw, kind, mut lang, mut shape)) = found else {
            return c + 1;
        };
        let mut lang_owned = lang.to_string();
        k += 1;
        if matches!(kw, "type" | "package") && self.is(k, "body") {
            lang_owned = format!("{lang} body");
            shape = Shape::Container;
            k += 1;
        }
        lang = &lang_owned;
        if kw == "index" && self.is(k, "concurrently") {
            k += 1;
        }
        if self.is(k, "if") && self.is(k + 1, "not") && self.is(k + 2, "exists") {
            k += 3;
        }
        let named = !self.is_any(k, &["on", "authorization"]);
        let name = if named { self.name(k) } else { None };
        let after = name.as_ref().map_or(k, |(_, next)| *next);
        let (last, next) = match shape {
            Shape::Plain => self.plain_end(after),
            Shape::Routine => self.routine_end(after),
            Shape::Container => self.container_end(after),
        };
        let last = last.max(c);
        if let Some((name, _)) = name {
            let span = span_between(&self.tok(c).span, &self.tok(last).span);
            self.out.push(SymbolDecl {
                owner: None,
                name,
                kind,
                lang_kind: Some(lang.to_string()),
                span,
            });
            if shape == Shape::Container {
                let member = if kind == SymbolKind::Type {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                self.members(after, last.min(n - 1) + 1, member);
            }
        }
        next.max(c + 1)
    }

    /// A possibly qualified, possibly quoted name at `c`: the unquoted
    /// name and the position after it.
    fn name(&self, mut c: usize) -> Option<(String, usize)> {
        let mut parts = Vec::new();
        loop {
            let part = self.name_part(c)?;
            parts.push(part);
            c += 1;
            if c + 1 < self.code.len()
                && self.text(c) == "."
                && self.adjacent(c - 1, c)
                && self.adjacent(c, c + 1)
                && self.name_part(c + 1).is_some()
            {
                c += 1;
            } else {
                break;
            }
        }
        Some((parts.join("."), c))
    }

    fn name_part(&self, c: usize) -> Option<String> {
        if c >= self.code.len() || self.boundary[c] {
            return None;
        }
        let t = self.tok(c);
        let s = t.text.as_str();
        let quoted = |open: char, close: char| {
            (s.len() >= 2 && s.starts_with(open) && s.ends_with(close))
                .then(|| s[1..s.len() - 1].to_string())
        };
        let part = match t.class {
            TokenClass::Identifier => quoted('[', ']')
                .or_else(|| quoted('"', '"'))
                .unwrap_or_else(|| s.to_string()),
            TokenClass::Literal => quoted('`', '`')?,
            _ => return None,
        };
        (!part.trim().is_empty()).then_some(part)
    }

    /// End of a plain statement from `c`: (last code position, next).
    fn plain_end(&self, mut c: usize) -> (usize, usize) {
        let n = self.code.len();
        while c < n {
            if self.boundary[c] || self.is(c, "create") {
                return (c.saturating_sub(1), c);
            }
            match self.text(c) {
                "(" | "[" | "{" => match self.close_of(c) {
                    Some(close) if !self.boundary[c..close].iter().any(|&b| b) => c = close,
                    _ => {}
                },
                ";" => return (c, c + 1),
                _ => {}
            }
            c += 1;
        }
        (n.saturating_sub(1), n)
    }

    /// End of a routine whose header starts at `c`.
    fn routine_end(&self, mut c: usize) -> (usize, usize) {
        let n = self.code.len();
        let mut stack: Vec<Block> = Vec::new();
        let mut block_done = false;
        let mut begin_seen = false;
        // Whether the body is one outer `BEGIN ... END` block: the first
        // word after `AS`/`IS` is `BEGIN`, or a `BEGIN` comes with no
        // `AS`/`IS` before it (MySQL). `None` until known.
        let mut outer: Option<bool> = None;
        let mut after_as = false;
        let start = c;
        while c < n {
            if self.boundary[c] {
                return (c.saturating_sub(1), c);
            }
            if stack.is_empty() && outer.is_none() {
                if after_as {
                    outer = Some(self.is_begin(c));
                } else if self.is_any(c, &["as", "is"]) {
                    after_as = true;
                } else if self.is_begin(c) {
                    outer = Some(true);
                }
            }
            if let Some(e) = self.dollar_open(c) {
                c = self.skip_dollar(c, e);
                continue;
            }
            if self.is_begin(c) {
                stack.push(Block::Begin);
                begin_seen = true;
            } else if self.is(c, "case") {
                stack.push(Block::Case);
            } else if self.is(c, "end") {
                if self.is_any(c + 1, &["if", "loop", "while", "repeat", "for"]) {
                    c += 2;
                    continue;
                }
                let closed = stack.pop();
                if stack.is_empty() && !matches!(closed, Some(Block::Case)) {
                    block_done = true;
                }
                if self.is(c + 1, "case") {
                    c += 1;
                }
            } else if stack.is_empty() && c > start && self.is(c, "create") {
                return (c - 1, c);
            } else if stack.is_empty() && self.text(c) == ";" {
                let whole = outer == Some(true);
                if (block_done && whole) || ((!begin_seen || !whole) && self.ends_here(c + 1)) {
                    return (c, c + 1);
                }
            } else if matches!(self.text(c), "(" | "[") {
                if let Some(close) = self.close_of(c) {
                    if !self.boundary[c..close].iter().any(|&b| b) {
                        c = close;
                    }
                }
            }
            c += 1;
        }
        (n.saturating_sub(1), n)
    }

    /// After a `;` that ends a routine's statement before any `BEGIN`: the
    /// routine ends there unless a block `BEGIN` (PL/SQL declarations) or a
    /// batch separator (T-SQL body up to `GO`) comes before the next
    /// `CREATE`.
    fn ends_here(&self, mut c: usize) -> bool {
        while c < self.code.len() {
            if self.boundary[c] || self.is_begin(c) {
                return false;
            }
            if self.is(c, "create") {
                return true;
            }
            if let Some(e) = self.dollar_open(c) {
                c = self.skip_dollar(c, e);
                continue;
            }
            c += 1;
        }
        true
    }

    /// End of a PL/SQL package (or type body) from `c`: its own `END
    /// [name];` (an `END` with no block open), a separator or a `CREATE`.
    fn container_end(&self, mut c: usize) -> (usize, usize) {
        let n = self.code.len();
        let mut depth = 0usize;
        while c < n {
            if self.boundary[c] {
                return (c.saturating_sub(1), c);
            }
            if self.is_begin(c) || self.is(c, "case") {
                depth += 1;
            } else if self.is(c, "end") {
                if self.is_any(c + 1, &["if", "loop", "while", "repeat", "for"]) {
                    c += 2;
                    continue;
                }
                if depth == 0 {
                    // `END [name] ;`
                    let mut e = c + 1;
                    if e < n && self.text(e) != ";" && self.name_part(e).is_some() {
                        e += 1;
                    }
                    return if e < n && self.text(e) == ";" {
                        (e, e + 1)
                    } else {
                        (e - 1, e)
                    };
                }
                depth -= 1;
                if self.is(c + 1, "case") {
                    c += 1;
                }
            } else if depth == 0 && self.is(c, "create") {
                return (c.saturating_sub(1), c);
            }
            c += 1;
        }
        (n.saturating_sub(1), n)
    }

    /// `PROCEDURE` / `FUNCTION` members of a package in code range `[lo, hi)`.
    fn members(&mut self, lo: usize, hi: usize, kind: SymbolKind) {
        let mut c = lo;
        let mut depth = 0usize;
        while c < hi {
            if self.is_begin(c) || self.is(c, "case") {
                depth += 1;
            } else if self.is(c, "end") {
                if self.is_any(c + 1, &["if", "loop", "while", "repeat", "for", "case"]) {
                    c += 1;
                }
                depth = depth.saturating_sub(1);
            } else if depth == 0 && self.is_any(c, &["procedure", "function"]) {
                let lang = if self.is(c, "procedure") {
                    "procedure"
                } else {
                    "function"
                };
                if let Some((name, after)) = self.name(c + 1).filter(|(_, a)| *a <= hi) {
                    let last = self.member_end(after, hi);
                    let span = span_between(&self.tok(c).span, &self.tok(last).span);
                    self.out.push(SymbolDecl {
                        owner: None,
                        name,
                        kind,
                        lang_kind: Some(lang.into()),
                        span,
                    });
                    c = last + 1;
                    continue;
                }
            }
            c += 1;
        }
    }

    /// Last code position of a package member whose header continues at `c`:
    /// a spec's `;`, or a body's `END [name];`.
    fn member_end(&self, mut c: usize, hi: usize) -> usize {
        let mut body = false;
        let mut depth = 0usize;
        while c < hi {
            match self.text(c) {
                "(" => {
                    if let Some(close) = self.close_of(c).filter(|&x| x < hi) {
                        c = close;
                    }
                }
                ";" if depth == 0 && !body => return c,
                _ if !body && self.is_any(c, &["is", "as"]) => body = true,
                _ if self.is_begin(c) || self.is(c, "case") => depth += 1,
                _ if self.is(c, "end") => {
                    if self.is_any(c + 1, &["if", "loop", "while", "repeat", "for", "case"]) {
                        c += 2;
                        continue;
                    }
                    if depth <= 1 && body {
                        let mut e = c + 1;
                        while e < hi && self.text(e) != ";" && e <= c + 2 {
                            e += 1;
                        }
                        return if e < hi && self.text(e) == ";" { e } else { c };
                    }
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
            c += 1;
        }
        hi - 1
    }
}

#[cfg(test)]
mod tests;
