//! TypeScript extractor: the JavaScript scanner in its TypeScript mode
//! (`graph_lang_javascript::typescript_symbols`) plus a pass for the
//! declarations JavaScript does not have. A token-stream scanner, not a parser.
//!
//! | TypeScript | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | everything the JavaScript extractor finds (functions, classes, methods, arrow functions) | as JavaScript | as JavaScript |
//! | `abstract class Name {...}` | Type | `abstract_class` |
//! | typed class fields (`private x: T = ...;`) | Variable | `field` |
//! | method signatures (`abstract m(): void;`, overloads) | Method | `method` |
//! | `declare function f(): T;` | Function | `function` |
//! | `interface Name {...}` | Type | `interface` |
//! | `type Name<T> = ...;` | Type | `type` |
//! | `enum Name {...}` / `const enum` | Type | `enum` |
//! | `namespace A.B {...}` / `module A {...}` / `declare module "m" {...}` | Module | `namespace` / `module` |
//!
//! Spans run from the first keyword (`export`, `declare`, `abstract`, ...)
//! through the closing `}` (or a type alias's `;`, or the last token of its
//! type). Decorators are ignored (they are not part of any span). Not
//! symbols: interface members, enum members, `declare global`, overloads of
//! arrow-typed variables. TSX is scanned like TypeScript; JSX text that
//! looks like a regex or unbalanced braces can cut a scan short, never
//! producing invalid spans or `has_errors`.
use graph_core::scan::{code_index, span_between, NestedEnds, Step};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use graph_lang_javascript::TypeEnds;

pub struct TypeScriptExtractor;

/// Tokenizer dialect used for TypeScript (the JavaScript one).
pub const TS_TOKENIZER: TokenizerOptions = TokenizerOptions::TYPESCRIPT;

impl Extractor for TypeScriptExtractor {
    fn language(&self) -> &str {
        "typescript"
    }

    fn extensions(&self) -> &[&str] {
        &["ts", "tsx", "mts", "cts"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("typescript-scan-2+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, TS_TOKENIZER);
        let symbols = symbols(&tokens);
        // After the symbol scan, which reads identifiers as it always has.
        // TypeScript's own words (`interface`, `type`, `number`, ...) are
        // contextual and stay identifiers.
        graph_lang_javascript::mark_js_keywords(&mut tokens);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Symbols in TypeScript tokens (as produced with [`TS_TOKENIZER`]), in
/// source order (an enclosing symbol before what it contains).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let mut out = graph_lang_javascript::typescript_symbols(tokens);
    let code = code_index(tokens, &[TokenClass::Comment]);
    let closes = graph_core::scan::code_close_table(tokens, &code);
    let s = Scanner {
        tokens,
        code: &code,
        closes: &closes,
        types: TypeEnds::new(tokens, &code, &closes),
        generics: NestedEnds::new(),
    };
    s.scan(&mut out);
    out.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then(b.span.end.cmp(&a.span.end))
    });
    out
}

/// Words that may precede a declaration and belong to its span.
const PREFIXES: &[&str] = &["export", "default", "declare", "const"];

struct Scanner<'a> {
    tokens: &'a [TokenDecl],
    code: &'a [usize],
    closes: &'a [Option<usize>],
    /// Type ends, sharing their memo across declarations.
    types: TypeEnds<'a>,
    /// Ends of type-alias generic parameter lists.
    generics: NestedEnds,
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

    fn start_of(&self, mut c: usize) -> usize {
        while c > 0 && PREFIXES.contains(&self.text(c - 1)) {
            c -= 1;
        }
        c
    }

    fn push(
        &self,
        out: &mut Vec<SymbolDecl>,
        name: String,
        kind: SymbolKind,
        lang: &str,
        span: (usize, usize),
    ) {
        out.push(SymbolDecl {
            owner: None,
            name,
            kind,
            lang_kind: Some(lang.into()),
            span: span_between(&self.tok(span.0).span, &self.tok(span.1).span),
        });
    }

