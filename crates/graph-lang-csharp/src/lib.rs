//! C# extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer (`csharp_strings` dialect). Symbols
//! are found by walking declarations at namespace and type level:
//!
//! | C# | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `namespace` (block or file-scoped) | Module | `namespace` |
//! | `class` `struct` `interface` `record` `enum` | Type | the keyword |
//! | `delegate` | Type | `delegate` |
//! | methods, constructors, finalizers, operators | Method | `method` `constructor` `finalizer` `operator` |
//! | properties, indexers, events | Variable | `property` `indexer` `event` |
//! | fields / `const` | Variable / Constant | `field` / `const` |
//!
//! A declaration's span runs from its first token (attributes and modifiers
//! included) through its closing `}` or `;`. Method bodies are not scanned
//! (no local functions or variables). Odd input never sets `has_errors`.
//!
//! Known limits: preprocessor lines are dropped, so when both `#if`/`#else`
//! branches open a brace the braces are unbalanced; an unmatched `{` then
//! runs to the end of its enclosing range, so later declarations are still
//! found (possibly nested one level too deep), except those that end up
//! inside an unclosed method body, which is never scanned. A multi-declarator field
//! (`int x = 1, y = 2;`) yields one symbol, named by its first declarator;
//! fixed-size buffers (`fixed byte b[4];`) and top-level local functions are
//! not symbols; a string nested inside an interpolation hole (`$"{"x"}"`)
//! ends the literal early (a tokenizer limit).
use graph_core::scan::{matching_close, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct CSharpExtractor;

/// Tokenizer dialect used for C#.
pub const CSHARP_TOKENIZER: TokenizerOptions = TokenizerOptions {
    rust_literals: false,
    single_quote_strings: false,
    csharp_strings: true,
    markup: false,
    aspx: false,
    regex_literals: false,
};

impl Extractor for CSharpExtractor {
    fn language(&self) -> &str {
        "csharp"
    }

    fn extensions(&self) -> &[&str] {
        &["cs", "csx"]
    }

    fn version(&self) -> String {
        format!("csharp-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, CSHARP_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in C# tokens (as produced with [`CSHARP_TOKENIZER`]). Exposed so
/// other extractors (e.g. for server-side code embedded in markup) can reuse
/// the scanner on a token slice.
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_indices(tokens);
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
    };
    s.body(0, code.len(), &Level::Namespace);
    s.out
}

/// Indices of tokens that matter for structure: no comments, no
/// preprocessor lines (`#region`, `#if`, ...).
fn code_indices(tokens: &[TokenDecl]) -> Vec<usize> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut skip_line = None;
    let mut prev_line = 0;
    for (i, t) in tokens.iter().enumerate() {
        if skip_line == Some(t.span.start_line) {
            continue;
        }
        if t.class == TokenClass::Comment {
            continue;
        }
        let first_on_line = i == 0 || t.span.start_line != prev_line;
        prev_line = t.span.end_line;
        if t.text == "#" && first_on_line {
            skip_line = Some(t.span.start_line);
            continue;
        }
        out.push(i);
    }
    out
}

enum Level {
    /// File or namespace body: only namespaces, types and delegates.
    Namespace,
    /// Body of the named class/struct/interface/record: members too.
    Type(String),
}

const TYPE_KEYWORDS: &[&str] = &["class", "struct", "interface", "record", "enum"];

/// Words that can precede a tuple return type's `(`, so that `(` does not
/// start a parameter list.
const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "virtual",
    "override",
    "abstract",
    "sealed",
    "async",
    "extern",
    "unsafe",
    "new",
    "readonly",
    "partial",
    "required",
    "volatile",
    "file",
    "ref",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    /// Indices into `tokens` of code tokens.
    code: &'a [usize],
    out: Vec<SymbolDecl>,
}

/// How a declaration header ended.
enum End {
    /// `{` at code position `open`, closed by `}` at code position `close`;
    /// `last` is `close` or the `;` of a trailing `= initializer;`.
    Block {
        open: usize,
        close: usize,
        last: usize,
    },
    /// `;` at code position `at` (after an optional `=>` or `=` expression).
    Semi { at: usize, arrow: bool },
}

