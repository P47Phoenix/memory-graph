//! Rust extractor: symbols from `syn`, tokens from the generic tokenizer.
//!
//! `syn` supplies item structure and byte ranges but drops comments and
//! refuses to lex broken input, so tokens always come from
//! `graph_core::tokenizer` (exact spans, comments kept). A file that does not
//! parse yields tokens only and is flagged `has_errors`.
use graph_core::tokenizer::tokenize;
use graph_core::{Extraction, Extractor, Span, SymbolDecl, SymbolKind};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

pub struct RustExtractor;

impl Extractor for RustExtractor {
    fn language(&self) -> &str {
        "rust"
    }

    fn version(&self) -> String {
        format!("rust-syn-1+tok{}", graph_core::tokenizer::TOKENIZER_VERSION)
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize(source);
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

fn line_starts(src: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

struct Collector<'a> {
    bom: usize,
    src: &'a str,
    line_starts: Vec<usize>,
    out: Vec<SymbolDecl>,
}

impl Collector<'_> {
    /// 1-based line, 1-based char column (a leading BOM takes no column).
    fn pos(&self, off: usize) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= off) - 1;
        let col = self.src[self.line_starts[line]..off]
            .chars()
            .filter(|&c| c != '\u{feff}')
            .count();
        (line as u32 + 1, col as u32 + 1)
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
