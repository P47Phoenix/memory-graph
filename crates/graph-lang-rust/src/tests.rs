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

fn code_tokens(src: &str) -> Vec<graph_core::TokenDecl> {
    let opts = TokenizerOptions {
        rust_literals: true,
        ..Default::default()
    };
    tokenize_with(src, opts)
}

/// The stack bound holds for the worst shapes measured (#245): each parses
/// on a thread of exactly `stack::bound` bytes, not the 16 MiB floor. The
/// sizes are fixed so the test does not move with the constants.
#[test]
fn the_stack_bound_covers_the_worst_shapes() {
    let e = |body: String| format!("fn f() {{ loop {{ {body}; }} }}\n");
    let shapes = [
        e(format!("{}1", "break ".repeat(3000))),
        e(format!("{}1", "return ".repeat(3000))),
        e(format!("{}{}", "{ ".repeat(2000), "}".repeat(2000))),
        format!("type T = {}u8{};\n", "V<".repeat(2000), ">".repeat(2000)),
        format!("{}{}", "fn f() { ".repeat(1000), "}".repeat(1000)),
    ];
    for src in shapes {
        let need = crate::stack::bound(crate::stack::largest_item(&code_tokens(&src)));
        let parsed = std::thread::Builder::new()
            .stack_size(need)
            .spawn(move || parse_symbols_unguarded(&src).is_some())
            .expect("spawn")
            .join()
            .expect("no overflow");
        assert!(parsed);
    }
}

/// The largest item parsed in place (no parse thread) parses on a 2 MiB
/// thread, the default for spawned threads.
#[test]
fn in_place_items_parse_on_a_2_mib_thread() {
    let mut n = 0;
    while crate::stack::plan(crate::stack::largest_item(&code_tokens(&format!(
        "fn f() {{ {}1 }}",
        "break ".repeat(n + 1)
    )))) == crate::stack::Plan::InPlace
    {
        n += 1;
    }
    let src = format!("fn f() {{ {}1 }}", "break ".repeat(n));
    let (ex, note) = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || RustExtractor.extract_noted(&src))
        .expect("spawn")
        .join()
        .expect("no overflow");
    assert_eq!((ex.symbols.len(), note), (1, None));
}

/// The `+kw1` marker changes the fingerprint of every Rust file, so a store
/// indexed before keyword classing re-indexes them; dropping it must fail.
#[test]
fn version_pins_keyword_classing() {
    assert_eq!(
        RustExtractor.version(),
        format!(
            "rust-syn-2+kw1+deep1+em1+tok{}",
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

/// `(name, span text)` of the `variant` symbols, in order.
fn variants(src: &str) -> Vec<(String, String)> {
    syms(src)
        .into_iter()
        .filter(|s| s.2 == "variant")
        .map(|s| {
            assert_eq!(s.1, SymbolKind::Constant, "{}", s.0);
            (s.0, s.3)
        })
        .collect()
}

/// #269: enum variants are `Constant` / `variant`; a span covers outer
/// attributes, doc comments, fields and the discriminant.
#[test]
fn enum_variants_are_symbols() {
    let src = "/// Doc.
#[derive(Debug)]
pub enum E {
    /// The unit.
    #[default]
    Unit,
    Tuple(u8, String),
    Struct { a: i32, b: Vec<u8> },
    #[deprecated = \"x\"] Disc = 1 << 2,
    unit,
}
";
    assert_eq!(
        variants(src),
        [
            (
                "Unit",
                "/// The unit.
    #[default]
    Unit"
            ),
            ("Tuple", "Tuple(u8, String)"),
            ("Struct", "Struct { a: i32, b: Vec<u8> }"),
            ("Disc", "#[deprecated = \"x\"] Disc = 1 << 2"),
            ("unit", "unit"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let s = syms(src);
    // The enum keeps its span (doc comment and attributes included) and
    // every variant nests in it.
    let e = s.iter().find(|x| x.2 == "enum").unwrap();
    assert!(e.3.starts_with("/// Doc.") && e.3.ends_with('}'));
    let ex = RustExtractor.extract(src);
    let es = ex.symbols.iter().find(|x| x.name == "E").unwrap().span;
    for v in ex.symbols.iter().filter(|x| x.kind == SymbolKind::Constant) {
        assert!(
            es.start <= v.span.start && v.span.end <= es.end,
            "{}",
            v.name
        );
    }
    // Struct-variant fields are not symbols.
    assert!(s.iter().all(|x| x.0 != "a" && x.0 != "b"), "{s:#?}");
    assert_eq!(s.len(), 6, "{s:#?}");
}

#[test]
fn enum_variants_empty_nested_and_in_fns() {
    assert!(variants("enum Never {}").is_empty());
    let src = "mod m { fn f() { enum Local { A } } pub enum G<T> where T: Copy { X(T), Y, } }";
    assert_eq!(
        variants(src),
        [("A", "A"), ("X", "X(T)"), ("Y", "Y")].map(|(a, b)| (a.to_string(), b.to_string()))
    );
    // A broken file still yields no symbols.
    assert!(RustExtractor.extract("enum E { A, B(").symbols.is_empty());
}
