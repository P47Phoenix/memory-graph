//! C and C++ extractors: token-stream scanners, not parsers.
//!
//! Two extractors share one scanner: [`CExtractor`] (language `c`, files
//! `.c` and `.h`) and [`CppExtractor`] (language `cpp`, files `.cpp`, `.cc`,
//! `.cxx`, `.hpp`, `.hh`, `.hxx`, `.ipp`). A `.h` file is always C. Symbols
//! are found by walking declarations at file, namespace and class level:
//!
//! | C / C++ | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `#define` (object- or function-like) | Other | `macro` |
//! | `struct` `union` `enum` (and C++ `class`) with a body | Type | the keyword |
//! | `typedef`, C++ `using X = ...;` | Type | `typedef` / `using` |
//! | function definition | Function | `function` (`operator` for a free operator) |
//! | global variable / `const`/`constexpr` global | Variable / Constant | `variable` / `const` |
//! | C++ `namespace` (named) | Module | `namespace` |
//! | C++ member function (inline, declared, or out of class `A::f`) | Method | `method` `constructor` `destructor` `operator` |
//! | C++ data member | Variable / Constant | `field` / `const` |
//!
//! A declaration's span runs from its first token (`template <...>` and
//! `[[attributes]]` included) through its closing `}` or `;`. Function
//! bodies are not scanned. Function prototypes at file level are not
//! symbols (inside a C++ class they are methods). C struct fields are not
//! symbols; nested types in a C struct are. Odd input never sets
//! `has_errors`.
//!
//! Known limits: preprocessor lines other than `#define` are dropped and
//! only the first branch of each `#if`/`#ifdef`/`#ifndef` group is scanned
//! (`#define`s are found in every branch), so code only in an `#else` is
//! not seen. A macro invocation without a trailing `;` merges with the next
//! declaration, unless it is an all-caps macro alone on its line. Variables
//! declared after a type body (`struct S { .. } s;`) and all but the first
//! declarator of `int a, b;` are not symbols. Names never carry a
//! qualifier: an out-of-line `A::B::f` is named `f` with owner `B`, and a
//! definition qualified by a namespace (`ns::f() {}`) is reported as a
//! method of `ns`. A destructor is named `~Box`. `extern` declarations and
//! K&R-style parameter declarations are skipped. Bodies nested deeper than
//! 64 levels are not scanned.
//!
//! A `.h` file is C unless it uses C++-only syntax (`class X {`,
//! `namespace X {`, `template <`, `public:`); then it is scanned with the
//! C++ rules but keeps the language `c`.
use graph_core::scan::{mark_keywords, span_between, NestedEnds, Step};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// `(c, hi)` -> `expression_end` from `c`.
type ExprMemo = RefCell<HashMap<(usize, usize), Option<(usize, usize)>>>;

/// The C extractor (`.c`, `.h`).
pub struct CExtractor;
/// The C++ extractor (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.ipp`).
pub struct CppExtractor;

impl Extractor for CExtractor {
    fn language(&self) -> &str {
        "c"
    }

