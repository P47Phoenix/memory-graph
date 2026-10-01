//! Rust extractor: symbols from `syn`, tokens from the generic tokenizer.
//!
//! `syn` supplies item structure and byte ranges but drops comments and
//! refuses to lex broken input, so tokens always come from
//! `graph_core::tokenizer` (exact spans, comments kept). A file that does not
//! parse yields tokens only and is flagged `has_errors`.
use graph_core::tokenizer::{tokenize_with, TokenizerOptions};
use graph_core::{Extraction, Extractor, Span, SymbolDecl, SymbolKind, TokenClass};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

pub struct RustExtractor;

impl Extractor for RustExtractor {
    fn language(&self) -> &str {
        "rust"
    }

    fn version(&self) -> String {
        // `kw1`: keywords are classed `keyword` (#98). Part of the file
        // fingerprint, so a store indexed before it re-indexes Rust files.
        format!(
            "rust-syn-2+kw1+tok{}",
            graph_core::tokenizer::TOKENIZER_VERSION
        )
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(
            source,
            TokenizerOptions {
                rust_literals: true,
                ..Default::default()
            },
        );
        for i in 0..tokens.len() {
            // The tokenizer splits a raw identifier `r#use` into `r`, `#`,
            // `use`: that `use` is an identifier. A lifetime `'static` is
            // `'`, `static`: no keyword either.
            let lifetime = i >= 1
                && tokens[i - 1].text == "'"
                && tokens[i - 1].span.end == tokens[i].span.start;
            let raw = lifetime
                || i >= 2
                    && tokens[i - 1].text == "#"
                    && tokens[i - 1].span.end == tokens[i].span.start
                    && tokens[i - 2].text == "r"
                    && tokens[i - 2].span.end == tokens[i - 1].span.start;
            let t = &mut tokens[i];
            if !raw && t.class == TokenClass::Identifier && is_keyword(&t.text) {
                t.class = TokenClass::Keyword;
            }
        }
        // syn strips a BOM before lexing, so its byte ranges start after it.
        let bom = if source.starts_with('\u{feff}') { 3 } else { 0 };
        let file = match syn::parse_file(&source[bom..]) {
            Ok(f) => f,
            Err(_) => {
                return Extraction {
                    symbols: vec![],
                    tokens,
                    has_errors: true,
                }
            }
        };
        let mut v = Collector {
            bom,
            src: source,
            line_starts: line_starts(source),
            char_marks: char_marks(source),
            out: vec![],
        };
        v.visit_file(&file);
        Extraction {
            symbols: v.out,
            tokens,
            has_errors: false,
        }
    }
}

/// Rust's strict and reserved keywords (the Reference, edition 2018 and
/// later). Weak keywords (`union`, `macro_rules`, `raw`, `safe`) are also
/// ordinary identifiers, so they stay identifiers; so does `gen`, reserved
/// only in edition 2024 and a common method name before it (`rng.gen()`).
fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "try"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
    )
}

fn line_starts(src: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// Bytes per checkpoint in [`char_marks`].
const MARK: usize = 64;

/// Whether byte `i` starts a char that takes a column: any char except
/// U+FEFF (`EF BB BF`).
fn counts(b: &[u8], i: usize) -> bool {
    b[i] & 0xC0 != 0x80 && !b[i..].starts_with(&[0xEF, 0xBB, 0xBF])
}

/// `marks[k]` = column-taking chars in `src[..k * MARK]`, so a column costs
/// at most `MARK` bytes of scanning instead of a scan from the line start,
/// which was quadratic on one long line of items (#157).
fn char_marks(src: &str) -> Vec<u32> {
    let b = src.as_bytes();
    let mut marks = Vec::with_capacity(b.len() / MARK + 2);
    let mut n = 0u32;
    for i in 0..b.len() {
        if i.is_multiple_of(MARK) {
            marks.push(n);
        }
        n += counts(b, i) as u32;
    }
    if b.len().is_multiple_of(MARK) {
        marks.push(n);
    }
    marks
}

struct Collector<'a> {
    bom: usize,
    src: &'a str,
    line_starts: Vec<usize>,
    char_marks: Vec<u32>,
    out: Vec<SymbolDecl>,
}

