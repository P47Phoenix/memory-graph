//! Shell extractor (sh, bash, zsh, ksh): a token-stream scanner, not a
//! parser.
//!
//! | Shell | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `name() { ... }`, `function name { ... }`, `function name() ( ... )` | Function | `function` |
//! | `export NAME[=value]`, `declare -x NAME` / `typeset -x` | Variable | `export` / `declare` |
//! | `readonly NAME[=value]`, `declare -r NAME` / `typeset -r` | Constant | `readonly` / `declare` |
//!
//! A function's span runs from its first token (`function` or the name)
//! through the end of its `{ ... }` or `( ... )` body; functions defined
//! inside functions nest. Names may contain `-`, `:`, `.` and `@`
//! (`foo-bar::baz()`). A variable's span runs from the keyword (the name
//! for the second and later names of one command) through its value.
//! Declarations are recognized in command position only (line start or
//! after `;` `&` `|` `{` `(` `then` `do` `else`).
//!
//! Known limits: a function whose body is a bare compound command
//! (`f() if ...; fi`) is not a symbol; `export -f` / `declare -f` and
//! `declare` without `-r`/`-x` are ignored; a `case` word used as a plain
//! argument (`echo case`) confuses the matching of `)` inside a `(...)`
//! body; plain `NAME=value` assignments and `local` are not symbols; zsh's
//! multi-name `function a b c { ... }` is not a symbol (only one name per
//! definition is recognized).
use graph_core::scan::{span_between, NestedEnds, Step};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct ShellExtractor;

/// Tokenizer dialect used for shell scripts.
pub const SHELL_TOKENIZER: TokenizerOptions = TokenizerOptions::SHELL;

impl Extractor for ShellExtractor {
    fn language(&self) -> &str {
        "shell"
    }

    fn extensions(&self) -> &[&str] {
        &["sh", "bash", "zsh", "ksh"]
    }

    fn version(&self) -> String {
        format!("shell-scan-2+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, SHELL_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in shell tokens (as produced with [`SHELL_TOKENIZER`]).
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
        out: Vec::new(),
        braces: NestedEnds::new(),
    };
    for c in 0..code.len() {
        if s.command_position(c) {
            s.function(c);
            s.variables(c);
        }
    }
    drop_overlaps(s.out)
}

/// Keeps symbols whose spans nest or are disjoint: on odd input (an
/// `export` value group running into a function body) a symbol that would
/// partially overlap an earlier-starting one is dropped.
fn drop_overlaps(mut syms: Vec<SymbolDecl>) -> Vec<SymbolDecl> {
    syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
    let mut open: Vec<u64> = Vec::new();
    let mut out = Vec::with_capacity(syms.len());
    for s in syms {
        let (start, end) = (u64::from(s.span.start), u64::from(s.span.end));
        while open.last().is_some_and(|&e| e <= start) {
            open.pop();
        }
        if open.last().is_some_and(|&e| end > e) {
            continue;
        }
        open.push(end);
        out.push(s);
    }
    out
}

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    out: Vec<SymbolDecl>,
    /// `{` closers, remembered between lookups.
    braces: NestedEnds,
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    fn is_punct(&self, c: usize, p: &str) -> bool {
        c < self.code.len() && self.tok(c).class != TokenClass::Literal && self.text(c) == p
    }

    fn is_word(&self, c: usize, w: &str) -> bool {
        c < self.code.len() && self.tok(c).class == TokenClass::Identifier && self.text(c) == w
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        b < self.code.len() && self.tok(a).span.end == self.tok(b).span.start
    }

    fn same_line(&self, a: usize, b: usize) -> bool {
        b < self.code.len() && self.tok(a).span.end_line == self.tok(b).span.start_line
    }

    /// A command separator or closer that ends an `export` word list.
    fn is_separator(&self, c: usize) -> bool {
        c < self.code.len()
            && self.tok(c).class != TokenClass::Literal
            && matches!(self.text(c), ";" | "&" | "|" | ")" | "}")
    }

    fn command_position(&self, c: usize) -> bool {
        if c == 0 || !self.same_line(c - 1, c) {
            return true;
        }
        let p = c - 1;
        matches!(self.text(p), ";" | "&" | "|" | "{" | "(")
            && self.tok(p).class != TokenClass::Literal
            || ["then", "do", "else"].iter().any(|w| self.is_word(p, w))
    }

    /// A function name starting at `c`: adjacent identifier pieces joined by
    /// `-` `:` `.` `@`. Returns (name, last code position of the name).
    fn name(&self, c: usize) -> Option<(String, usize)> {
        let ok_start = |k: usize| {
            let t = self.tok(k);
            t.class == TokenClass::Identifier && !t.text.starts_with('$')
        };
        if c >= self.code.len() || !ok_start(c) {
            return None;
        }
        let mut name = self.text(c).to_string();
        let mut last = c;
        while self.adjacent(last, last + 1) {
            let k = last + 1;
            let t = self.tok(k);
            let piece = matches!(t.text.as_str(), "-" | ":" | "." | "@" | "+")
                || (t.class == TokenClass::Identifier && !t.text.starts_with('$'))
                || (t.class == TokenClass::Literal && t.text.bytes().all(|b| b.is_ascii_digit()));
            if !piece {
                break;
            }
            name.push_str(&t.text);
            last = k;
        }
        Some((name, last))
    }

