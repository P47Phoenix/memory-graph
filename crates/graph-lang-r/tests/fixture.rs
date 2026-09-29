//! A larger hand-written fixture (no suitable permissively licensed R repo is
//! vendored in the corpus).
use graph_core::{Extractor, SymbolKind};
use graph_lang_r::RExtractor;

#[test]
fn r_markdown_fixture_symbols() {
    let src = include_str!("fixtures/analysis.Rmd");
    let ex = RExtractor.extract(src);
    for t in &ex.tokens {
        assert_eq!(t.text, src[t.span.start as usize..t.span.end as usize]);
    }
    let got: Vec<(&str, SymbolKind, &str)> = ex
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind, s.lang_kind.as_deref().unwrap()))
        .collect();
    use SymbolKind::*;
    assert_eq!(
        got,
        [
            ("summarise_sales", Function, "function"),
            ("Sale", Type, "setClass"),
            ("total", Method, "generic"),
            ("total", Method, "method"),
            ("Ledger", Type, "R6Class"),
            ("add", Method, "function"),
            ("balance", Method, "function"),
            ("with_tax", Function, "function"),
        ]
    );
}
