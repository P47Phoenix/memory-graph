//! R extractor: a token-stream scanner, not a parser. Also used for R
//! Markdown and Quarto documents (`rmd`, `qmd`).
//!
//! | R | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `name <- function(...)`, `name <<- function`, `name = function`, `name <- \(x)` | Function | `function` |
//! | `function(...) body -> name` | Function | `function` |
//! | `setClass("N", ...)` / `setRefClass` / `R6Class` | Type | `setClass` `setRefClass` `R6Class` |
//! | `setGeneric("f", ...)` / `setMethod("f", ...)` | Method | `generic` / `method` |
//! | a named function inside a class call (`R6Class(public = list(m = function() ...))`) | Method | `function` |
//!
//! A function's span runs from its name through its body: a `{ ... }` block,
//! or an expression that ends at the end of its line (groups may span
//! lines), a `,` or a closing bracket. Names may be dotted, backquoted
//! (`` `odd name` ``, reported without the backquotes) or accessors
//! (`obj$method`). A class or generic is named by its first string argument
//! (a class assigned without one by the assigned name); its span covers the
//! assignment when there is one, otherwise the call.
//!
//! R Markdown / Quarto: when the file has a line starting with ```` ``` ````,
//! it is treated as a document: only fenced code chunks whose header is
//! `{r ...}` are tokenized as R and scanned for symbols; everything else
//! gets the generic tokenizer and no symbols.
//!
//! Known limits: a body expression continued on the next line by a trailing
//! operator is cut at the line end; `setMethod` names the generic, not the
//! class; `assign("name", function...)` and S4 `setValidity` are not symbols.
use graph_core::scan::{code_close_table, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct RExtractor;

/// Tokenizer dialect used for R.
pub const R_TOKENIZER: TokenizerOptions = TokenizerOptions::R;

impl Extractor for RExtractor {
    fn language(&self) -> &str {
        "r"
    }

    fn extensions(&self) -> &[&str] {
        &["r", "rmd", "qmd"]
    }

    fn version(&self) -> String {
        format!("r-scan-2+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        if !is_document(source) {
            let tokens = tokenize_with(source, R_TOKENIZER);
            let symbols = symbols(&tokens);
            return Extraction {
                symbols,
                tokens,
                has_errors: false,
            };
        }
        let mut tokens = Vec::new();
        let mut syms = Vec::new();
        for seg in segments(source) {
            let text = &source[seg.start..seg.end];
            let opts = if seg.code {
                R_TOKENIZER
            } else {
                TokenizerOptions::DEFAULT
            };
            let mut part = tokenize_with(text, opts);
            for t in &mut part {
                shift(&mut t.span, seg.start as u32, seg.line - 1);
            }
            if seg.code {
                syms.extend(symbols(&part));
            }
            tokens.extend(part);
        }
        Extraction {
            symbols: syms,
            tokens,
            has_errors: false,
        }
    }
}

fn shift(span: &mut graph_core::Span, bytes: u32, lines: u32) {
    span.start += bytes;
    span.end += bytes;
    span.start_line += lines;
    span.end_line += lines;
}

/// A Markdown document (R Markdown, Quarto): some line starts with a fence.
fn is_document(source: &str) -> bool {
    source
        .trim_start_matches('\u{feff}')
        .lines()
        .any(|l| l.starts_with("```"))
}

/// A run of whole lines: `[start, end)` in bytes, first line number
/// (1-based), and whether it is the body of an R chunk.
struct Segment {
    start: usize,
    end: usize,
    line: u32,
    code: bool,
}

/// Splits a document into prose (fences included) and R chunk bodies.
fn segments(source: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    // The open fence, if any: (backtick count, whether it is an R chunk).
    let mut fence: Option<(usize, bool)> = None;
    let mut offset = 0;
    for (i, raw) in source.split_inclusive('\n').enumerate() {
        let line = raw.trim_start_matches('\u{feff}').trim_start();
        let ticks = line.bytes().take_while(|&b| b == b'`').count();
        let rest = &line[ticks..];
        let code = match fence {
            Some((len, is_r)) => {
                if ticks >= len && rest.trim().is_empty() {
                    fence = None;
                    false
                } else {
                    is_r
                }
            }
            None if ticks >= 3 => {
                let rest = rest.trim_start();
                let r_chunk = rest.strip_prefix('{').is_some_and(|h| {
                    let h = h.trim_start();
                    (h.starts_with('r') || h.starts_with('R'))
                        && h[1..]
                            .chars()
                            .next()
                            .is_some_and(|c| matches!(c, '}' | ',' | ' ' | '\t'))
                });
                fence = Some((ticks, r_chunk));
                false
            }
            None => false,
        };
        let end = offset + raw.len();
        match out.last_mut() {
            Some(seg) if seg.code == code => seg.end = end,
            _ => out.push(Segment {
                start: offset,
                end,
                line: i as u32 + 1,
                code,
            }),
        }
        offset = end;
    }
    out
}

/// Symbols in R tokens (as produced with [`R_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.class != TokenClass::Comment)
        .map(|(i, _)| i)
        .collect();
    let mut s = Scanner {
        tokens,
        code: &code,
        closes: code_close_table(tokens, &code),
        out: Vec::new(),
    };
    for c in 0..code.len() {
        s.at(c);
    }
    let mut out = drop_overlaps(s.out);
    // A named function inside a class definition is a method.
    let types: Vec<(u32, u32)> = out
        .iter()
        .filter(|x| x.kind == SymbolKind::Type)
        .map(|x| (x.span.start, x.span.end))
        .collect();
    for x in &mut out {
        if x.kind == SymbolKind::Function
            && types
                .iter()
                .any(|&(a, b)| a <= x.span.start && x.span.end <= b)
        {
            x.kind = SymbolKind::Method;
        }
    }
    out
}