impl Scanner<'_> {
    fn tok(&self, c: usize) -> &TokenDecl {
        &self.tokens[self.code[c]]
    }

    fn text(&self, c: usize) -> &str {
        &self.tok(c).text
    }

    /// Code position of the token matching the delimiter at code position `c`.
    fn close_of(&self, c: usize) -> Option<usize> {
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

    /// Declarations in code positions `[lo, hi)`.
    fn body(&mut self, lo: usize, hi: usize, level: &Level) {
        let mut c = lo;
        while c < hi {
            match self.text(c) {
                ";" | "}" => {
                    c += 1;
                    continue;
                }
                _ => {}
            }
            let Some((end, next)) = self.header_end(c, hi) else {
                // Unbalanced input: resynchronize one token later.
                c += 1;
                continue;
            };
            let file_scoped =
                matches!(end, End::Semi { .. }) && (c..next).any(|k| self.text(k) == "namespace");
            self.declaration(c, &end, level, hi);
            if file_scoped {
                // Its body is the rest of the range, already scanned.
                return;
            }
            c = next;
        }
    }

    /// Find where the declaration starting at `start` ends. Returns the end
    /// and the code position after it; `None` if input is unbalanced or ends.
    fn header_end(&self, start: usize, hi: usize) -> Option<(End, usize)> {
        let mut c = start;
        let mut operator = false;
        while c < hi {
            match self.text(c) {
                "(" | "[" => c = self.close_of(c)? + 1,
                "operator" => {
                    operator = true;
                    c += 1;
                }
                // `int[] a = { 1 };`, `Action d = delegate { };`: a field
                // initializer, not a body.
                "=" if !operator && !self.is_arrow(c, hi) => {
                    let (at, after) = self.expression_end(c, hi)?;
                    return Some((End::Semi { at, arrow: false }, after));
                }
                "{" => {
                    // An unmatched `{` (e.g. from both `#if` branches being
                    // kept) runs to the end of the enclosing range, so the
                    // declarations after it are still found.
                    let close = match self.close_of(c) {
                        Some(close) if close < hi => close,
                        // `close` is exclusive here (the body is `open+1..hi`);
                        // the span ends at the range's last token.
                        _ => {
                            let end = End::Block {
                                open: c,
                                close: hi,
                                last: hi - 1,
                            };
                            return Some((end, hi));
                        }
                    };
                    // `int P { get; set; } = 1;` keeps its initializer.
                    let (mut last, mut next) = (close, close + 1);
                    if next < hi && self.text(next) == "=" && !self.is_arrow(next, hi) {
                        (last, next) = self.expression_end(next, hi)?;
                    }
                    return Some((
                        End::Block {
                            open: c,
                            close,
                            last,
                        },
                        next,
                    ));
                }
                ";" => {
                    return Some((
                        End::Semi {
                            at: c,
                            arrow: false,
                        },
                        c + 1,
                    ))
                }
                "}" => return None,
                _ if self.is_arrow(c, hi) => {
                    let (at, after) = self.expression_end(c, hi)?;
                    return Some((End::Semi { at, arrow: true }, after));
                }
                _ => c += 1,
            }
        }
        None
    }

    /// From an `=` or `=>` at `c`, the `;` ending the expression (braces,
    /// brackets and parens skipped as groups). Returns (`;` position, next).
    fn expression_end(&self, mut c: usize, hi: usize) -> Option<(usize, usize)> {
        while c < hi {
            match self.text(c) {
                "(" | "[" | "{" => c = self.close_of(c)? + 1,
                ";" => return Some((c, c + 1)),
                "}" => return None,
                _ => c += 1,
            }
        }
        None
    }

    /// Record the declaration `[start, end]` (and recurse into type bodies).
    fn declaration(&mut self, start: usize, end: &End, level: &Level, hi: usize) {
        let last = match *end {
            End::Block { last, .. } => last,
            End::Semi { at, .. } => at,
        };
        // Header: from `start` up to the body/terminator.
        let head_end = match *end {
            End::Block { open, .. } => open,
            End::Semi { at, .. } => at,
        };
        // Skip leading attribute groups `[...]`.
        let mut h = start;
        while h < head_end && self.text(h) == "[" {
            match self.close_of(h) {
                Some(close) if close < head_end => h = close + 1,
                _ => return,
            }
        }
        // Top-level `(` / `=` / `where` bound the part that names the declaration.
        let mut first_paren = None;
        let mut first_eq = None;
        let mut first_where = None;
        let mut first_arrow = None;
        let mut c = h;
        let mut operator = false;
        while c < head_end {
            // Only the part before an expression body `=>` names things.
            if self.is_arrow(c, head_end + 1) {
                first_arrow = Some(c);
                break;
            }
            match self.text(c) {
                "operator" => operator = true,
                "(" | "[" => {
                    // A parameter list follows a name (or the `>` of generic
                    // arguments), not a modifier or nothing (tuple types).
                    let params = self.text(c) == "("
                        && c > h
                        && (self.text(c - 1) == ">"
                            || (self.is_ident(c - 1) && !MODIFIERS.contains(&self.text(c - 1))));
                    if params && first_paren.is_none() && first_eq.is_none() {
                        first_paren = Some(c);
                    }
                    c = match self.close_of(c) {
                        Some(close) => close + 1,
                        None => return,
                    };
                    continue;
                }
                "=" if first_eq.is_none() && !operator => first_eq = Some(c),
                "where" if first_where.is_none() => first_where = Some(c),
                _ => {}
            }
            c += 1;
        }
        let name_end = [
            first_paren,
            first_eq,
            first_where,
            first_arrow,
            Some(head_end),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(head_end);
        let span_end = match *end {
            // A file-scoped namespace runs to the end of its body range.
            End::Semi { at, .. }
                if self.words(h, name_end).any(|w| w == "namespace") && hi > at + 1 =>
            {
                hi - 1
            }
            _ => last,
        };
        let span = span_between(&self.tok(start).span, &self.tok(span_end).span);
        let push = |s: &mut Self, name: String, kind: SymbolKind, lang: &str| {
            s.out.push(SymbolDecl {
                name,
                kind,
                lang_kind: Some(lang.to_string()),
                span,
            });
        };

        // namespace A.B { ... }  /  namespace A.B;
        if let Some(ns) = (h..name_end).find(|&c| self.text(c) == "namespace") {
            let name: String = (ns + 1..name_end).map(|c| self.text(c)).collect();
            if name.is_empty() {
                return;
            }
            push(self, name, SymbolKind::Module, "namespace");
            match *end {
                End::Block { open, close, .. } => self.body(open + 1, close, &Level::Namespace),
                End::Semi { at, .. } => self.body(at + 1, hi, &Level::Namespace),
            }
            return;
        }
        // class / struct / interface / record / enum
        if let Some(kw) = (h..name_end).find(|&c| TYPE_KEYWORDS.contains(&self.text(c))) {
            let Some(name) = (kw + 1..name_end)
                .find(|&c| !matches!(self.text(c), "class" | "struct"))
                .filter(|&c| self.is_ident(c))
            else {
                return;
            };
            let kind = self.text(kw).to_string();
            let name = self.text(name).to_string();
            push(self, name.clone(), SymbolKind::Type, &kind);
            if let End::Block { open, close, .. } = *end {
                if kind != "enum" {
                    self.body(open + 1, close, &Level::Type(name));
                }
            }
            return;
        }
        // delegate R Name<T>(...);
        if self.words(h, name_end).any(|w| w == "delegate") {
            if let Some(n) = first_paren.and_then(|p| self.name_before(p, h)) {
                push(self, self.text(n).to_string(), SymbolKind::Type, "delegate");
            }
            return;
        }
        let Level::Type(owner) = level else {
            return;
        };
        // Type members.
        let is_event = self.words(h, name_end).any(|w| w == "event");
        if let Some(op) = (h..name_end).find(|&c| self.text(c) == "operator") {
            // `operator ==`, `operator int`: adjacent tokens join without a space.
            let mut name = String::from("operator");
            for c in (op + 1..name_end).take_while(|&c| self.text(c) != "(") {
                if self.tok(c - 1).span.end != self.tok(c).span.start || c == op + 1 {
                    name.push(' ');
                }
                name.push_str(self.text(c));
            }
            push(self, name, SymbolKind::Method, "operator");
        } else if (h..name_end)
            .any(|c| self.text(c) == "this" && c + 1 < head_end && self.text(c + 1) == "[")
        {
            push(self, "this".into(), SymbolKind::Variable, "indexer");
        } else if let (Some(p), false) = (first_paren, is_event) {
            let Some(n) = self.name_before(p, h) else {
                return;
            };
            let finalizer = n > h && self.text(n - 1) == "~";
            let ctor = owner == self.text(n);
            let lang = if finalizer {
                "finalizer"
            } else if ctor {
                "constructor"
            } else {
                "method"
            };
            push(self, self.text(n).to_string(), SymbolKind::Method, lang);
        } else {
            let Some(n) = self.name_before(name_end, h) else {
                return;
            };
            let name = self.text(n).to_string();
            let (kind, lang) = match *end {
                _ if is_event => (SymbolKind::Variable, "event"),
                End::Block { .. } => (SymbolKind::Variable, "property"),
                End::Semi { arrow: true, .. } if first_eq.is_none() => {
                    (SymbolKind::Variable, "property")
                }
                End::Semi { .. } if self.words(h, name_end).any(|w| w == "const") => {
                    (SymbolKind::Constant, "const")
                }
                End::Semi { .. } => (SymbolKind::Variable, "field"),
            };
            push(self, name, kind, lang);
        }
    }

    fn words(&self, lo: usize, hi: usize) -> impl Iterator<Item = &str> + '_ {
        (lo..hi).map(move |c| self.text(c))
    }

    fn is_ident(&self, c: usize) -> bool {
        let t = self.tok(c);
        t.class == TokenClass::Identifier
    }

    /// The identifier naming a declaration whose name ends before code
    /// position `end`: skips a trailing generic argument list `<...>`.
    fn name_before(&self, end: usize, lo: usize) -> Option<usize> {
        let mut c = end.checked_sub(1)?;
        if c < lo {
            return None;
        }
        if self.text(c) == ">" {
            let mut depth = 0usize;
            loop {
                match self.text(c) {
                    ">" => depth += 1,
                    "<" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if c == lo {
                    return None;
                }
                c -= 1;
            }
            c = c.checked_sub(1)?;
            if c < lo {
                return None;
            }
        }
        self.is_ident(c).then_some(c)
    }
}

#[cfg(test)]
mod tests;
