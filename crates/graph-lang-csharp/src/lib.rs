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
//! | enum members (`A`, `B = 1 << 2`) | Constant | `enum_member` |
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
//! ends the literal early (a tokenizer limit). Enum members: a comma inside
//! `<...>` in an initializer splits the member unless the `<...>` reads as
//! generic arguments made only of type words (`Gen<int, long>.Max` is
//! fine); an unbalanced bracket in an enum body drops the members after it;
//! since both `#if`/`#else` branches are kept, a member declared in each
//! gives two symbols with the same name.
use graph_core::scan::{code_close_table, mark_keywords, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// C#'s reserved keywords. Contextual keywords (`var`, `async`, `await`,
/// `get`, `set`, `record`, `where`, ...) are also valid identifiers and stay
/// identifiers. Public for other extractors' tests; the list may grow (and
/// any change needs this crate's and `graph-lang-aspx`'s versions bumped).
pub const KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "base",
    "bool",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "checked",
    "class",
    "const",
    "continue",
    "decimal",
    "default",
    "delegate",
    "do",
    "double",
    "else",
    "enum",
    "event",
    "explicit",
    "extern",
    "false",
    "finally",
    "fixed",
    "float",
    "for",
    "foreach",
    "goto",
    "if",
    "implicit",
    "in",
    "int",
    "interface",
    "internal",
    "is",
    "lock",
    "long",
    "namespace",
    "new",
    "null",
    "object",
    "operator",
    "out",
    "override",
    "params",
    "private",
    "protected",
    "public",
    "readonly",
    "ref",
    "return",
    "sbyte",
    "sealed",
    "short",
    "sizeof",
    "stackalloc",
    "static",
    "string",
    "struct",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "uint",
    "ulong",
    "unchecked",
    "unsafe",
    "ushort",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

/// `(c, hi)` -> `expression_end` from `c`.
type ExprMemo = RefCell<HashMap<(usize, usize), Option<(usize, usize)>>>;

pub struct CSharpExtractor;

/// Tokenizer dialect used for C#.
pub const CSHARP_TOKENIZER: TokenizerOptions = TokenizerOptions {
    csharp_strings: true,
    ..TokenizerOptions::DEFAULT
};

impl Extractor for CSharpExtractor {
    fn language(&self) -> &str {
        "csharp"
    }

    fn extensions(&self) -> &[&str] {
        &["cs", "csx"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        // `em1`: enum members are symbols (#269).
        format!("csharp-scan-1+kw1+em1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, CSHARP_TOKENIZER);
        let symbols = symbols(&tokens);
        // After the symbol scan, which reads identifiers as it always has.
        mark_csharp_keywords(&mut tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Relabels C#'s reserved words in `tokens` (as produced with
/// [`CSHARP_TOKENIZER`]) as `keyword`. Exposed so other extractors (server
/// code embedded in markup) class embedded C# the same way. Call it after
/// any symbol scan, which reads identifiers.
///
/// `graph-lang-aspx` calls this on server script bodies: a change here
/// changes ASPX tokens too, so it needs an aspx version bump as well.
pub fn mark_csharp_keywords(tokens: &mut [TokenDecl]) {
    // `@class` is a verbatim identifier: `@` right before the word.
    mark_keywords(tokens, KEYWORDS, |t, i| {
        i > 0 && t[i - 1].text == "@" && t[i - 1].span.end == t[i].span.start
    });
}

/// Symbols in C# tokens (as produced with [`CSHARP_TOKENIZER`]). Exposed so
/// other extractors (e.g. for server-side code embedded in markup) can reuse
/// the scanner on a token slice.
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_indices(tokens);
    let mut s = Scanner {
        tokens,
        code: &code,
        closes: code_close_table(tokens, &code),
        out: Vec::new(),
        depth: 0,
        header_fails: RefCell::default(),
        expr_ends: RefCell::default(),
    };
    s.body(0, code.len(), &Level::Namespace);
    s.out
}

/// Like [`symbols`], but `tokens` are the members of a type body (fields,
/// properties, methods, nested types): what an ASP.NET
/// `<script runat="server">` block holds, which the page class wraps.
/// Nothing is reported as a constructor (the enclosing type is unnamed).
pub fn member_symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_indices(tokens);
    let mut s = Scanner {
        tokens,
        code: &code,
        closes: code_close_table(tokens, &code),
        out: Vec::new(),
        depth: 0,
        header_fails: RefCell::default(),
        expr_ends: RefCell::default(),
    };
    s.body(0, code.len(), &Level::Type(String::new()));
    s.out
}

/// Built-in type keywords that can start a local declaration.
const BUILTIN_TYPES: &[&str] = &[
    "bool", "byte", "char", "decimal", "double", "float", "int", "long", "object", "sbyte",
    "short", "string", "uint", "ulong", "ushort",
];

/// Longest local declaration statement, in code tokens, whose `;` is found;
/// a longer one's span ends at its name.
const MAX_STATEMENT: usize = 4096;

/// Contextual words that start a statement, not a type (`await x;`).
const NOT_TYPES: &[&str] = &["await", "yield", "nameof", "when", "goto"];

/// Local declarations in a run of C# statements (as produced with
/// [`CSHARP_TOKENIZER`]), which may be a fragment such as the body of an
/// ASP.NET `<% ... %>` code block (`if (x) {`, `int n = 0;`, `}`). Not used
/// by [`CSharpExtractor`] (method bodies are not scanned there), so a change
/// here needs only `graph-lang-aspx`'s version bumped.
///
/// | C# | `SymbolKind` | `lang_kind` | span |
/// |---|---|---|---|
/// | `T x = ...;`, `var x;`, `T x, y;` | Variable | `local` | type through `;` |
/// | `const T x = ...;` | Constant | `local_const` | `const` through `;` |
/// | `for (T x = ...`, `using (T x = ...`, `foreach (T x in` | Variable | `local` | type through name |
/// | `catch (T x)` | Variable | `local` | type through name |
///
/// A declaration is a type (a name or built-in type keyword, optionally
/// qualified, generic, nullable or an array) then an identifier then `=`,
/// `;` or `,` (`in` for `foreach`, `)` for `catch`), at a statement start:
/// the first token, or after `;`, `{` or `}`. A multi-declarator statement
/// yields one symbol, named by its first declarator. Not found: tuple
/// deconstruction, pattern and `out var` variables, local functions.
pub fn local_symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_indices(tokens);
    let t = |c: usize| tokens[code[c]].text.as_str();
    let n = code.len();
    let mut out = Vec::new();
    for c in 0..n {
        let stmt_start = c == 0 || matches!(t(c - 1), ";" | "{" | "}");
        // `for (`, `foreach (`, `using (`, `catch (`: the header's first word.
        let header = (c >= 2 && t(c - 1) == "(")
            .then(|| t(c - 2))
            .filter(|w| matches!(*w, "for" | "foreach" | "using" | "catch"));
        if !stmt_start && header.is_none() {
            continue;
        }
        let constant = stmt_start && t(c) == "const";
        let ty = if constant { c + 1 } else { c };
        let Some(name) = type_end(tokens, &code, ty) else {
            continue;
        };
        if name >= n || !is_name(&tokens[code[name]]) {
            continue;
        }
        let after = if name + 1 < n { t(name + 1) } else { "" };
        let ok = match header {
            Some("foreach") => after == "in",
            Some("catch") => after == ")",
            Some(_) => after == "=",
            None => matches!(after, "=" | ";" | ","),
        };
        if !ok {
            continue;
        }
        let last = if header.is_none() {
            statement_end(tokens, &code, name + 1).unwrap_or(name)
        } else {
            name
        };
        out.push(SymbolDecl {
            owner: None,
            name: t(name).to_string(),
            kind: if constant {
                SymbolKind::Constant
            } else {
                SymbolKind::Variable
            },
            lang_kind: Some(if constant { "local_const" } else { "local" }.into()),
            span: span_between(&tokens[code[c]].span, &tokens[code[last]].span),
        });
    }
    out
}

/// A non-reserved identifier (`int x` is not read as `return x`).
fn is_name(t: &TokenDecl) -> bool {
    t.class == TokenClass::Identifier && !KEYWORDS.contains(&t.text.as_str())
}

fn is_type_word(t: &TokenDecl) -> bool {
    BUILTIN_TYPES.contains(&t.text.as_str())
        || (is_name(t) && !NOT_TYPES.contains(&t.text.as_str()))
}

/// Code position just past a local's type starting at `c`, or `None` when
/// no type starts there.
fn type_end(tokens: &[TokenDecl], code: &[usize], c: usize) -> Option<usize> {
    let n = code.len();
    let tk = |c: usize| &tokens[code[c]];
    let simple = |c: usize| c < n && is_type_word(tk(c));
    if !simple(c) {
        return None;
    }
    let mut c = c + 1;
    while c + 1 < n && tk(c).text == "." && simple(c + 1) {
        c += 2;
    }
    if c < n && tk(c).text == "<" {
        // Generic arguments: names, built-ins, `.`, `,`, `?`, `[]`, nested.
        let mut depth = 0i32;
        while c < n {
            match tk(c).text.as_str() {
                "<" => depth += 1,
                // The tokenizer emits `>>` and `>>>` as single `>`s.
                ">" => depth -= 1,
                "." | "," | "?" | "[" | "]" => {}
                _ if simple(c) => {}
                _ => return None,
            }
            c += 1;
            if depth <= 0 {
                break;
            }
        }
        if depth != 0 {
            return None;
        }
    }
    if c < n && tk(c).text == "?" {
        c += 1;
    }
    while c + 1 < n && tk(c).text == "[" {
        let mut j = c + 1;
        while j < n && tk(j).text == "," {
            j += 1;
        }
        if j < n && tk(j).text == "]" {
            c = j + 1;
        } else {
            break;
        }
    }
    Some(c)
}

/// Code position of the `;` ending the statement from `c` (brackets
/// balanced), or `None` when the tokens end first or it is more than
/// [`MAX_STATEMENT`] tokens away (bounding the scan on adversarial input).
fn statement_end(tokens: &[TokenDecl], code: &[usize], mut c: usize) -> Option<usize> {
    let mut depth = 0i32;
    let hi = code.len().min(c.saturating_add(MAX_STATEMENT));
    while c < hi {
        match tokens[code[c]].text.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            ";" if depth == 0 => return Some(c),
            _ => {}
        }
        c += 1;
    }
    None
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
    /// [`code_close_table`] of `code`: closers found in one linear pass.
    closes: Vec<Option<usize>>,
    out: Vec<SymbolDecl>,
    /// Nesting of `body` calls, capped at [`MAX_DEPTH`].
    depth: usize,
    /// `(c, hi, operator)` states from which [`Scanner::header_end`] fails.
    header_fails: RefCell<HashSet<(usize, usize, bool)>>,
    /// `(c, hi)` -> [`Scanner::expression_end`] from `c`.
    expr_ends: ExprMemo,
}

/// Deepest nesting of namespace and type bodies scanned. Deeper
/// declarations are not reported, their enclosing ones are: this bounds the
/// recursion so adversarial input (thousands of nested `namespace a {`)
/// cannot overflow the indexer thread's stack.
pub const MAX_DEPTH: usize = 256;

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
        self.closes[c]
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
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        self.body_level(lo, hi, level);
        self.depth -= 1;
    }

    fn body_level(&mut self, lo: usize, hi: usize, level: &Level) {
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
    ///
    /// The scan depends only on the position, `hi` and whether
    /// it saw `operator`, so a failed scan records the states it passed and
    /// a later scan reaching one fails at once: the caller's
    /// resynchronize-one-token-later loop stays linear on long runs with no
    /// terminator such as `<div>` or `"` (#146).
    fn header_end(&self, start: usize, hi: usize) -> Option<(End, usize)> {
        let mut visited = Vec::new();
        let r = self.header_end_scan(start, hi, &mut visited);
        if r.is_none() {
            self.header_fails
                .borrow_mut()
                .extend(visited.into_iter().map(|(c, op)| (c, hi, op)));
        }
        r
    }

    fn header_end_scan(
        &self,
        start: usize,
        hi: usize,
        visited: &mut Vec<(usize, bool)>,
    ) -> Option<(End, usize)> {
        let mut c = start;
        let mut operator = false;
        while c < hi {
            // Nothing depends on `start` itself, so its state is recorded
            // and checked like any other (as in Java).
            if memo_on() && self.header_fails.borrow().contains(&(c, hi, operator)) {
                return None;
            }
            visited.push((c, operator));
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
    /// Memoized by position (every position the scan passes has the same
    /// answer), so repeated calls over one unterminated run stay linear.
    fn expression_end(&self, c: usize, hi: usize) -> Option<(usize, usize)> {
        let mut visited = Vec::new();
        let r = self.expression_end_scan(c, hi, &mut visited);
        let mut memo = self.expr_ends.borrow_mut();
        for v in visited {
            memo.insert((v, hi), r);
        }
        r
    }

    fn expression_end_scan(
        &self,
        mut c: usize,
        hi: usize,
        visited: &mut Vec<usize>,
    ) -> Option<(usize, usize)> {
        while c < hi {
            if let Some(&r) = self.expr_ends.borrow().get(&(c, hi)).filter(|_| memo_on()) {
                return r;
            }
            visited.push(c);
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
                owner: None,
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
                if kind == "enum" {
                    self.enum_members(open + 1, close);
                } else {
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

    /// Members of an enum body in code positions `[lo, hi)`: comma-separated
    /// `[Attr] Name = value` entries. Each span runs from its first token
    /// (attributes included) through the end of its initializer. The name is
    /// the first identifier before any `=` (an entry with none, such as
    /// `= 1`, is not a symbol). In an initializer, a generic type
    /// (`Gen<int, long>.Max`) is skipped whole so its commas do not split
    /// the member; an unbalanced bracket ends the scan (later members of
    /// that enum are dropped).
    fn enum_members(&mut self, lo: usize, hi: usize) {
        let mut c = lo;
        while c < hi {
            let start = c;
            let mut name = None;
            let mut in_initializer = false;
            while c < hi && self.text(c) != "," {
                match self.text(c) {
                    "(" | "[" | "{" => match self.close_of(c) {
                        Some(close) if close < hi => {
                            c = close + 1;
                            continue;
                        }
                        _ => return,
                    },
                    "=" => in_initializer = true,
                    _ if in_initializer && self.is_ident(c) => {
                        // `type_end` accepts `<...>` only when it holds
                        // nothing but type words, `.`, `,`, `?` and `[]`.
                        match type_end(self.tokens, self.code, c) {
                            Some(end) if end > c + 1 && end <= hi => {
                                c = end;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    _ if name.is_none() && self.is_ident(c) => name = Some(c),
                    _ => {}
                }
                c += 1;
            }
            // A name was seen, so the entry is non-empty (`c > start`).
            if let Some(n) = name {
                self.out.push(SymbolDecl {
                    owner: None,
                    name: self.text(n).to_string(),
                    kind: SymbolKind::Constant,
                    lang_kind: Some("enum_member".into()),
                    span: span_between(&self.tok(start).span, &self.tok(c - 1).span),
                });
            }
            c += 1;
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

/// Whether the scan memos are consulted. Tests switch them off to compare
/// against the plain forward scans; always on outside tests.
fn memo_on() -> bool {
    #[cfg(test)]
    return tests::MEMO.with(std::cell::Cell::get);
    #[cfg(not(test))]
    true
}