    /// `name() body` or `function name [()] body` at `c`.
    fn function(&mut self, c: usize) {
        let keyword = self.is_word(c, "function");
        let (name, mut k) = if keyword {
            if !self.same_line(c, c + 1) {
                return;
            }
            match self.name(c + 1) {
                Some((n, last)) => (n, last + 1),
                None => return,
            }
        } else {
            match self.name(c) {
                Some((n, last)) => (n, last + 1),
                None => return,
            }
        };
        let parens = self.is_punct(k, "(") && self.is_punct(k + 1, ")");
        if parens {
            k += 2;
        } else if !keyword {
            return;
        }
        if name == "function" && !keyword {
            return;
        }
        if k >= self.code.len() || !(self.is_punct(k, "{") || self.is_punct(k, "(")) {
            return;
        }
        let Some(close) = self.close_of(k) else {
            return;
        };
        let span = span_between(&self.tok(c).span, &self.tok(close).span);
        self.out.push(SymbolDecl {
            owner: None,
            name,
            kind: SymbolKind::Function,
            lang_kind: Some("function".into()),
            span,
        });
    }

    /// The code position closing the `{` or `(` at `open`. Only the same
    /// kind of delimiter counts; inside `case ... esac` a `)` that would close
    /// below the `case`'s own depth is a pattern terminator.
    fn close_of(&self, open: usize) -> Option<usize> {
        if self.text(open) == "{" {
            // Memoized: one `{` scan per `function f() {` over a long
            // unclosed run would be quadratic.
            return self.braces.find(open, self.code.len(), |k| {
                let t = self.tok(k);
                match t.text.as_str() {
                    _ if matches!(t.class, TokenClass::Identifier | TokenClass::Literal) => {
                        Step::Other
                    }
                    "{" => Step::Open,
                    "}" => Step::Close,
                    _ => Step::Other,
                }
            });
        }
        let (o, cl) = ("(", ")");
        let mut depth = 0usize;
        let mut cases: Vec<usize> = Vec::new();
        for k in open..self.code.len() {
            let t = self.tok(k);
            if t.class == TokenClass::Identifier {
                match t.text.as_str() {
                    "case" => cases.push(depth),
                    "esac" => {
                        cases.pop();
                    }
                    _ => {}
                }
                continue;
            }
            if t.class == TokenClass::Literal {
                continue;
            }
            if t.text == o {
                depth += 1;
            } else if t.text == cl {
                if o == "(" && cases.last() == Some(&depth) {
                    continue;
                }
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(k);
                }
            }
        }
        None
    }

    /// `export` / `readonly` / `declare -r|-x` / `typeset -r|-x` at `c`.
    fn variables(&mut self, c: usize) {
        let kw = self.text(c).to_string();
        if self.tok(c).class != TokenClass::Identifier
            || !matches!(kw.as_str(), "export" | "readonly" | "declare" | "typeset")
        {
            return;
        }
        let n = self.code.len();
        let mut k = c + 1;
        let mut flags = String::new();
        // Options: `-rx`, `+x`, `--`.
        while self.same_line(k - 1, k) && matches!(self.text(k), "-" | "+") {
            let mut e = k;
            while self.adjacent(e, e + 1) && self.tok(e + 1).class != TokenClass::Literal {
                e += 1;
                flags.push_str(self.text(e));
            }
            k = e + 1;
        }
        if flags.contains(['f', 'F', 'n', 'p']) {
            return;
        }
        let (kind, lang) = match kw.as_str() {
            "export" => (SymbolKind::Variable, "export"),
            "readonly" => (SymbolKind::Constant, "readonly"),
            _ if flags.contains('r') => (SymbolKind::Constant, "declare"),
            _ if flags.contains('x') => (SymbolKind::Variable, "declare"),
            _ => return,
        };
        let mut first = true;
        while k < n && self.same_line(k - 1, k) {
            if self.is_separator(k) {
                break;
            }
            let start = k;
            // The word runs through adjacent tokens (and groups after `$`/`=`).
            let mut last = k;
            let mut e = k;
            loop {
                if self.is_punct(e, "(") && e > start {
                    match self.close_of(e) {
                        Some(close) => e = close,
                        None => break,
                    }
                }
                last = e;
                if !self.adjacent(e, e + 1) || self.is_separator(e + 1) {
                    break;
                }
                e += 1;
            }
            let t = self.tok(start);
            let is_name = t.class == TokenClass::Identifier
                && !t.text.starts_with('$')
                && (start == last
                    || self.is_punct(start + 1, "=")
                    || self.is_punct(start + 1, "+"));
            if is_name {
                let from = if first { c } else { start };
                let span = span_between(&self.tok(from).span, &self.tok(last).span);
                self.out.push(SymbolDecl {
                    owner: None,
                    name: t.text.clone(),
                    kind,
                    lang_kind: Some(lang.into()),
                    span,
                });
                first = false;
            }
            k = last + 1;
        }
    }
}

#[cfg(test)]
mod tests;
