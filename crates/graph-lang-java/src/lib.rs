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
//!
//! A declaration's span runs from its first token (annotations and modifiers
//! included) through its closing `}` or `;`. The package's span runs to the
//! end of the file, so every type nests in it. Method bodies are not scanned
//! (no local or anonymous classes, no lambdas); enum constants are not
//! symbols. A multi-declarator field (`int a, b;`) yields one symbol, named by
//! its first declarator. Odd input never sets `has_errors`: unbalanced
//! braces make the scanner resynchronize one token later. Type bodies nested
//! more than [`MAX_DEPTH`] deep are not scanned (the outer types are kept).
use graph_core::scan::{code_close_table, code_index, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

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
        format!("java-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, JAVA_TOKENIZER);
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in Java tokens (as produced with [`JAVA_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, &[TokenClass::Comment]);
    let mut s = Scanner {
        tokens,
        code: &code,
        out: Vec::new(),
        depth: 0,
        closes: code_close_table(tokens, &code),
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
    fn header_end(&self, start: usize, hi: usize) -> Option<(End, usize)> {
        let mut c = start;
        while c < hi {
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
    fn semi_after(&self, mut c: usize, hi: usize) -> Option<usize> {
        while c < hi {
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

    /// The `;` ending an enum's constant list in `[lo, hi)`, if any.
    fn enum_members(&self, mut c: usize, hi: usize) -> Option<usize> {
        while c < hi {
            match self.text(c) {
                "(" | "[" | "{" => c = self.close_of(c).filter(|&p| p < hi)? + 1,
                ";" => return Some(c),
                _ => c += 1,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
