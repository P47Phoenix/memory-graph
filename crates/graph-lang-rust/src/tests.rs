use super::*;

fn syms(src: &str) -> Vec<(String, SymbolKind, String, String)> {
    RustExtractor
        .extract(src)
        .symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

const SRC: &str = "use std::fmt;\n// note\nstruct S { a: i32 }\nimpl S {\n    fn a(&self) -> i32 { let x = 1; let (y, z) = (2, 3); x + y + z }\n    fn b(&self) {}\n}\nconst N: u8 = 1;\n";

#[test]
fn structure() {
    let s = syms(SRC);
    let names: Vec<_> = s
        .iter()
        .map(|(n, k, l, _)| (n.as_str(), *k, l.as_str()))
        .collect();
    assert!(names.contains(&("S", SymbolKind::Type, "struct")));
    assert!(names.contains(&("S", SymbolKind::Other, "impl")));
    assert!(names.contains(&("a", SymbolKind::Method, "fn")));
    assert!(names.contains(&("b", SymbolKind::Method, "fn")));
    assert!(names.contains(&("x", SymbolKind::Variable, "let")));
    assert!(names.contains(&("z", SymbolKind::Variable, "let")));
    assert!(names.contains(&("N", SymbolKind::Constant, "const")));
    let a = s.iter().find(|x| x.0 == "a").unwrap();
    assert_eq!(
        a.3,
        "fn a(&self) -> i32 { let x = 1; let (y, z) = (2, 3); x + y + z }"
    );
}

#[test]
fn syntax_error_falls_back() {
    let e = RustExtractor.extract("fn foo( { let = ; }");
    assert!(e.has_errors && e.symbols.is_empty() && !e.tokens.is_empty());
}

#[test]
fn comments_and_use_are_tokens() {
    let e = RustExtractor.extract(SRC);
    assert!(e.tokens.iter().any(|t| t.text == "// note"));
    assert!(!e.has_errors);
}

#[test]
fn spans_line_col() {
    let e = RustExtractor.extract("\u{feff}fn é() {}\nfn g() {}\n");
    let g = e.symbols.iter().find(|s| s.name == "g").unwrap();
    assert_eq!((g.span.start_line, g.span.start_col), (2, 1));
    assert_eq!(e.symbols[0].span.start_col, 1);
}

#[test]
fn version_includes_tokenizer_version() {
    assert!(RustExtractor
        .version()
        .ends_with(&format!("+tok{}", graph_core::tokenizer::TOKENIZER_VERSION)));
}

/// #98: Rust keywords are classed `keyword`, identifiers (including weak
/// keywords and raw identifiers) stay `identifier`; broken files too.
#[test]
fn keywords_are_classed_keyword() {
    let src =
        "use std::fmt;\npub fn f(x: &'static str) -> Self { let union = r#use; rng.gen(); }\n";
    for src in [src, "use a; fn {"] {
        let toks = RustExtractor.extract(src).tokens;
        let class = |text: &str| {
            toks.iter()
                .filter(|t| t.text == text)
                .map(|t| t.class)
                .collect::<Vec<_>>()
        };
        assert_eq!(class("use")[0], TokenClass::Keyword, "{src}");
        assert_eq!(class("fn"), [TokenClass::Keyword], "{src}");
        if src.contains("union") {
            assert_eq!(class("pub"), [TokenClass::Keyword]);
            assert_eq!(class("let"), [TokenClass::Keyword]);
            assert_eq!(class("Self"), [TokenClass::Keyword]);
            assert_eq!(class("std"), [TokenClass::Identifier]);
            assert_eq!(class("union"), [TokenClass::Identifier]);
            // The lifetime `'static`.
            assert_eq!(class("static"), [TokenClass::Identifier]);
            // `gen` is reserved only in edition 2024: `rng.gen()` is a call.
            assert_eq!(class("gen"), [TokenClass::Identifier]);
            // The `use` of `r#use` is an identifier.
            let raw = src.find("r#use").unwrap() as u32 + 2;
            let t = toks.iter().find(|t| t.span.start == raw).unwrap();
            assert_eq!((t.text.as_str(), t.class), ("use", TokenClass::Identifier));
        }
    }
}

/// The `+kw1` marker changes the fingerprint of every Rust file, so a store
/// indexed before keyword classing re-indexes them; dropping it must fail.
#[test]
fn version_pins_keyword_classing() {
    assert_eq!(
        RustExtractor.version(),
        format!(
            "rust-syn-2+kw1+tok{}",
            graph_core::tokenizer::TOKENIZER_VERSION
        )
    );
}