    fn extensions(&self) -> &[&str] {
        &["c", "h"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("c-scan-1+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        extract(source, TokenizerOptions::C, false, true)
    }
}

impl Extractor for CppExtractor {
    fn language(&self) -> &str {
        "cpp"
    }

    fn extensions(&self) -> &[&str] {
        &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "ipp"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("cpp-scan-1+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        extract(source, TokenizerOptions::CPP, true, false)
    }
}

/// C keywords (C11 and C23).
const C_KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_Bool",
    "_Complex",
    "_Generic",
    "_Imaginary",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "alignas",
    "alignof",
    "bool",
    "constexpr",
    "false",
    "nullptr",
    "static_assert",
    "thread_local",
    "true",
    "typeof",
    "typeof_unqual",
];

/// C++20 keywords and alternative operator tokens. Contextual words
/// (`override`, `final`, `import`, `module`) stay identifiers.
const CPP_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "and",
    "and_eq",
    "asm",
    "auto",
    "bitand",
    "bitor",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "char8_t",
    "char16_t",
    "char32_t",
    "class",
    "compl",
    "concept",
    "const",
    "consteval",
    "constexpr",
    "constinit",
    "const_cast",
    "continue",
    "co_await",
    "co_return",
    "co_yield",
    "decltype",
    "default",
    "delete",
    "do",
    "double",
    "dynamic_cast",
    "else",
    "enum",
    "explicit",
    "export",
    "extern",
    "false",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "not",
    "not_eq",
    "nullptr",
    "operator",
    "or",
    "or_eq",
    "private",
    "protected",
    "public",
    "register",
    "reinterpret_cast",
    "requires",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "static_cast",
    "struct",
    "switch",
    "template",
    "this",
    "thread_local",
    "throw",
    "true",
    "try",
    "typedef",
    "typeid",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "wchar_t",
    "while",
    "xor",
    "xor_eq",
];

/// Nesting depth past which bodies are not scanned (deeper symbols are
/// dropped), so that pathological input cannot overflow the stack.
const MAX_DEPTH: usize = 64;

fn extract(source: &str, dialect: TokenizerOptions, cpp: bool, sniff: bool) -> Extraction {
    let mut tokens = tokenize_with(source, dialect);
    let (code, macros) = preprocess(&tokens);
    // A C header (`.h`) that uses C++-only constructs is scanned as C++; its
    // language stays `c`.
    let cpp = cpp || (sniff && looks_like_cpp(&tokens, &code));
    let pairs = match_pairs(&tokens, &code);
    let mut s = Scanner {
        tokens: &tokens,
        code: &code,
        pairs: &pairs,
        out: macros,
        cpp,
        depth: 0,
        memo: Memo::default(),
    };
    s.body(0, code.len(), &Level::File);
    let mut symbols = s.out;
    symbols.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
    // After the symbol scan. A header sniffed as C++ gets the C++ list.
    // Directive names (`#if`, `#else`) stay identifiers, like `#define`.
    let words = if cpp { CPP_KEYWORDS } else { C_KEYWORDS };
    mark_keywords(&mut tokens, words, |t, i| {
        t[..i]
            .iter()
            .rev()
            .find(|t| t.class != TokenClass::Comment)
            .is_some_and(|p| p.text == "#")
    });
    Extraction {
        symbols,
        tokens,
        has_errors: false,
    }
}

/// Matching `(`/`[`/`{` closers over the code stream only (so tokens in
/// dropped preprocessor branches do not count), ignoring literals. A closer
/// that does not match the innermost opener is ignored.
fn match_pairs(tokens: &[TokenDecl], code: &[usize]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; code.len()];
    let mut stack: Vec<(usize, &str)> = Vec::new();
    for (c, &i) in code.iter().enumerate() {
        let t = &tokens[i];
        if t.class == TokenClass::Literal {
            continue;
        }
        let text = t.text.as_str();
        match text {
            "(" => stack.push((c, ")")),
            "[" => stack.push((c, "]")),
            "{" => stack.push((c, "}")),
            ")" | "]" | "}" if stack.last().is_some_and(|&(_, want)| want == text) => {
                if let Some((open, _)) = stack.pop() {
                    pairs[open] = Some(c);
                }
            }
            _ => {}
        }
    }
    pairs
}

/// Whether code tokens use C++-only syntax: `class Name {`/`:`,
/// `namespace Name {`, `template <`, an access specifier `public:`, or `::`.
fn looks_like_cpp(tokens: &[TokenDecl], code: &[usize]) -> bool {
    let text = |k: usize| code.get(k).map_or("", |&i| tokens[i].text.as_str());
    let ident = |k: usize| {
        code.get(k)
            .is_some_and(|&i| tokens[i].class == TokenClass::Identifier)
    };
    (0..code.len()).any(|k| match text(k) {
        "class" | "namespace" => ident(k + 1) && matches!(text(k + 2), "{" | ":"),
        "template" => text(k + 1) == "<",
        "public" | "private" | "protected" => text(k + 1) == ":" && text(k + 2) != ":",
        _ => false,
    })
}

