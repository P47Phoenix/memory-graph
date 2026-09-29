//! Haskell extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer (`HASKELL` dialect). Declarations
//! are found by layout: a top-level declaration starts a line at the
//! module body's indentation and runs through its indented block
//! (`graph_core::scan::indent_block`).
//!
//! | Haskell | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `module` | Module | `module` |
//! | `data` `newtype` `type` `class` `instance` | Type | the keyword (`type family`, `data instance`, ...) |
//! | top-level functions and values | Function | `function` |
//! | methods in a `class` or `instance` body | Method | `method` |
//!
//! Consecutive declarations of the same name (a type signature followed by
//! its equations) are one symbol spanning from the first through the last.
//! An infix definition (``x <+> y = ...``, ``a `op` b = ...``) is named by
//! its operator, as is `(<+>) :: ...`, so both group together. An
//! `instance` is named by its head (`Show (Tree a)`); a context
//! (`Eq a =>`) is not part of any name.
//!
//! Literate Haskell (`.lhs`): the extractor only sees the source, not the
//! file name, so a file is treated as literate when it has a line starting
//! with `\begin{code}` or a bird-track line (`>` followed by a space or the
//! line end). Then only bird-track lines or the lines between
//! `\begin{code}` and `\end{code}` are code; prose is tokenized with the
//! generic tokenizer, line by line, and never yields symbols.
//!
//! Known limits: explicit-brace layout (`where { ... }`) is not followed;
//! pattern bindings (`(a, b) = ...`) and Template Haskell splices are not
//! symbols; a `.hs` file with a bird-track-looking line is read as literate.
use graph_core::scan::{code_index, indent_block, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct HaskellExtractor;

/// Tokenizer dialect used for Haskell.
pub const HASKELL_TOKENIZER: TokenizerOptions = TokenizerOptions::HASKELL;

impl Extractor for HaskellExtractor {
    fn language(&self) -> &str {
        "haskell"
    }

    fn extensions(&self) -> &[&str] {
        &["hs", "lhs"]
    }

    fn version(&self) -> String {
        format!("haskell-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let (tokens, symbols) = match literate_mask(source) {
            Some(masked) => {
                let (tokens, code) = literate_tokens(source, &masked);
                (tokens, symbols(&code))
            }
            None => {
                let tokens = tokenize_with(source, HASKELL_TOKENIZER);
                let symbols = symbols(&tokens);
                (tokens, symbols)
            }
        };
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Lines of `src` with their byte offsets (without the `\n`).
fn lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut off = 0;
    src.split('\n').map(move |l| {
        let start = off;
        off += l.len() + 1;
        (start, l)
    })
}

fn is_bird(line: &str) -> bool {
    let l = line.strip_prefix('\u{feff}').unwrap_or(line);
    l.starts_with('>') && l[1..].chars().next().is_none_or(|c| c.is_whitespace())
}

fn is_begin(line: &str) -> bool {
    let l = line.strip_prefix('\u{feff}').unwrap_or(line);
    l.starts_with("\\begin{code}")
}

/// For a literate source, a copy of the same byte length where every prose
/// byte and each bird-track `>` is a space (the BOM and newlines are kept),
/// so code tokens keep their exact positions. `None` if not literate.
fn literate_mask(src: &str) -> Option<Vec<u8>> {
    if !lines(src).any(|(_, l)| is_bird(l) || is_begin(l)) {
        return None;
    }
    let mut out = src.as_bytes().to_vec();
    let mut in_code = false;
    for (off, line) in lines(src) {
        let bom = if line.starts_with('\u{feff}') { 3 } else { 0 };
        let body = &line[bom..];
        let (from, to) = if in_code {
            if body.starts_with("\\end{code}") {
                in_code = false;
                (bom, line.len())
            } else {
                (0, 0)
            }
        } else if is_begin(line) {
            in_code = true;
            (bom, line.len())
        } else if is_bird(line) {
            (bom, bom + 1)
        } else {
            (bom, line.len())
        };
        for b in &mut out[off + from..off + to] {
            if *b != b'\r' {
                *b = b' ';
            }
        }
    }
    Some(out)
}

/// Tokens of a literate source: code tokens from the masked copy, plus
/// generic tokens for the masked-out prose and bird marks, all with exact
/// positions in `src`.
/// Also returns the code tokens alone, for the symbol scan.
fn literate_tokens(src: &str, masked: &[u8]) -> (Vec<TokenDecl>, Vec<TokenDecl>) {
    // Masking replaces whole UTF-8 sequences with spaces, so it stays UTF-8.
    let masked = std::str::from_utf8(masked).unwrap_or(src);
    let mut out = tokenize_with(masked, HASKELL_TOKENIZER);
    // A code token running into prose (an unclosed `{-`) keeps its source text.
    for t in &mut out {
        t.text = src[t.span.start as usize..t.span.end as usize].to_string();
    }
    let code = out.clone();
    for (line_no, (off, line)) in lines(src).enumerate() {
        let m = &masked[off..off + line.len()];
        // Runs of bytes that were masked out: prose (and bird marks).
        let bytes = line.as_bytes();
        let mb = m.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == mb[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i] != mb[i] {
                i += 1;
            }
            // Masked runs cover whole characters, so these are boundaries.
            let prefix_cols = line[..start].chars().filter(|&c| c != '\u{feff}').count() as u32;
            for mut t in tokenize_with(&line[start..i], TokenizerOptions::DEFAULT) {
                t.span.start += (off + start) as u32;
                t.span.end += (off + start) as u32;
                t.span.start_line = line_no as u32 + 1;
                t.span.end_line = line_no as u32 + 1;
                t.span.start_col += prefix_cols;
                t.span.end_col += prefix_cols;
                out.push(t);
            }
        }
    }
    out.sort_by_key(|t| t.span.start);
    // Drop prose tokens inside a code token that ran over them.
    let mut end = 0;
    out.retain(|t| {
        let keep = t.span.start >= end;
        if keep {
            end = t.span.end;
        }
        keep
    });
    (out, code)
}

/// Symbols in Haskell tokens (as produced with [`HASKELL_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, &[TokenClass::Comment]);
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
    };
    s.run();
    s.out
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Top,
    Class,
}

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    out: Vec<SymbolDecl>,
}