    /// Every TypeScript-only declaration, at any depth.
    fn scan(&self, out: &mut Vec<SymbolDecl>) {
        let hi = self.code.len();
        for c in 0..hi {
            // `a.type`, `x.module`: member accesses, not declarations.
            if c > 0 && matches!(self.text(c - 1), "." | "?.") {
                continue;
            }
            // `class type ...`, `function module(...)`: a declared name, not
            // a keyword (#145: `class type A = {};` made a type alias that
            // ran past the class).
            if c > 0
                && matches!(
                    self.text(c - 1),
                    "class" | "interface" | "enum" | "function" | "namespace" | "module" | "type"
                )
            {
                continue;
            }
            if !self.is_ident(c) || c + 1 >= hi {
                continue;
            }
            match self.text(c) {
                "interface" => self.interface(c, hi, out),
                "type" => self.type_alias(c, hi, out),
                "enum" => self.enum_decl(c, hi, out),
                "namespace" | "module" => self.namespace(c, hi, out),
                _ => {}
            }
        }
    }

    /// The `{` opening a body after a header starting at `c`: generic lists
    /// and groups are skipped; `None` at `;`, `=`, a closer or the end.
    fn body_open(&self, mut c: usize, hi: usize) -> Option<usize> {
        let mut angle = 0usize;
        while c < hi {
            match self.text(c) {
                "<" => angle += 1,
                ">" => angle = angle.saturating_sub(1),
                "{" if angle == 0 => return Some(c),
                "(" | "[" | "{" => c = self.close_of(c).filter(|&p| p < hi)?,
                "=" if angle > 0 => {}
                ";" | "=" | ")" | "]" | "}" => return None,
                _ => {}
            }
            c += 1;
        }
        None
    }

    fn interface(&self, kw: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let name = kw + 1;
        if !self.is_ident(name) {
            return;
        }
        let Some(open) = self.body_open(name + 1, hi) else {
            return;
        };
        let Some(close) = self.close_of(open) else {
            return;
        };
        let name = self.text(name).to_string();
        self.push(
            out,
            name,
            SymbolKind::Type,
            "interface",
            (self.start_of(kw), close),
        );
    }

    fn type_alias(&self, kw: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let name = kw + 1;
        if !self.is_ident(name) || name + 1 >= hi {
            return;
        }
        let mut eq = name + 1;
        if self.text(eq) == "<" {
            // Memoized, so a long run of `type a<` stays linear.
            let close = self.generics.find(eq, hi, |c| match self.text(c) {
                "<" => Step::Open,
                ">" => Step::Close,
                "(" | "[" | "{" => Step::Skip(self.close_of(c)),
                ";" | ")" | "]" | "}" => Step::Stop,
                _ => Step::Other,
            });
            let Some(close) = close else {
                return;
            };
            eq = close + 1;
        }
        if eq + 1 >= hi || self.text(eq) != "=" || self.text(eq + 1) == ">" {
            return;
        }
        let after = self.types.type_end(eq + 1, hi);
        let last = if after < hi && self.text(after) == ";" {
            after
        } else if after > eq + 1 {
            after - 1
        } else {
            return;
        };
        let name = self.text(name).to_string();
        self.push(
            out,
            name,
            SymbolKind::Type,
            "type",
            (self.start_of(kw), last),
        );
    }

    fn enum_decl(&self, kw: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let name = kw + 1;
        if !self.is_ident(name) || name + 1 >= hi || self.text(name + 1) != "{" {
            return;
        }
        let Some(close) = self.close_of(name + 1) else {
            return;
        };
        let name = self.text(name).to_string();
        self.push(
            out,
            name,
            SymbolKind::Type,
            "enum",
            (self.start_of(kw), close),
        );
    }

    /// `namespace A.B {`, `module A {`, `declare module "m" {`.
    fn namespace(&self, kw: usize, hi: usize, out: &mut Vec<SymbolDecl>) {
        let mut c = kw + 1;
        let mut name = String::new();
        if self.tok(c).class == TokenClass::Literal {
            let t = self.text(c);
            let quoted =
                t.len() >= 2 && (t.starts_with('"') || t.starts_with('\'')) && t.ends_with(&t[..1]);
            if !quoted {
                return;
            }
            name.push_str(&t[1..t.len() - 1]);
            c += 1;
        } else {
            loop {
                if c >= hi || !self.is_ident(c) {
                    return;
                }
                name.push_str(self.text(c));
                c += 1;
                if c + 1 < hi && self.text(c) == "." {
                    name.push('.');
                    c += 1;
                    continue;
                }
                break;
            }
        }
        if c >= hi || self.text(c) != "{" || name.is_empty() {
            return;
        }
        let Some(close) = self.close_of(c) else {
            return;
        };
        let lang = self.text(kw).to_string();
        self.push(
            out,
            name,
            SymbolKind::Module,
            &lang,
            (self.start_of(kw), close),
        );
    }
}

#[cfg(test)]
mod tests;