/// Code token indices (no comments, no preprocessor lines, only the first
/// branch of each `#if`/`#ifdef`/`#ifndef` group) and the `#define` macros
/// found on the way (from every branch).
fn preprocess(tokens: &[TokenDecl]) -> (Vec<usize>, Vec<SymbolDecl>) {
    let mut code = Vec::with_capacity(tokens.len());
    let mut macros = Vec::new();
    // One entry per open `#if` group: whether a later branch is being skipped.
    let mut groups: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.class == TokenClass::Comment {
            i += 1;
            continue;
        }
        let first_on_line = i == 0 || tokens[i - 1].span.end_line < t.span.start_line;
        if t.text != "#" || !first_on_line {
            if !groups.contains(&true) {
                code.push(i);
            }
            i += 1;
            continue;
        }
        // A directive runs to the end of its line, continued by a `\`.
        let mut j = i;
        loop {
            let mut line = tokens[j].span.end_line;
            if tokens[j].text == "\\" {
                line += 1;
            }
            if j + 1 < tokens.len() && tokens[j + 1].span.start_line <= line {
                j += 1;
            } else {
                break;
            }
        }
        let words: Vec<usize> = (i + 1..=j)
            .filter(|&k| tokens[k].class != TokenClass::Comment)
            .collect();
        match words.first().map(|&d| tokens[d].text.as_str()) {
            Some("if" | "ifdef" | "ifndef") => groups.push(false),
            Some("elif" | "else" | "elifdef" | "elifndef") => {
                if let Some(g) = groups.last_mut() {
                    *g = true;
                }
            }
            Some("endif") => {
                groups.pop();
            }
            _ => {}
        }
        if let [d, n, ..] = words[..] {
            if tokens[d].text == "define" && tokens[n].class == TokenClass::Identifier {
                let last = *words.last().unwrap_or(&n);
                macros.push(SymbolDecl {
                    owner: None,
                    name: tokens[n].text.clone(),
                    kind: SymbolKind::Other,
                    lang_kind: Some("macro".into()),
                    span: span_between(&t.span, &tokens[last].span),
                });
            }
        }
        i = j + 1;
    }
    (code, macros)
}

enum Level {
    /// File, namespace or `extern "C"` body.
    File,
    /// Body of the named struct/class/union.
    Class(String),
}

const TYPE_KEYWORDS: &[&str] = &["struct", "class", "union", "enum"];
const ACCESS: &[&str] = &[
    "public",
    "private",
    "protected",
    "signals",
    "slots",
    "Q_SIGNALS",
    "Q_SLOTS",
];
/// Words whose `(` is not a parameter list.
const NOT_CALLS: &[&str] = &[
    "__attribute__",
    "__declspec",
    "alignas",
    "_Alignas",
    "decltype",
    "noexcept",
    "throw",
    "requires",
    "__asm__",
    "__asm",
    "asm",
    "sizeof",
    "alignof",
    "_Alignof",
    "typeof",
    "__typeof__",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    /// Indices into `tokens` of code tokens.
    code: &'a [usize],
    out: Vec<SymbolDecl>,
    cpp: bool,
    /// For each code position, the code position of its matching closer.
    pairs: &'a [Option<usize>],
    /// Current body nesting depth (see [`MAX_DEPTH`]).
    depth: usize,
    /// Memo of the forward scans, so that resynchronizing one token at a
    /// time over input with no terminator stays linear (#146).
    memo: Memo,
}

/// Results of forward scans that depend only on where they are (not on
/// where they started), keyed by position and the range end `hi`.
#[derive(Default)]
struct Memo {
    /// `(c, hi, flags)`: `header_end`, reaching `c` with these flags, fails.
    header_fails: RefCell<HashSet<(usize, usize, u8)>>,
    /// `(c, hi)` -> [`Scanner::expression_end`] from `c`.
    expr: ExprMemo,
    /// `(c, hi)` -> [`Scanner::semi_after_block`] from `c`.
    semi: RefCell<HashMap<(usize, usize), Option<usize>>>,
    /// `(open, hi)` -> [`Scanner::angle_end`].
    angle: NestedEnds,
}

/// How a declaration header ended.
enum End {
    /// `{` at `open`, closed at `close` (`hi` if unmatched); `last` is the
    /// last token of the declaration (`close`, or a trailing `;`).
    Block {
        open: usize,
        close: usize,
        last: usize,
    },
    /// `;` at `at`.
    Semi { at: usize },
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

    /// An all-caps identifier such as `MY_MACRO` (at least two characters).
    fn is_caps(&self, c: usize) -> bool {
        let t = self.text(c);
        self.is_ident(c)
            && t.len() > 1
            && t.chars().any(|ch| ch.is_ascii_uppercase())
            && t.chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    }

    /// Whether the token at `c` starts a line.
    fn new_line(&self, c: usize) -> bool {
        c == 0 || self.tok(c).span.start_line > self.tok(c - 1).span.end_line
    }

    /// Code position of the token matching the delimiter at `c`.
    fn close_of(&self, c: usize) -> Option<usize> {
        self.pairs[c]
    }

    /// `::` made of the tokens at `c` and `c + 1`.
    fn is_scope(&self, c: usize, hi: usize) -> bool {
        c + 1 < hi
            && self.text(c) == ":"
            && self.text(c + 1) == ":"
            && self.tok(c).span.end == self.tok(c + 1).span.start
    }