/// A run of same-named declarations being grouped.
struct Group {
    name: String,
    first: usize,
    last: usize,
}

const NOT_NAMES: &[&str] = &[
    "let", "where", "if", "then", "else", "case", "of", "do", "in", "import", "infix", "infixl",
    "infixr", "deriving", "foreign", "default", "module", "pattern",
];

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    fn line_start(&self, c: usize) -> bool {
        c == 0 || self.tok(c).span.start_line > self.tok(c - 1).span.end_line
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        self.tok(a).span.end == self.tok(b).span.start
    }

    fn is_op(&self, c: usize) -> bool {
        self.tok(c).class == TokenClass::Operator
            || matches!(self.text(c), ":" | "." | "$" | "#" | "\\")
    }

    /// An operator made of adjacent symbol tokens starting at `c`, up to
    /// `hi`: (text, last position).
    fn op_run(&self, c: usize, hi: usize) -> Option<(String, usize)> {
        if c >= hi || !self.is_op(c) {
            return None;
        }
        let mut s = self.text(c).to_string();
        let mut last = c;
        while last + 1 < hi && self.is_op(last + 1) && self.adjacent(last, last + 1) {
            last += 1;
            s.push_str(self.text(last));
        }
        Some((s, last))
    }

    fn block_end(&self, c: usize, hi: usize) -> usize {
        let last = indent_block(self.tokens, self.code[c], &[TokenClass::Comment]);
        let pos = self.code.partition_point(|&i| i <= last).saturating_sub(1);
        pos.clamp(c, hi - 1)
    }

    /// Code position of `where` at bracket depth 0 in `[from, to]`.
    fn find_where(&self, from: usize, to: usize) -> Option<usize> {
        let mut depth = 0usize;
        for k in from..=to {
            match self.text(k) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                "where" if depth == 0 => return Some(k),
                _ => {}
            }
        }
        None
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

    fn run(&mut self) {
        let n = self.code.len();
        if n == 0 {
            return;
        }
        let mut lo = 0;
        if self.text(0) == "module" {
            let mut name = String::new();
            let mut c = 1;
            while c < n
                && self.text(c) != "where"
                && (self.tok(c).class == TokenClass::Identifier || self.text(c) == ".")
            {
                name.push_str(self.text(c));
                c += 1;
            }
            if !name.is_empty() {
                self.emit(name, SymbolKind::Module, "module", 0, n - 1);
            }
            lo = self.find_where(c.min(n - 1), n - 1).map_or(c, |w| w + 1);
        }
        self.decls(lo, n, Ctx::Top);
    }

    /// Name of the function or value declared at `c` (a signature or an
    /// equation), or `None`.
    fn fn_name(&self, c: usize, end: usize) -> Option<String> {
        let hi = end + 1;
        let t = self.tok(c);
        if self.text(c) == "(" {
            let (op, last) = self.op_run(c + 1, hi)?;
            return (last + 1 < hi && self.text(last + 1) == ")").then_some(op);
        }
        let first = t.text.chars().next()?;
        if t.class != TokenClass::Identifier
            || !(first.is_lowercase() || first == '_')
            || NOT_NAMES.contains(&t.text.as_str())
        {
            return None;
        }
        if c + 1 < hi {
            // Backticks lex as a quoted literal: ``x `op` y``.
            let next = self.text(c + 1);
            if next.len() > 2 && next.starts_with('`') && next.ends_with('`') {
                return Some(next[1..next.len() - 1].to_string());
            }
            if let Some((op, _)) = self.op_run(c + 1, hi) {
                if !matches!(op.as_str(), "=" | "|" | "@" | "!" | "~" | "::" | "," | ":")
                    && !op.starts_with("::")
                {
                    return Some(op);
                }
            }
        }
        Some(t.text.clone())
    }

    fn flush(&mut self, group: &mut Option<Group>, ctx: Ctx) {
        if let Some(g) = group.take() {
            let (kind, lk) = match ctx {
                Ctx::Top => (SymbolKind::Function, "function"),
                Ctx::Class => (SymbolKind::Method, "method"),
            };
            self.emit(g.name, kind, lk, g.first, g.last);
        }
    }

    /// Declarations in code positions `[lo, hi)`.
    fn decls(&mut self, lo: usize, hi: usize, ctx: Ctx) {
        let Some(level) = (lo..hi).find(|&c| self.line_start(c)) else {
            return;
        };
        let col = self.tok(level).span.start_col;
        let mut group: Option<Group> = None;
        let mut c = level;
        while c < hi {
            if !self.line_start(c) || self.tok(c).span.start_col != col {
                c += 1;
                continue;
            }
            let end = self.block_end(c, hi);
            let kw = self.text(c).to_string();
            if matches!(
                kw.as_str(),
                "data" | "newtype" | "type" | "class" | "instance"
            ) {
                self.flush(&mut group, ctx);
                self.type_decl(c, end, &kw, ctx);
            } else if let Some(name) = self.fn_name(c, end) {
                match &mut group {
                    Some(g) if g.name == name => g.last = end,
                    _ => {
                        self.flush(&mut group, ctx);
                        group = Some(Group {
                            name,
                            first: c,
                            last: end,
                        });
                    }
                }
            } else {
                self.flush(&mut group, ctx);
            }
            c = end + 1;
        }
        self.flush(&mut group, ctx);
    }

    fn type_decl(&mut self, c: usize, end: usize, kw: &str, ctx: Ctx) {
        let mut lk = kw.to_string();
        let mut k = c + 1;
        if k <= end && matches!(self.text(k), "family" | "instance") {
            lk = format!("{kw} {}", self.text(k));
            k += 1;
        }
        let where_at = self.find_where(k.min(end), end);
        // The head runs to `where`, or to `=` / `|` / `::` for data and types.
        let mut head_end = where_at.map_or(end, |w| w.saturating_sub(1));
        if kw != "class" && kw != "instance" {
            let mut depth = 0usize;
            for j in k..=head_end.max(k).min(end) {
                match self.text(j) {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    "=" | "|" | ":" if depth == 0 => {
                        head_end = j.saturating_sub(1);
                        break;
                    }
                    _ => {}
                }
            }
        }
        // Skip a context: everything through the last `=>` in the head.
        if let Some(j) = (k..head_end.min(end))
            .rev()
            .find(|&j| self.text(j) == "=" && self.text(j + 1) == ">" && self.adjacent(j, j + 1))
        {
            k = j + 2;
        }
        if k > head_end || k > end {
            return;
        }
        let name = if kw == "instance" {
            let src_start = self.tok(k).span.start;
            let mut s = String::new();
            let mut prev_end = src_start;
            for j in k..=head_end {
                let t = self.tok(j);
                if t.span.start > prev_end && !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&t.text);
                prev_end = t.span.end;
            }
            s
        } else if self.text(k) == "(" {
            match self.op_run(k + 1, end + 1) {
                Some((op, _)) => op,
                None => return,
            }
        } else if self.tok(k).class == TokenClass::Identifier {
            self.text(k).to_string()
        } else {
            return;
        };
        self.emit(name, SymbolKind::Type, &lk, c, end);
        if ctx == Ctx::Top && (kw == "class" || kw == "instance") {
            if let Some(w) = where_at {
                self.decls(w + 1, end + 1, Ctx::Class);
            }
        }
    }
}

#[cfg(test)]
mod tests;
