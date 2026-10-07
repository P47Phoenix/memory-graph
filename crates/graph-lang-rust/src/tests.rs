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

/// Input nested too deep for `syn` comes back as tokens only, with a note
/// for the store to report, and no symbols (#245).
#[test]
fn too_deep_input_is_tokens_only_with_a_note() {
    let n = crate::MAX_NESTING_DEPTH + 1;
    let src = format!("fn f() {{ {}1{}; }}", "(".repeat(n), ")".repeat(n));
    let (ex, note) = RustExtractor.extract_noted(&src);
    assert!(ex.symbols.is_empty() && !ex.has_errors);
    // fn f ( ) { (.. 1 ..) ; }
    assert_eq!(ex.tokens.len(), 2 * n + 8);
    assert!(note.expect("a note").contains("nesting depth"));
    assert_eq!(RustExtractor.extract(&src), ex);
    let (ok, note) = RustExtractor.extract_noted("fn f() {}");
    assert_eq!((ok.symbols.len(), note), (1, None));
}

/// The `+kw1` marker changes the fingerprint of every Rust file, so a store
/// indexed before keyword classing re-indexes them; dropping it must fail.
#[test]
fn version_pins_keyword_classing() {
    assert_eq!(
        RustExtractor.version(),
        format!(
            "rust-syn-2+kw1+deep1+tok{}",
            graph_core::tokenizer::TOKENIZER_VERSION
        )
    );
}

/// Many items on one line stay linear (#157): each symbol's column used to
/// be counted from the line start. 4x the input stays well under 16x the
/// time (allowing 10x for noise).
#[test]
fn many_items_on_one_line_is_linear() {
    // The fastest of 3 runs per size, to shed scheduler noise.
    let time = |n: usize| {
        let src = "struct a {} ".repeat(n);
        (0..3)
            .map(|_| {
                let t = std::time::Instant::now();
                assert_eq!(RustExtractor.extract(&src).symbols.len(), n);
                t.elapsed()
            })
            .min()
            .unwrap()
    };
    time(2_000);
    let small = time(10_000).max(std::time::Duration::from_millis(5));
    let big = time(40_000);
    assert!(big < small * 10, "10k: {small:?}, 40k: {big:?}");
}

/// Columns stay exact across checkpoints: a BOM takes no column, a
/// multi-byte char takes one, on lines longer than a checkpoint block.
#[test]
fn columns_exact_with_bom_and_multibyte() {
    let pad = "struct \u{e9} {} ".repeat(20);
    let src = format!("\u{feff}{pad}struct b {{}}\n{pad}struct c {{}}");
    let syms = RustExtractor.extract(&src).symbols;
    let col = |s: &graph_core::SymbolDecl| {
        let r = s.span.start as usize..s.span.end as usize;
        let line = src[..r.start].rfind('\n').map_or(0, |i| i + 1);
        let c = |o: usize| src[line..o].chars().filter(|&c| c != '\u{feff}').count() as u32 + 1;
        (c(r.start), c(r.end))
    };
    for s in &syms {
        assert_eq!((s.span.start_col, s.span.end_col), col(s), "{}", s.name);
    }
    let b = syms.iter().find(|s| s.name == "b").unwrap();
    assert_eq!(b.span.start_col, 20 * 12 + 1);
}

/// A symbol ending exactly at EOF when the file length is a multiple of the
/// checkpoint size needs the final mark.
#[test]
fn symbol_ending_at_eof_on_a_mark_boundary() {
    let src = format!("{}struct a {{}}", " ".repeat(53));
    assert_eq!(src.len() % MARK, 0);
    let syms = RustExtractor.extract(&src).symbols;
    assert_eq!(syms.len(), 1);
    let s = &syms[0].span;
    assert_eq!((s.start, s.end), (53, 64));
    assert_eq!(
        (s.start_line, s.start_col, s.end_line, s.end_col),
        (1, 54, 1, 65)
    );
}

/// `pos` matches the original formula (chars from the line start, U+FEFF
/// excluded) at every char boundary of seeded random text: CR, CRLF, BOMs
/// mid-file, 4-byte chars and lines longer than a checkpoint block.
#[test]
fn pos_matches_line_scan_differential() {
    let palette = [
        "a",
        " ",
        "\n",
        "\r",
        "\r\n",
        "\u{feff}",
        "\u{e9}",
        "\u{20ac}",
        "\u{1f600}",
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    for case in 0..64 {
        let len = next() % 400;
        let src: String = (0..len)
            .map(|_| {
                // Mostly 'a' so some lines run well past 64 bytes.
                let r = next() % 40;
                if r < palette.len() {
                    palette[r]
                } else {
                    "a"
                }
            })
            .collect();
        let c = Collector {
            bom: 0,
            src: &src,
            line_starts: line_starts(&src),
            char_marks: char_marks(&src),
            out: vec![],
        };
        for off in (0..=src.len()).filter(|&o| src.is_char_boundary(o)) {
            let ls = src[..off].rfind('\n').map_or(0, |i| i + 1);
            let line = src[..ls].matches('\n').count() as u32 + 1;
            let col = src[ls..off].chars().filter(|&ch| ch != '\u{feff}').count() as u32 + 1;
            assert_eq!(c.pos(off), (line, col), "case {case} off {off} {src:?}");
        }
    }
}