    /// A `<` at `c` that opens a template argument list (C++ only).
    fn opens_angle(&self, c: usize, lo: usize) -> bool {
        self.cpp
            && self.text(c) == "<"
            && c > lo
            && (self.is_ident(c - 1) && self.text(c - 1) != "operator")
    }

    /// The `>` closing the `<` at `open`, skipping bracket groups; `None`
    /// on `;`, `{`, `}` or the end of the range.
    ///
    /// Memoized ([`NestedEnds`]), so looking up every `<` of a long
    /// unclosed run (`template <` x 100k) stays linear.
    fn angle_end(&self, open: usize, hi: usize) -> Option<usize> {
        self.memo.angle.find(open, hi, |c| match self.text(c) {
            "<" => Step::Open,
            ">" => Step::Close,
            "(" | "[" => Step::Skip(self.close_of(c).filter(|&x| x < hi)),
            ";" | "{" | "}" => Step::Stop,
            _ => Step::Other,
        })
    }

    /// Declarations in code positions `[lo, hi)`.
    fn body(&mut self, lo: usize, hi: usize, level: &Level) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        self.body_inner(lo, hi, level);
        self.depth -= 1;
    }

    fn body_inner(&mut self, lo: usize, hi: usize, level: &Level) {
        let mut c = lo;
        while c < hi {
            match self.text(c) {
                ";" | "}" | ")" | "]" => {
                    c += 1;
                    continue;
                }
                a if self.cpp && matches!(level, Level::Class(_)) && ACCESS.contains(&a) => {
                    while c < hi && ACCESS.contains(&self.text(c)) {
                        c += 1;
                    }
                    if c < hi && self.text(c) == ":" && !self.is_scope(c, hi) {
                        c += 1;
                    }
                    continue;
                }
                // A macro invocation alone on its line (`MACRO(args)` or a
                // bare `MACRO` followed by another macro), as used for
                // pragmas and export markers: not part of the next
                // declaration.
                _ if self.is_caps(c) => {
                    let after = if c + 1 < hi && self.text(c + 1) == "(" {
                        self.close_of(c + 1).filter(|&x| x < hi).map(|x| x + 1)
                    } else if c + 1 < hi && self.is_caps(c + 1) {
                        Some(c + 1)
                    } else {
                        None
                    };
                    if let Some(a) = after {
                        let own_line = a >= hi
                            || (self.tok(a).span.start_line > self.tok(a - 1).span.end_line
                                && self.text(a) != "{");
                        if own_line && (c == lo || self.new_line(c)) {
                            c = a;
                            continue;
                        }
                    }
                }
                // extern "C" { ... }: transparent.
                "extern"
                    if c + 2 < hi
                        && self.tok(c + 1).class == TokenClass::Literal
                        && self.text(c + 2) == "{" =>
                {
                    let close = self.close_of(c + 2).filter(|&x| x < hi).unwrap_or(hi);
                    self.body(c + 3, close, level);
                    c = close + 1;
                    continue;
                }
                _ => {}
            }
            let Some((end, next)) = self.header_end(c, hi) else {
                // Unbalanced input: resynchronize one token later.
                c += 1;
                continue;
            };
            self.declaration(c, &end, level);
            c = next;
        }
    }

    /// Find where the declaration starting at `start` ends. Returns the end
    /// and the code position after it; `None` if input is unbalanced or ends.
    ///
    /// Past `start`, the scan depends only on the position, `hi` and its
    /// four flags, so a failed scan records every state it passed through
    /// and a later scan reaching one of them fails at once: the caller's
    /// resynchronize-one-token-later loop stays linear on input with no
    /// terminator (#146).
    fn header_end(&self, start: usize, hi: usize) -> Option<(End, usize)> {
        let mut visited = Vec::new();
        let r = self.header_end_scan(start, hi, &mut visited);
        if r.is_none() {
            self.memo
                .header_fails
                .borrow_mut()
                .extend(visited.into_iter().map(|(c, f)| (c, hi, f)));
        }
        r
    }

    fn header_end_scan(
        &self,
        start: usize,
        hi: usize,
        visited: &mut Vec<(usize, u8)>,
    ) -> Option<(End, usize)> {
        let mut c = start;
        let mut operator = false;
        let mut params = false;
        let mut init_list = false;
        let mut namespace = false;
        while c < hi {
            if c > start {
                let flags = u8::from(operator)
                    | u8::from(params) << 1
                    | u8::from(init_list) << 2
                    | u8::from(namespace) << 3;
                if memo_on() && self.memo.header_fails.borrow().contains(&(c, hi, flags)) {
                    return None;
                }
                visited.push((c, flags));
            }
            match self.text(c) {
                "template" if c + 1 < hi && self.text(c + 1) == "<" => {
                    c = self.angle_end(c + 1, hi)? + 1;
                }
                "<" if !operator && self.opens_angle(c, start) => match self.angle_end(c, hi) {
                    Some(close) => c = close + 1,
                    None => c += 1,
                },
                "(" | "[" => {
                    if self.text(c) == "(" && (operator || self.is_param_paren(c, start)) {
                        params = true;
                    }
                    c = self.close_of(c)? + 1;
                }
                "operator" => {
                    operator = true;
                    c += 1;
                }
                "namespace" => {
                    namespace = true;
                    c += 1;
                }
                ":" if self.cpp && params && !self.is_scope(c, hi) => {
                    init_list = true;
                    c += 1;
                }
                ":" if self.is_scope(c, hi) => c += 2,
                "=" if !operator => {
                    let (at, after) = self.expression_end(c, hi)?;
                    return Some((End::Semi { at }, after));
                }
                "{" => {
                    // A brace initializer in a constructor's init list.
                    if init_list && c > start && (self.is_ident(c - 1) || self.text(c - 1) == ">") {
                        c = self.close_of(c)? + 1;
                        continue;
                    }
                    let close = match self.close_of(c) {
                        Some(close) if close < hi => close,
                        // Unmatched: runs to the end of the enclosing range.
                        _ => {
                            let end = End::Block {
                                open: c,
                                close: hi,
                                last: hi - 1,
                            };
                            return Some((end, hi));
                        }
                    };
                    let block = |last| End::Block {
                        open: c,
                        close,
                        last,
                    };
                    if params || namespace {
                        return Some((block(close), close + 1));
                    }
                    // A type body or brace initializer: runs to its `;`.
                    if let Some(k) = self.semi_after_block(close + 1, hi) {
                        return Some((block(k), k + 1));
                    }
                    return Some((block(close), close + 1));
                }
                ";" => return Some((End::Semi { at: c }, c + 1)),
                "}" => return None,
                _ => c += 1,
            }
        }
        None
    }

    /// Whether the `(` at `c` is a parameter list: it follows a name (not
    /// an attribute-like keyword) or the `>` of template arguments.
    fn is_param_paren(&self, c: usize, lo: usize) -> bool {
        c > lo
            && ((self.is_ident(c - 1) && !NOT_CALLS.contains(&self.text(c - 1)))
                || self.text(c - 1) == ">")
    }

    /// From an `=` at `c`, the `;` ending the expression (groups skipped).
    /// Returns (`;` position, next).
    /// Memoized by position (every position the scan passes has the same
    /// answer), so repeated calls over one unterminated run stay linear.
    fn expression_end(&self, c: usize, hi: usize) -> Option<(usize, usize)> {
        let mut visited = Vec::new();
        let r = self.expression_end_scan(c, hi, &mut visited);
        let mut memo = self.memo.expr.borrow_mut();
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
            if let Some(&r) = self.memo.expr.borrow().get(&(c, hi)).filter(|_| memo_on()) {
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

    /// After a type body or brace initializer closing before `c`, the `;`
    /// ending it (groups skipped); `None` at a `}`, an unmatched group or
    /// the end of the range. Memoized like [`Scanner::expression_end`].
    fn semi_after_block(&self, mut c: usize, hi: usize) -> Option<usize> {
        let mut visited = Vec::new();
        let r = loop {
            if c >= hi {
                break None;
            }
            if let Some(&r) = self.memo.semi.borrow().get(&(c, hi)).filter(|_| memo_on()) {
                break r;
            }
            visited.push(c);
            match self.text(c) {
                "(" | "[" | "{" => match self.close_of(c) {
                    Some(x) if x < hi => c = x + 1,
                    _ => break None,
                },
                ";" => break Some(c),
                "}" => break None,
                _ => c += 1,
            }
        };
        let mut memo = self.memo.semi.borrow_mut();
        for v in visited {
            memo.insert((v, hi), r);
        }
        r
    }

    fn words(&self, lo: usize, hi: usize) -> impl Iterator<Item = &str> + '_ {
        (lo..hi).map(move |c| self.text(c))
    }

    fn push(&mut self, name: String, kind: SymbolKind, lang: &str, first: usize, last: usize) {
        let span = span_between(&self.tok(first).span, &self.tok(last).span);
        self.out.push(SymbolDecl {
            owner: None,
            name,
            kind,
            lang_kind: Some(lang.to_string()),
            span,
        });
    }

    /// Record the declaration starting at `start` (and recurse into bodies).
    fn declaration(&mut self, start: usize, end: &End, level: &Level) {
        let (head_end, last) = match *end {
            End::Block { open, last, .. } => (open, last),
            End::Semi { at } => (at, at),
        };
        // Skip `template <...>` prefixes and `[[attributes]]`.
        let mut h = start;
        while h < head_end {
            if self.text(h) == "template" && h + 1 < head_end && self.text(h + 1) == "<" {
                match self.angle_end(h + 1, head_end) {
                    Some(close) => h = close + 1,
                    None => return,
                }
            } else if self.text(h) == "[" {
                match self.close_of(h) {
                    Some(close) if close < head_end => h = close + 1,
                    _ => return,
                }
            } else {
                break;
            }
        }
        if h >= head_end {
            return;
        }
        let in_class = matches!(level, Level::Class(_));

        // namespace A::B { ... }
        if self.cpp && (h..head_end).take(2).any(|c| self.text(c) == "namespace") {
            if let End::Block { open, close, .. } = *end {
                let ns = (h..head_end)
                    .find(|&c| self.text(c) == "namespace")
                    .unwrap_or(h);
                let name: String = self.words(ns + 1, open).collect();
                if !name.is_empty() {
                    self.push(name, SymbolKind::Module, "namespace", start, last);
                }
                self.body(open + 1, close, &Level::File);
            }
            return;
        }
        if self
            .words(h, head_end)
            .any(|w| w == "friend" || w == "static_assert")
        {
            return;
        }
        // using X = ...;
        if self.cpp && self.text(h) == "using" {
            if h + 2 < head_end && self.is_ident(h + 1) && self.text(h + 2) == "=" {
                let name = self.text(h + 1).to_string();
                self.push(name, SymbolKind::Type, "using", start, last);
            }
            return;
        }
        let kw = self.top_level_keyword(h, head_end);
        // typedef ...;
        if self.words(h, head_end).any(|w| w == "typedef") {
            let name = match *end {
                End::Block { close, last, .. } if close < last => {
                    self.declarator_name(close + 1, last, close + 1)
                }
                End::Block { .. } => None,
                End::Semi { at } => self.declarator_name(h, at, h + 1),
            };
            if let End::Block { open, close, .. } = *end {
                if let Some(kw) = kw {
                    self.type_body(kw, open, close, kw, close.min(last));
                }
            }
            if let Some(n) = name {
                let name = self.text(n).to_string();
                self.push(name, SymbolKind::Type, "typedef", start, last);
            }
            return;
        }
        let (paren, eq, op) = self.find_params(h, head_end);
        // struct / class / union / enum with a body.
        if let (End::Block { open, close, .. }, None, Some(kw)) = (end, paren, kw) {
            self.type_body(kw, *open, *close, start, last);
            return;
        }
        if let Some(p) = paren {
            if !self.cpp && in_class {
                return;
            }
            if matches!(end, End::Semi { .. }) && !in_class {
                return; // a prototype
            }
            self.callable(h, p, op, level, start, last);
            return;
        }
        // Variables and fields.
        if !self.cpp && in_class {
            return;
        }
        let words_end = match *end {
            End::Block { open, .. } => open,
            End::Semi { at } => eq.unwrap_or(at),
        };
        if !in_class && self.words(h, words_end).any(|w| w == "extern") {
            return;
        }
        let Some(n) = self.declarator_name(h, words_end, h + 1) else {
            return;
        };
        // `struct S;`, `enum E : int;`: forward declarations.
        if kw.is_some_and(|k| {
            n == k + 1 || (n == k + 2 && matches!(self.text(k + 1), "class" | "struct"))
        }) {
            return;
        }
        let constant = self.is_constant(h, n);
        let (kind, lang) = match (constant, in_class) {
            (true, _) => (SymbolKind::Constant, "const"),
            (false, true) => (SymbolKind::Variable, "field"),
            (false, false) => (SymbolKind::Variable, "variable"),
        };
        let name = self.text(n).to_string();
        self.push(name, kind, lang, start, last);
    }

    /// Whether the declaration head `[lo, n)` before the name `n` makes a
    /// constant: `constexpr`, or a top-level `const` (outside template
    /// arguments) with no pointer or reference declarator.
    fn is_constant(&self, lo: usize, n: usize) -> bool {
        let (mut konst, mut indirect) = (false, false);
        let mut c = lo;
        while c < n {
            match self.text(c) {
                "constexpr" => return true,
                "const" => konst = true,
                "*" | "&" => indirect = true,
                "<" if self.opens_angle(c, lo) => {
                    if let Some(x) = self.angle_end(c, n) {
                        c = x;
                    }
                }
                _ => {}
            }
            c += 1;
        }
        konst && !indirect
    }

    /// The first `struct`/`class`/`union`/`enum` in `[lo, hi)` outside
    /// bracket groups.
    fn top_level_keyword(&self, lo: usize, hi: usize) -> Option<usize> {
        let mut c = lo;
        while c < hi {
            match self.text(c) {
                "(" | "[" => c = self.close_of(c).filter(|&x| x < hi)? + 1,
                t if TYPE_KEYWORDS.contains(&t) => {
                    return (t != "class" || self.cpp).then_some(c);
                }
                _ => c += 1,
            }
        }
        None
    }

    /// Record a type with keyword at `kw` and body `open..close`, spanning
    /// `first..=last`, and scan its body.
    fn type_body(&mut self, kw: usize, open: usize, close: usize, first: usize, last: usize) {
        // The name is the last identifier before `{`, a base clause `:` or
        // template arguments `<`, skipping `final` and attribute groups.
        let mut name = None;
        let mut c = kw + 1;
        while c < open {
            match self.text(c) {
                "class" | "struct" | "final" | "sealed" => c += 1,
                "(" | "[" => match self.close_of(c) {
                    Some(x) if x < open => c = x + 1,
                    _ => break,
                },
                ":" if self.is_scope(c, open) => c += 2,
                ":" | "<" => break,
                _ => {
                    if self.is_ident(c) && !NOT_CALLS.contains(&self.text(c)) {
                        name = Some(c);
                    }
                    c += 1;
                }
            }
        }
        let Some(n) = name else {
            return;
        };
        let name = self.text(n).to_string();
        let kw_text = self.text(kw).to_string();
        let last = last.max(first);
        self.push(name.clone(), SymbolKind::Type, &kw_text, first, last);
        if kw_text != "enum" && open < close {
            self.body(open + 1, close, &Level::Class(name));
        }
    }

    /// Top-level `(` of the parameter list in `[h, hi)`, the first `=`
    /// before it, and the `operator` keyword if any.
    fn find_params(
        &self,
        h: usize,
        mut hi: usize,
    ) -> (Option<usize>, Option<usize>, Option<usize>) {
        let mut c = h;
        while c < hi {
            match self.text(c) {
                "operator" => {
                    let op = c;
                    c += 1;
                    // `operator()` names the call operator.
                    if c + 1 < hi && self.text(c) == "(" && self.text(c + 1) == ")" {
                        c += 2;
                    }
                    while c < hi && self.text(c) != "(" {
                        c += 1;
                    }
                    return ((c < hi).then_some(c), None, Some(op));
                }
                // `void (*signal(int))(int)`: the parameters are inside
                // the pointer declarator group; `int (*fp)(int)` has none.
                "(" if self.is_param_paren(c, h)
                    && c + 1 < hi
                    && matches!(self.text(c + 1), "*" | "&" | "^") =>
                {
                    match self.close_of(c) {
                        Some(x) if x < hi => {
                            hi = x;
                            c += 1;
                        }
                        _ => return (None, None, None),
                    }
                }
                "(" if self.is_param_paren(c, h) => {
                    // `MACRO(type) name(...)`: a macro call before the
                    // declarator; the parameters are the later group.
                    let Some(close) = self.close_of(c).filter(|&x| x < hi) else {
                        return (Some(c), None, None);
                    };
                    let mut k = close + 1;
                    let mut named = false;
                    while k < hi {
                        if self.is_ident(k) && !NOT_CALLS.contains(&self.text(k)) {
                            named = true;
                            k += 1;
                        } else if matches!(self.text(k), "*" | "&") {
                            k += 1;
                        } else if self.is_scope(k, hi) {
                            k += 2;
                        } else if self.opens_angle(k, h) {
                            match self.angle_end(k, hi) {
                                Some(x) => k = x + 1,
                                None => break,
                            }
                        } else {
                            break;
                        }
                    }
                    if named && k < hi && self.text(k) == "(" && self.is_param_paren(k, h) {
                        c = k;
                        continue;
                    }
                    return (Some(c), None, None);
                }
                "<" if self.opens_angle(c, h) => match self.angle_end(c, hi) {
                    Some(x) => c = x + 1,
                    None => c += 1,
                },
                "(" | "[" | "{" => match self.close_of(c) {
                    Some(x) if x < hi => c = x + 1,
                    _ => return (None, None, None),
                },
                "=" => return (None, Some(c), None),
                _ => c += 1,
            }
        }
        (None, None, None)
    }

    /// A function or method whose parameter list opens at `p`.
    fn callable(
        &mut self,
        h: usize,
        p: usize,
        op: Option<usize>,
        level: &Level,
        first: usize,
        last: usize,
    ) {
        let (name, qual, hint) = if let Some(o) = op {
            // `operator ==`, `operator()`, `operator bool`: adjacent tokens
            // join without a space.
            let mut name = String::from("operator");
            for c in o + 1..p {
                if self.tok(c - 1).span.end != self.tok(c).span.start {
                    name.push(' ');
                }
                name.push_str(self.text(c));
            }
            (name, self.qualifier_before(o, h), "operator")
        } else {
            let Some(n) = self.name_before(p, h) else {
                return;
            };
            let dtor = n > h && self.text(n - 1) == "~";
            let q = self.qualifier_before(if dtor { n - 1 } else { n }, h);
            let name = if dtor {
                format!("~{}", self.text(n))
            } else {
                self.text(n).to_string()
            };
            (name, q, if dtor { "destructor" } else { "" })
        };
        let owner = match level {
            Level::Class(o) => Some(o.clone()),
            Level::File => qual,
        };
        let lang = match &owner {
            _ if !hint.is_empty() => hint,
            Some(o) if *o == name => "constructor",
            Some(_) => "method",
            None => "function",
        };
        let kind = if owner.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        self.push(name, kind, lang, first, last);
    }

    /// The identifier naming a declaration whose name ends before `end`,
    /// skipping trailing template arguments `<...>`.
    fn name_before(&self, end: usize, lo: usize) -> Option<usize> {
        let mut c = end.checked_sub(1)?;
        if c < lo {
            return None;
        }
        if self.text(c) == ">" {
            c = self.angle_start(c, lo)?.checked_sub(1)?;
            if c < lo {
                return None;
            }
        }
        self.is_ident(c).then_some(c)
    }

    /// The `<` matching the `>` at `close`, scanning back to `lo`.
    fn angle_start(&self, close: usize, lo: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut c = close;
        loop {
            match self.text(c) {
                ">" => depth += 1,
                "<" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(c);
                    }
                }
                _ => {}
            }
            if c <= lo {
                return None;
            }
            c -= 1;
        }
    }

    /// For a name at `n` preceded by `Owner::` (or `Owner<T>::`), `Owner`.
    fn qualifier_before(&self, n: usize, lo: usize) -> Option<String> {
        if n < lo + 3 || !self.is_scope(n - 2, n) {
            return None;
        }
        let q = self.name_before(n - 2, lo)?;
        Some(self.text(q).to_string())
    }

    /// The name of the first declarator in `[lo, hi)`: the last identifier
    /// before a top-level `,`, `[`, `:` or `hi`, looking inside a
    /// `(*name)` group. Must be at `min` or later.
    fn declarator_name(&self, lo: usize, mut hi: usize, mut min: usize) -> Option<usize> {
        let mut c = lo;
        let mut name = None;
        while c < hi {
            match self.text(c) {
                "(" => {
                    let close = self.close_of(c).filter(|&x| x < hi)?;
                    if c + 1 < close && matches!(self.text(c + 1), "*" | "&" | "^") {
                        (c, hi, min, name) = (c + 1, close, c + 1, None);
                        continue;
                    }
                    if self.is_ident(c.checked_sub(1)?) {
                        break; // parameters after the name
                    }
                    c = close + 1;
                }
                "<" if self.opens_angle(c, lo) => match self.angle_end(c, hi) {
                    Some(x) => c = x + 1,
                    None => break,
                },
                ":" if self.is_scope(c, hi) => c += 2,
                "," | "[" | ":" | "{" | "=" => break,
                _ => {
                    if self.is_ident(c) {
                        name = Some(c);
                    }
                    c += 1;
                }
            }
        }
        name.filter(|&n| n >= min)
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
