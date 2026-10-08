//! Java extractor: a token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer's Java dialect (text blocks). Symbols
//! are found by walking declarations at file and type level:
//!
//! | Java | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `package a.b;` | Module | `package` |
//! | `class` `interface` `enum` `record` | Type | the keyword |
//! | `@interface` | Type | `annotation` |
//! | methods / constructors (incl. annotation elements and compact record constructors) | Method | `method` / `constructor` |
//! | fields | Variable | `field` |
//! | `static final` fields, interface fields | Constant | `constant` |
//! | enum constants (`A`, `@Ann B(1) { ... }`) | Constant | `enum_constant` |
//!
//! A declaration's span runs from its first token (annotations and modifiers
//! included) through its closing `}` or `;`. The package's span runs to the
//! end of the file, so every type nests in it. Method bodies are not scanned
//! (no local or anonymous classes, no lambdas), and neither are enum
//! constant class bodies (in `A { void m() {} }`, `m` is not a symbol). An
//! enum constant's span runs from its first annotation through its
//! arguments and class body; an unbalanced bracket in the constant list
//! drops the constants after it. A multi-declarator field (`int a, b;`)
//! yields one symbol, named by its first declarator. Odd input never sets `has_errors`: unbalanced
//! braces make the scanner resynchronize one token later. Type bodies nested
//! more than [`MAX_DEPTH`] deep are not scanned (the outer types are kept).
use graph_core::scan::{code_close_table, code_index, mark_keywords, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

pub struct JavaExtractor;

/// Tokenizer dialect used for Java.
pub const JAVA_TOKENIZER: TokenizerOptions = TokenizerOptions::JAVA;

impl Extractor for JavaExtractor {
    fn language(&self) -> &str {
        "java"
    }

    fn extensions(&self) -> &[&str] {
        &["java"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        // `em1`: enum constants are symbols (#269).
        format!("java-scan-1+kw1+em1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, JAVA_TOKENIZER);
        let symbols = symbols(&tokens);
        // After the symbol scan, which reads identifiers as it always has.
        // Java has no escaped identifiers.
        mark_keywords(&mut tokens, KEYWORDS, |_, _| false);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Java's reserved keywords, `_` (a keyword since Java 9) and literals
/// (`true`, `false`, `null`).
/// Contextual keywords (`var`, `record`, `yield`, `sealed`, `permits`,
/// `module`, ...) are valid identifiers and stay identifiers.
const KEYWORDS: &[&str] = &[
    "_",
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
];

/// Symbols in Java tokens (as produced with [`JAVA_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, &[TokenClass::Comment]);
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
        depth: 0,
        closes: code_close_table(tokens, &code),
        header_fails: RefCell::default(),
        semis: RefCell::default(),
    };
    s.body(0, code.len(), &Level::File);
    s.out
}

enum Level {
    /// File body: package, imports and types.
    File,
    /// Body of the named type; `interface` fields are constants.
    Type { name: String, interface: bool },
}

const TYPE_KEYWORDS: &[&str] = &["class", "interface", "enum", "record"];

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "static",
    "final",
    "abstract",
    "native",
    "synchronized",
    "transient",
    "volatile",
    "strictfp",
    "default",
    "sealed",
    "non",
    "-",
];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    out: Vec<SymbolDecl>,
    /// Nesting of `body` calls, capped at [`MAX_DEPTH`].
    depth: usize,
    /// [`code_close_table`] of `code`.
    closes: Vec<Option<usize>>,
    /// `(c, hi)` from which [`Scanner::header_end`] fails.
    header_fails: RefCell<HashSet<(usize, usize)>>,
    /// `(c, hi)` -> [`Scanner::semi_after`] from `c`.
    semis: RefCell<HashMap<(usize, usize), Option<usize>>>,
}

/// Deepest nesting of type bodies scanned. Deeper declarations are not
/// reported, their enclosing ones are: this bounds the recursion so
/// adversarial input cannot overflow the indexer thread's stack.
pub const MAX_DEPTH: usize = 256;

/// How a declaration ended.
enum End {
    /// `{` at `open` closed by `}` at `close` (or the range end, exclusive,
    /// when unbalanced).
    Block { open: usize, close: usize },
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

    fn close_of(&self, c: usize) -> Option<usize> {
        self.closes[c]
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
            if matches!(self.text(c), ";" | "}") {
                c += 1;
                continue;
            }
            let Some((end, next)) = self.header_end(c, hi) else {
                c += 1;
                continue;
            };
            if self.declaration(c, &end, level, hi) {
                // A package declaration: the rest of the range is its body,
                // already scanned.
                return;
            }
            c = next;
        }
    }

    /// Where the declaration starting at `start` ends, and the position
    /// after it; `None` if the input is unbalanced or ends first.
    ///
    /// The scan depends only on the position and `hi`, so a failed scan
    /// records every position it passed and a later scan reaching one fails
    /// at once: resynchronizing one token later stays linear on long runs
    /// with no terminator (#146).
    fn header_end(&self, start: usize, hi: usize) -> Option<(End, usize)> {
        let mut visited = Vec::new();
        let r = self.header_end_scan(start, hi, &mut visited);
        if r.is_none() {
            self.header_fails
                .borrow_mut()
                .extend(visited.into_iter().map(|c| (c, hi)));
        }
        r
    }

    fn header_end_scan(
        &self,
        start: usize,
        hi: usize,
        visited: &mut Vec<usize>,
    ) -> Option<(End, usize)> {
        let mut c = start;
        while c < hi {
            if memo_on() && self.header_fails.borrow().contains(&(c, hi)) {
                return None;
            }
            visited.push(c);
            match self.text(c) {
                "(" | "[" => c = self.close_of(c).filter(|&p| p < hi)? + 1,
                // A field initializer (`int[] a = { 1 };`, anonymous classes).
                "=" => {
                    let at = self.semi_after(c, hi)?;
                    return Some((End::Semi { at }, at + 1));
                }
                "{" => {
                    return Some(match self.close_of(c).filter(|&p| p < hi) {
                        Some(close) => (End::Block { open: c, close }, close + 1),
                        // Unbalanced: the body runs to the end of the range.
                        None => (End::Block { open: c, close: hi }, hi),
                    });
                }
                ";" => return Some((End::Semi { at: c }, c + 1)),
                "}" => return None,
                _ => c += 1,
            }
        }
        None
    }

    /// The `;` ending an expression starting at `c`, groups skipped.
    /// Memoized by position (every position the scan passes has the same
    /// answer), so repeated calls over one unterminated run stay linear.
    fn semi_after(&self, c: usize, hi: usize) -> Option<usize> {
        let mut visited = Vec::new();
        let r = self.semi_after_scan(c, hi, &mut visited);
        let mut memo = self.semis.borrow_mut();
        for v in visited {
            memo.insert((v, hi), r);
        }
        r
    }

    fn semi_after_scan(&self, mut c: usize, hi: usize, visited: &mut Vec<usize>) -> Option<usize> {
        while c < hi {
            if let Some(&r) = self.semis.borrow().get(&(c, hi)).filter(|_| memo_on()) {
                return r;
            }
            visited.push(c);
            match self.text(c) {
                "(" | "[" | "{" => c = self.close_of(c).filter(|&p| p < hi)? + 1,
                ";" => return Some(c),
                "}" => return None,
                _ => c += 1,
            }
        }
        None
    }

    fn push(&mut self, name: String, kind: SymbolKind, lang: &str, span: (usize, usize)) {
        let span = span_between(&self.tok(span.0).span, &self.tok(span.1).span);
        self.out.push(SymbolDecl {
            owner: None,
            name,
            kind,
            lang_kind: Some(lang.into()),
            span,
        });
    }

    /// Record the declaration starting at `start`; true for a package
    /// declaration (which consumes the rest of the range).
    fn declaration(&mut self, start: usize, end: &End, level: &Level, hi: usize) -> bool {
        let (head_end, last) = match *end {
            End::Block { open, close } => (open, close.min(hi - 1)),
            End::Semi { at } => (at, at),
        };
        // Skip annotations: `@Name`, `@a.b.Name`, `@Name(...)`, but not
        // `@interface`.
        let mut h = start;
        while h + 1 < head_end && self.text(h) == "@" && self.text(h + 1) != "interface" {
            h += 2;
            while h + 1 < head_end && self.text(h) == "." {
                h += 2;
            }
            if h < head_end && self.text(h) == "(" {
                match self.close_of(h) {
                    Some(p) if p < head_end => h = p + 1,
                    _ => return false,
                }
            }
        }
        if h >= head_end {
            return false;
        }
        // The part naming the declaration ends at the first top-level `(`,
        // `=` or `,` (outside generic arguments).
        let mut name_end = head_end;
        let mut first_paren = None;
        let mut angle = 0usize;
        let mut c = h;
        while c < head_end {
            match self.text(c) {
                "<" => angle += 1,
                ">" => angle = angle.saturating_sub(1),
                "(" => {
                    first_paren = Some(c);
                    name_end = c;
                    break;
                }
                "[" => {
                    c = match self.close_of(c) {
                        Some(p) => p + 1,
                        None => return false,
                    };
                    continue;
                }
                "=" => {
                    name_end = c;
                    break;
                }
                "," if angle == 0 => {
                    name_end = c;
                    break;
                }
                _ => {}
            }
            c += 1;
        }

        if self.text(h) == "package" {
            let name: String = (h + 1..head_end).map(|c| self.text(c)).collect();
            if name.is_empty() {
                return false;
            }
            let End::Semi { at } = *end else {
                return false;
            };
            self.push(name, SymbolKind::Module, "package", (start, hi - 1));
            self.body(at + 1, hi, &Level::File);
            return true;
        }
        if self.text(h) == "import" {
            return false;
        }
        // class / interface / enum / record / @interface
        if let Some(kw) = (h..name_end).find(|&c| TYPE_KEYWORDS.contains(&self.text(c))) {
            let name = kw + 1;
            if name < head_end && self.is_ident(name) {
                let annotation = kw > h && self.text(kw - 1) == "@";
                let kw_text = self.text(kw).to_string();
                let lang = if annotation { "annotation" } else { &kw_text };
                let name_text = self.text(name).to_string();
                self.push(name_text.clone(), SymbolKind::Type, lang, (start, last));
                if let End::Block { open, close } = *end {
                    let level = Level::Type {
                        name: name_text,
                        interface: kw_text == "interface" || annotation,
                    };
                    if kw_text == "enum" {
                        // Constants first, members after the first top-level `;`.
                        if let Some(semi) = self.enum_members(open + 1, close) {
                            self.body(semi + 1, close, &level);
                        }
                    } else {
                        self.body(open + 1, close, &level);
                    }
                }
                return false;
            }
        }
        let Level::Type {
            name: owner,
            interface,
        } = level
        else {
            return false;
        };
        if let Some(p) = first_paren {
            // Method or constructor: the identifier right before `(`.
            let n = p.checked_sub(1).filter(|&n| n >= h && self.is_ident(n));
            let Some(n) = n.filter(|&n| !MODIFIERS.contains(&self.text(n))) else {
                return false;
            };
            let lang = if self.text(n) == owner {
                "constructor"
            } else {
                "method"
            };
            self.push(
                self.text(n).to_string(),
                SymbolKind::Method,
                lang,
                (start, last),
            );
            return false;
        }
        // Field: the identifier before the name end (skipping `[]`s).
        let mut n = name_end;
        while n > h + 1 && self.text(n - 1) == "]" && self.text(n - 2) == "[" {
            n -= 2;
        }
        let Some(n) = n.checked_sub(1).filter(|&n| n >= h && self.is_ident(n)) else {
            return false;
        };
        if let End::Block { .. } = *end {
            // A compact canonical record constructor (`Point { ... }`);
            // otherwise an initializer block (`static { }`) or a stray block.
            if self.text(n) == owner && (h..n).all(|c| MODIFIERS.contains(&self.text(c))) {
                self.push(
                    owner.clone(),
                    SymbolKind::Method,
                    "constructor",
                    (start, last),
                );
            }
            return false;
        }
        if n == h || MODIFIERS.contains(&self.text(n)) {
            return false;
        }
        let words = || (h..n).map(|c| self.text(c));
        let constant =
            *interface || (words().any(|w| w == "static") && words().any(|w| w == "final"));
        let (kind, lang) = if constant {
            (SymbolKind::Constant, "constant")
        } else {
            (SymbolKind::Variable, "field")
        };
        self.push(self.text(n).to_string(), kind, lang, (start, last));
        false
    }

    /// Emit the constants of an enum body in `[lo, hi)` and return the `;`
    /// ending the constant list, if any. Constants are comma-separated
    /// `@Ann NAME(args) { body }` entries before the first top-level `;`;
    /// each span runs from its first token (annotations included) through
    /// its last (arguments and class body included). An unbalanced bracket
    /// ends the scan: later constants are dropped and the members after the
    /// `;` are not scanned (as before #269).
    fn enum_members(&mut self, mut c: usize, hi: usize) -> Option<usize> {
        while c < hi {
            let start = c;
            let mut name = None;
            while c < hi && !matches!(self.text(c), "," | ";") {
                match self.text(c) {
                    "@" if name.is_none() => {
                        // `@Name`, `@a.b.Name`, `@Name(...)`.
                        c += 2;
                        while c + 1 < hi && self.text(c) == "." {
                            c += 2;
                        }
                        if c < hi && self.text(c) == "(" {
                            c = self.close_of(c).filter(|&p| p < hi)? + 1;
                        }
                        continue;
                    }
                    "(" | "[" | "{" => {
                        c = self.close_of(c).filter(|&p| p < hi)? + 1;
                        continue;
                    }
                    _ if name.is_none() && self.is_ident(c) => name = Some(c),
                    _ => {}
                }
                c += 1;
            }
            if let Some(n) = name {
                // A name was seen, so the entry is non-empty.
                let last = c.min(hi) - 1;
                self.push(
                    self.text(n).to_string(),
                    SymbolKind::Constant,
                    "enum_constant",
                    (start, last),
                );
            }
            if c < hi && self.text(c) == ";" {
                return Some(c);
            }
            c += 1;
        }
        None
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