/// Keeps symbols whose spans nest or are disjoint (drops a later-starting
/// symbol that would partially overlap an earlier one on odd input).
fn drop_overlaps(mut syms: Vec<SymbolDecl>) -> Vec<SymbolDecl> {
    syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
    let mut open: Vec<u32> = Vec::new();
    let mut out = Vec::with_capacity(syms.len());
    for s in syms {
        while open.last().is_some_and(|&e| e <= s.span.start) {
            open.pop();
        }
        if open.last().is_some_and(|&e| s.span.end > e) {
            continue;
        }
        open.push(s.span.end);
        out.push(s);
    }
    out
}

const CLASS_CALLS: &[&str] = &["setClass", "setRefClass", "R6Class"];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    /// [`code_close_table`] of `code`: closers found in one linear pass.
    closes: Vec<Option<usize>>,
    out: Vec<SymbolDecl>,
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    fn n(&self) -> usize {
        self.code.len()
    }

    fn is(&self, c: usize, s: &str) -> bool {
        c < self.n() && self.tok(c).class != TokenClass::Literal && self.text(c) == s
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        b < self.n() && self.tok(a).span.end == self.tok(b).span.start
    }

    fn close_of(&self, c: usize) -> Option<usize> {
        self.closes[c]
    }

    /// Length of a left-assignment operator at `c` (`<-`, `<<-`, `=`).
    fn left_assign(&self, c: usize) -> Option<usize> {
        if self.is(c, "<") && self.is(c + 1, "-") && self.adjacent(c, c + 1) {
            let prev_lt = c > 0 && self.is(c - 1, "<") && self.adjacent(c - 1, c);
            return (!prev_lt).then_some(2);
        }
        if self.is(c, "<")
            && self.is(c + 1, "<")
            && self.is(c + 2, "-")
            && self.adjacent(c, c + 1)
            && self.adjacent(c + 1, c + 2)
        {
            return Some(3);
        }
        if self.is(c, "=") {
            let glued_before = c > 0
                && self.adjacent(c - 1, c)
                && matches!(self.text(c - 1), "=" | "<" | ">" | "!");
            let glued_after = self.adjacent(c, c + 1) && self.is(c + 1, "=");
            return (!glued_before && !glued_after).then_some(1);
        }
        None
    }

    /// A left-assignment operator ends right before `c`.
    fn assigned(&self, c: usize) -> bool {
        (1..=3).any(|len| c >= len && self.left_assign(c - len) == Some(len))
    }

    /// The assignment target ending at `c`: (name, first code position).
    fn target(&self, c: usize) -> Option<(String, usize)> {
        let t = self.tok(c);
        let plain = |t: &TokenDecl| t.class == TokenClass::Identifier;
        if t.class == TokenClass::Literal && t.text.len() >= 2 && t.text.starts_with('`') {
            return Some((t.text[1..t.text.len() - 1].to_string(), c));
        }
        if !plain(t) {
            return None;
        }
        let mut first = c;
        while first >= 2
            && self.adjacent(first - 1, first)
            && matches!(self.text(first - 1), "." | "$" | "@")
            && self.adjacent(first - 2, first - 1)
            && plain(self.tok(first - 2))
        {
            first -= 2;
        }
        let name: String = (first..=c).map(|k| self.text(k)).collect();
        Some((name, first))
    }

    /// A function literal (`function(...)` or `\(...)`) at `c`: the code
    /// position of its last token.
    fn function_end(&self, c: usize) -> Option<usize> {
        let is_fn = (self.tok(c).class == TokenClass::Identifier && self.text(c) == "function")
            || (self.is(c, "\\") && self.adjacent(c, c + 1));
        if !is_fn || !self.is(c + 1, "(") {
            return None;
        }
        let params = self.close_of(c + 1)?;
        let body = params + 1;
        if body >= self.n() {
            return Some(params);
        }
        Some(self.expr_end(body))
    }

    /// Last code position of the expression starting at `c`.
    fn expr_end(&self, c: usize) -> usize {
        let mut last = c;
        let mut k = c;
        loop {
            if matches!(self.text(k), "(" | "[" | "{") && self.tok(k).class != TokenClass::Literal {
                match self.close_of(k) {
                    Some(close) => k = close,
                    None => return last.max(k.saturating_sub(1)).max(c),
                }
            }
            last = k;
            let next = k + 1;
            if next >= self.n()
                || self.tok(next).span.start_line != self.tok(k).span.end_line
                || matches!(self.text(next), ";" | "," | ")" | "]" | "}")
                || self.right_assign(next).is_some()
            {
                return last;
            }
            k = next;
        }
    }

    /// Length of a right-assignment operator at `c` (`->`, `->>`).
    fn right_assign(&self, c: usize) -> Option<usize> {
        if self.is(c, "-") && self.is(c + 1, ">") && self.adjacent(c, c + 1) {
            let more = self.is(c + 2, ">") && self.adjacent(c + 1, c + 2);
            return Some(if more { 3 } else { 2 });
        }
        None
    }

    fn push(&mut self, name: String, kind: SymbolKind, lang: &str, first: usize, last: usize) {
        if name.is_empty() {
            return;
        }
        let span = span_between(&self.tok(first).span, &self.tok(last).span);
        self.out.push(SymbolDecl {
            owner: None,
            name,
            kind,
            lang_kind: Some(lang.into()),
            span,
        });
    }

    fn at(&mut self, c: usize) {
        // `name <- function ...` / `name <- setRefClass(...)`
        if c > 0 {
            if let Some(len) = self.left_assign(c) {
                let rhs = c + len;
                let Some((name, first)) = self.target(c - 1) else {
                    return;
                };
                if rhs >= self.n() {
                    return;
                }
                if let Some(last) = self.function_end(rhs) {
                    self.push(name, SymbolKind::Function, "function", first, last);
                } else if let Some(call) = self.class_call(rhs) {
                    let (label, last) = call;
                    let named = self.first_string(rhs).unwrap_or(name);
                    self.push(named, SymbolKind::Type, &label, first, last);
                }
                return;
            }
        }
        // `function(...) body -> name`
        if let Some(last) = self.function_end(c) {
            if !self.assigned(c) {
                if let Some(len) = self.right_assign(last + 1) {
                    let t = last + 1 + len;
                    if t < self.n() {
                        if let Some((name, _)) = self.target(t) {
                            self.push(name, SymbolKind::Function, "function", c, t);
                        }
                    }
                }
            }
            return;
        }
        // `setClass("A", ...)` not assigned; `setGeneric` / `setMethod`.
        if !self.assigned(c) && !(c > 0 && self.is(c - 1, ":")) {
            if let Some((label, last)) = self.class_call(c) {
                if let Some(name) = self.first_string(c) {
                    self.push(name, SymbolKind::Type, &label, c, last);
                }
                return;
            }
        }
        if self.tok(c).class == TokenClass::Identifier && self.is(c + 1, "(") {
            let lang = match self.text(c) {
                "setGeneric" => "generic",
                "setMethod" => "method",
                _ => return,
            };
            if let (Some(name), Some(close)) = (self.first_string(c), self.close_of(c + 1)) {
                self.push(name, SymbolKind::Method, lang, c, close);
            }
        }
    }

    /// A class-defining call at `c` (`setClass(`, `R6::R6Class(`): its
    /// label and closing `)`.
    fn class_call(&self, mut c: usize) -> Option<(String, usize)> {
        // `R6::R6Class(...)`, `methods::setClass(...)`
        if self.tok(c).class == TokenClass::Identifier
            && self.is(c + 1, ":")
            && self.is(c + 2, ":")
            && c + 3 < self.n()
        {
            c += 3;
        }
        if self.tok(c).class != TokenClass::Identifier
            || !CLASS_CALLS.contains(&self.text(c))
            || !self.is(c + 1, "(")
        {
            return None;
        }
        let close = self.close_of(c + 1)?;
        Some((self.text(c).to_string(), close))
    }

    /// The first string literal in the first argument of the call whose
    /// name is at `c` (after an optional `pkg::`), unquoted.
    fn first_string(&self, mut c: usize) -> Option<String> {
        if self.is(c + 1, ":") && self.is(c + 2, ":") {
            c += 3;
        }
        if !self.is(c + 1, "(") {
            return None;
        }
        let mut k = c + 2;
        while k < self.n() && !self.is(k, ",") && !self.is(k, ")") {
            let t = self.tok(k);
            if t.class == TokenClass::Literal
                && t.text.len() >= 2
                && (t.text.starts_with('"') || t.text.starts_with('\''))
            {
                let s = &t.text[1..t.text.len() - 1];
                return (!s.is_empty()).then(|| s.to_string());
            }
            k += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests;