impl Collector<'_> {
    /// 1-based line, 1-based char column (a leading BOM takes no column).
    fn pos(&self, off: usize) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= off) - 1;
        let col = self.chars_before(off) - self.chars_before(self.line_starts[line]);
        (line as u32 + 1, col + 1)
    }

    /// Column-taking chars in `src[..off]` (`off` is a char boundary).
    fn chars_before(&self, off: usize) -> u32 {
        let b = self.src.as_bytes();
        let k = off / MARK;
        self.char_marks[k] + (k * MARK..off).filter(|&i| counts(b, i)).count() as u32
    }

    fn push(&mut self, name: String, kind: SymbolKind, lang_kind: &str, span: proc_macro2::Span) {
        let r = span.byte_range();
        let r = r.start + self.bom..r.end + self.bom;
        if r.start >= r.end || r.end > self.src.len() {
            return;
        }
        let (sl, sc) = self.pos(r.start);
        let (el, ec) = self.pos(r.end);
        self.out.push(SymbolDecl {
            owner: None,
            name,
            kind,
            lang_kind: Some(lang_kind.into()),
            span: Span {
                start: r.start as u32,
                end: r.end as u32,
                start_line: sl,
                start_col: sc,
                end_line: el,
                end_col: ec,
            },
        });
    }
}

fn type_name(t: &syn::Type) -> String {
    match t {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or("_".into(), |s| s.ident.to_string()),
        syn::Type::Reference(r) => type_name(&r.elem),
        _ => "_".into(),
    }
}

fn pat_idents(p: &syn::Pat, out: &mut Vec<String>) {
    match p {
        syn::Pat::Ident(i) => out.push(i.ident.to_string()),
        syn::Pat::Type(t) => pat_idents(&t.pat, out),
        syn::Pat::Tuple(t) => t.elems.iter().for_each(|e| pat_idents(e, out)),
        syn::Pat::TupleStruct(t) => t.elems.iter().for_each(|e| pat_idents(e, out)),
        syn::Pat::Reference(r) => pat_idents(&r.pat, out),
        _ => {}
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.push(i.ident.to_string(), SymbolKind::Type, "struct", i.span());
        visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.push(i.ident.to_string(), SymbolKind::Type, "enum", i.span());
        visit::visit_item_enum(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.push(i.ident.to_string(), SymbolKind::Type, "union", i.span());
        visit::visit_item_union(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.push(i.ident.to_string(), SymbolKind::Type, "trait", i.span());
        visit::visit_item_trait(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.push(
            i.ident.to_string(),
            SymbolKind::Type,
            "type_alias",
            i.span(),
        );
        visit::visit_item_type(self, i);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.push(type_name(&i.self_ty), SymbolKind::Other, "impl", i.span());
        visit::visit_item_impl(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.push(i.ident.to_string(), SymbolKind::Module, "mod", i.span());
        visit::visit_item_mod(self, i);
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.push(
            i.sig.ident.to_string(),
            SymbolKind::Function,
            "fn",
            i.span(),
        );
        visit::visit_item_fn(self, i);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.push(i.sig.ident.to_string(), SymbolKind::Method, "fn", i.span());
        visit::visit_impl_item_fn(self, i);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.push(i.sig.ident.to_string(), SymbolKind::Method, "fn", i.span());
        visit::visit_trait_item_fn(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.push(i.ident.to_string(), SymbolKind::Constant, "const", i.span());
        visit::visit_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.push(
            i.ident.to_string(),
            SymbolKind::Variable,
            "static",
            i.span(),
        );
        visit::visit_item_static(self, i);
    }
    fn visit_local(&mut self, l: &'ast syn::Local) {
        let mut names = vec![];
        pat_idents(&l.pat, &mut names);
        for n in names {
            self.push(n, SymbolKind::Variable, "let", l.span());
        }
        visit::visit_local(self, l);
    }
}

#[cfg(test)]
mod tests;
