use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = CobolExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    ex.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn find<'a>(s: &'a [Sym], name: &str) -> &'a Sym {
    s.iter()
        .find(|x| x.0 == name)
        .unwrap_or_else(|| panic!("{name} not found in {s:#?}"))
}

fn names(s: &[Sym]) -> Vec<&str> {
    s.iter().map(|x| x.0.as_str()).collect()
}

/// Every pair of spans nests or is disjoint (what the store requires).
fn assert_nested(ex: &Extraction) {
    for a in &ex.symbols {
        for b in &ex.symbols {
            let (a, b) = (&a.span, &b.span);
            let disjoint = a.end <= b.start || b.end <= a.start;
            let nested =
                (a.start <= b.start && b.end <= a.end) || (b.start <= a.start && a.end <= b.end);
            assert!(disjoint || nested, "partial overlap {a:?} {b:?}");
        }
    }
}

fn contains(s: &[Sym], outer: &str, inner: &str) -> bool {
    let o = find(s, outer);
    let i = find(s, inner);
    o.3.contains(&i.3) && o.3.len() > i.3.len()
}

fn token_texts(src: &str) -> Vec<(String, TokenClass)> {
    tokenize(src, false)
        .into_iter()
        .map(|t| {
            assert_eq!(src[t.span.start as usize..t.span.end as usize], t.text);
            (t.text, t.class)
        })
        .collect()
}

#[test]
fn digit_led_hyphenated_name_is_one_identifier() {
    let t = token_texts("       1000-ACCTFILE-GET-NEXT.\n       9A-X 0001-OPEN-FILES.\n");
    let ids: Vec<&str> = t
        .iter()
        .filter(|(_, c)| *c == TokenClass::Identifier)
        .map(|(s, _)| s.as_str())
        .collect();
    assert_eq!(ids, ["1000-ACCTFILE-GET-NEXT", "9A-X", "0001-OPEN-FILES"]);
}

#[test]
fn numbers_without_letters_or_with_gaps_are_not_joined() {
    let cases: [(&str, &[&str]); 4] = [
        (
            "       100-200 - 1 - A.\n",
            &["100", "-", "200", "-", "1", "-", "A", "."],
        ),
        ("       MOVE 1.5 TO Y.\n", &["MOVE", "1.5", "TO", "Y", "."]),
        ("       1.5-A\n", &["1.5", "-", "A"]),
        ("       1E5 - B\n", &["1E5", "-", "B"]),
    ];
    for (src, expected) in cases {
        let t = token_texts(src);
        let texts: Vec<&str> = t.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(texts, expected, "{src:?}");
    }
}

#[test]
fn exponent_like_and_multi_number_pieces_are_joined() {
    for (src, word) in [
        ("       1E5-X.\n", "1E5-X"),
        ("       12-34-AB.\n", "12-34-AB"),
    ] {
        let t = token_texts(src);
        let expected = [
            (word.to_string(), TokenClass::Identifier),
            (".".to_string(), TokenClass::Punctuation),
        ];
        assert_eq!(t, expected, "{src:?}");
    }
}

#[test]
fn digit_led_section_is_extracted() {
    let src = "       PROCEDURE DIVISION.\n       1000-MAIN SECTION.\n       1100-STEP.\n           GOBACK.\n";
    let s = syms(src);
    let section = find(&s, "1000-MAIN");
    assert_eq!(section.2, "section");
    assert!(section.3.starts_with("1000-MAIN SECTION."), "{section:?}");
    assert!(contains(&s, "1000-MAIN", "1100-STEP"));
}

#[test]
fn digit_led_paragraph_after_sequence_area_and_indicators() {
    let src = "\
000100 PROCEDURE DIVISION.
000200D    DISPLAY 'DEBUG'.
000300 1000-READ-NEXT.
000400     DISPLAY 'LONG
000500-    'TEXT'.
000600 2000-DONE.
000700     GOBACK.
";
    let s = syms(src);
    assert!(find(&s, "1000-READ-NEXT").3.starts_with("1000-READ-NEXT."));
    assert!(find(&s, "2000-DONE").3.starts_with("2000-DONE."));
    assert!(!names(&s).contains(&"000300"), "{s:#?}");
}

#[test]
fn digit_led_paragraph_in_free_format() {
    let src =
        ">>SOURCE FREE\nPROCEDURE DIVISION.\n1000-main.\n  DISPLAY 1.\n2000-next.\n  GOBACK.\n";
    let s = syms(src);
    assert_eq!(find(&s, "1000-main").2, "paragraph");
    assert_eq!(find(&s, "2000-next").2, "paragraph");
}

#[test]
fn digit_led_paragraphs_are_extracted() {
    let src = "\
       IDENTIFICATION DIVISION.
       PROGRAM-ID. P.
       PROCEDURE DIVISION.
       0000-MAIN.
           PERFORM 1000-ACCTFILE-GET-NEXT.
           GOBACK.
       1000-ACCTFILE-GET-NEXT.
           DISPLAY 'X'.
";
    let s = syms(src);
    let p = find(&s, "1000-ACCTFILE-GET-NEXT");
    assert_eq!(p.2, "paragraph");
    assert!(p.3.starts_with("1000-ACCTFILE-GET-NEXT."), "{p:?}");
    assert_eq!(find(&s, "0000-MAIN").2, "paragraph");
    assert!(contains(&s, "PROCEDURE", "1000-ACCTFILE-GET-NEXT"));
}

#[test]
fn lone_digit_led_paragraphs_fixed_format() {
    let src = "       PROCEDURE DIVISION.\n       100A.\n           DISPLAY 1.\n       9X.\n           GOBACK.\n";
    let s = syms(src);
    let p = find(&s, "100A");
    assert_eq!(p.2, "paragraph");
    assert!(p.3.starts_with("100A."), "{p:?}");
    assert_eq!(find(&s, "9X").2, "paragraph");
}

#[test]
fn lone_digit_led_paragraphs_free_format() {
    let src = ">>SOURCE FREE\nPROCEDURE DIVISION.\n100A.\n  DISPLAY 1.\n9x.\n  GOBACK.\n";
    let s = syms(src);
    assert_eq!(find(&s, "100A").2, "paragraph");
    assert_eq!(find(&s, "9x").2, "paragraph");
}

/// #268: `100A SECTION.` and `100A SECTION .` are section headers.
#[test]
fn lone_digit_led_sections() {
    for src in [
        "       PROCEDURE DIVISION.\n       100A SECTION.\n       P1.\n           DISPLAY 1.\n       9X SECTION .\n           GOBACK.\n",
        ">>SOURCE FREE\nPROCEDURE DIVISION.\n100A SECTION.\nP1.\n  DISPLAY 1.\n9X section .\n  GOBACK.\n",
    ] {
        let ex = CobolExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
        for (name, header) in [("100A", "100A SECTION."), ("9X", "9X ")] {
            let s = ex.symbols.iter().find(|s| s.name == name);
            let s = s.unwrap_or_else(|| panic!("{name}: {:#?}", ex.symbols));
            assert_eq!(s.lang_kind.as_deref(), Some("section"), "{src:?}");
            let start = src.find(&format!("{name} ")).unwrap();
            assert_eq!(s.span.start as usize, start);
            assert!(src[start..].starts_with(header));
            let line = src[..start].matches('\n').count() as u32 + 1;
            assert_eq!(s.span.start_line, line, "{name}");
            let col = (start - src[..start].rfind('\n').map_or(0, |p| p + 1)) as u32 + 1;
            assert_eq!(s.span.start_col, col, "{name}");
        }
        // The paragraph nests in the digit-led section; the section runs to the next one.
        assert!(contains_sym(&ex, "100A", "P1"));
        let sec = ex.symbols.iter().find(|s| s.name == "100A").unwrap();
        assert!(src[sec.span.start as usize..sec.span.end as usize].ends_with("DISPLAY 1."));
    }
}

fn contains_sym(ex: &Extraction, outer: &str, inner: &str) -> bool {
    let o = ex.symbols.iter().find(|s| s.name == outer).unwrap();
    let i = ex.symbols.iter().find(|s| s.name == inner).unwrap();
    o.span.start <= i.span.start && i.span.end <= o.span.end
}

/// #268: a segment number after SECTION (`100A SECTION 50.`).
#[test]
fn lone_digit_led_sections_with_segment_number() {
    for src in [
        "       PROCEDURE DIVISION.\n       100A SECTION 50.\n       P1.\n           DISPLAY 1.\n",
        ">>SOURCE FREE\nPROCEDURE DIVISION.\n100A SECTION 50.\nP1.\n  DISPLAY 1.\n",
    ] {
        let ex = CobolExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
        let s = ex.symbols.iter().find(|s| s.name == "100A");
        let s = s.unwrap_or_else(|| panic!("{src:?}: {:#?}", ex.symbols));
        assert_eq!(s.lang_kind.as_deref(), Some("section"));
        let start = src.find("100A SECTION 50.").unwrap();
        assert_eq!(s.span.start as usize, start);
        assert_eq!(s.span.end as usize, src.find("DISPLAY 1.").unwrap() + 10);
        let line = src[..start].matches('\n').count() as u32 + 1;
        assert_eq!(s.span.start_line, line);
        let col = (start - src[..start].rfind('\n').map_or(0, |p| p + 1)) as u32 + 1;
        assert_eq!(s.span.start_col, col);
        assert!(contains_sym(&ex, "100A", "P1"));
    }
}

#[test]
fn lone_digit_led_sections_negative() {
    let lit = |src: &str| {
        let t = token_texts(src);
        let class = t.iter().find(|(s, _)| s == "100A").map(|(_, c)| *c);
        assert_eq!(class, Some(TokenClass::Literal), "{src:?}: {t:?}");
        assert!(!names(&syms(src)).contains(&"100A"), "{src:?}");
    };
    // Area B.
    lit("       PROCEDURE DIVISION.\n       P.\n           100A SECTION.\n");
    // Not at sentence start.
    lit("       PROCEDURE DIVISION.\n       P. MOVE X TO 100A SECTION.\n");
    lit("       PROCEDURE DIVISION.\n       DISPLAY\n       100A SECTION.\n");
    // Free format, not first on its line.
    let free = ">>SOURCE FREE\nPROCEDURE DIVISION.\nP. 100A SECTION.\n";
    assert!(!names(&syms(free)).contains(&"100A"));
    // SECTION not followed by a period.
    lit("       PROCEDURE DIVISION.\n       100A SECTION X.\n");
    // A segment number must be an integer literal followed by a period.
    lit("       PROCEDURE DIVISION.\n       100A SECTION 50 X.\n");
    let free = ">>SOURCE FREE\nPROCEDURE DIVISION.\n100A SECTION X.\n";
    assert!(!names(&syms(free)).contains(&"100A"));
    // Exponent literal.
    let t = token_texts("       PROCEDURE DIVISION.\n       1E5 SECTION.\n");
    assert!(
        t.iter()
            .any(|(s, c)| s == "1E5" && *c == TokenClass::Literal),
        "{t:?}"
    );
}

#[test]
fn lone_digit_led_literals_outside_headers_stay_literals() {
    let lit = |src: &str, word: &str| {
        let t = token_texts(src);
        let class = t.iter().find(|(s, _)| s == word).map(|(_, c)| *c);
        assert_eq!(class, Some(TokenClass::Literal), "{src:?}: {t:?}");
    };
    lit("       MOVE 1E5 TO X.\n", "1E5");
    lit("       1E5.\n", "1E5");
    lit("       05 X PIC 999B99.\n", "999B99");
    lit("       01 100A.\n", "100A");
    // Area B, sentence start: not a header in fixed format.
    lit("       P.\n           100A.\n", "100A");
    lit("       ADD 100A TO B.\n", "100A");
    // Not followed by a period.
    lit("       100A TO B.\n", "100A");
    let s = syms("       DATA DIVISION.\n       01 100A PIC X.\n");
    assert!(!names(&s).contains(&"100A"), "{s:#?}");
    let free = ">>SOURCE FREE\nPROCEDURE DIVISION.\nP. 100A.\n";
    assert!(!names(&syms(free)).contains(&"100A"));
}

const FIXED: &str = "\
000100 IDENTIFICATION DIVISION.                                         HELLO001
000200 PROGRAM-ID. HELLO.
000300* A comment line
000400 ENVIRONMENT DIVISION.
000500 DATA DIVISION.
000600 FILE SECTION.
000700 FD  IN-FILE.
000800 01  IN-REC            PIC X(80).
000900 WORKING-STORAGE SECTION.
001000 01  WS-TOTAL.
001100     05  WS-COUNT      PIC 9(4) VALUE 0.
001200     05  WS-RATE       PIC 9V99 VALUE 1.25.
001300 01  FILLER            PIC X.
001400 77  WS-FLAG           PIC X VALUE 'Y'.
001500 PROCEDURE DIVISION.
001600 MAIN-LOGIC SECTION.
001700 MAIN-PARA.
001800     PERFORM COUNT-PARA
001900     DISPLAY 'it''s done'. *> inline
002000     STOP RUN.
002100 COUNT-PARA.
002200     ADD 1 TO WS-COUNT
002300     EXIT.
002400 END PROGRAM HELLO.
";

#[test]
fn fixed_format() {
    let s = syms(FIXED);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("HELLO"), (SymbolKind::Module, "program"));
    let prog = &find(&s, "HELLO").3;
    assert!(prog.starts_with("IDENTIFICATION DIVISION."));
    assert!(prog.ends_with("END PROGRAM HELLO."));
    assert_eq!(k("IDENTIFICATION"), (SymbolKind::Other, "division"));
    assert_eq!(k("PROCEDURE"), (SymbolKind::Other, "division"));
    assert_eq!(k("WORKING-STORAGE"), (SymbolKind::Other, "section"));
    assert_eq!(k("MAIN-LOGIC"), (SymbolKind::Other, "section"));
    assert_eq!(k("MAIN-PARA"), (SymbolKind::Function, "paragraph"));
    assert_eq!(
        find(&s, "MAIN-PARA").3,
        "MAIN-PARA.\n001800     PERFORM COUNT-PARA\n001900     DISPLAY 'it''s done'. *> inline\n002000     STOP RUN."
    );
    assert_eq!(
        find(&s, "COUNT-PARA").3,
        "COUNT-PARA.\n002200     ADD 1 TO WS-COUNT\n002300     EXIT."
    );
    assert_eq!(k("WS-TOTAL"), (SymbolKind::Variable, "level-01"));
    assert_eq!(
        find(&s, "WS-TOTAL").3,
        "01  WS-TOTAL.\n001100     05  WS-COUNT      PIC 9(4) VALUE 0.\n001200     05  WS-RATE       PIC 9V99 VALUE 1.25."
    );
    assert_eq!(k("WS-FLAG"), (SymbolKind::Variable, "level-77"));
    assert_eq!(
        find(&s, "WS-FLAG").3,
        "77  WS-FLAG           PIC X VALUE 'Y'."
    );
    assert_eq!(find(&s, "IN-REC").3, "01  IN-REC            PIC X(80).");
    // Nesting.
    assert!(contains(&s, "HELLO", "PROCEDURE"));
    assert!(contains(&s, "PROCEDURE", "MAIN-LOGIC"));
    assert!(contains(&s, "MAIN-LOGIC", "COUNT-PARA"));
    assert!(contains(&s, "WORKING-STORAGE", "WS-FLAG"));
    assert!(contains(&s, "DATA", "WORKING-STORAGE"));
    assert!(!contains(&s, "PROCEDURE", "WS-FLAG"));
    // Not symbols: subordinate items, FILLER, verbs, the FD.
    for n in ["WS-COUNT", "FILLER", "EXIT", "RUN", "IN-FILE", "STOP"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
}

#[test]
fn nested_programs() {
    let src = "\
       IDENTIFICATION DIVISION.
       PROGRAM-ID. OUTER.
       PROCEDURE DIVISION.
       P1.
           CALL 'INNER'.
       IDENTIFICATION DIVISION.
       PROGRAM-ID. INNER.
       PROCEDURE DIVISION.
       P2.
           GOBACK.
       END PROGRAM INNER.
       END PROGRAM OUTER.
       ID DIVISION.
       PROGRAM-ID. 'NEXT'.
";
    let s = syms(src);
    assert!(contains(&s, "OUTER", "INNER"));
    assert!(contains(&s, "INNER", "P2"));
    assert!(!contains(&s, "P1", "INNER"));
    assert!(find(&s, "OUTER").3.ends_with("END PROGRAM OUTER."));
    assert!(find(&s, "INNER").3.ends_with("END PROGRAM INNER."));
    assert_eq!(
        find(&s, "NEXT").3,
        "ID DIVISION.\n       PROGRAM-ID. 'NEXT'."
    );
}

#[test]
fn free_format() {
    let src = ">>SOURCE FORMAT FREE
*> A free-format program
IDENTIFICATION DIVISION.
PROGRAM-ID. free-demo.
DATA DIVISION.
WORKING-STORAGE SECTION.
01 counter PIC 9(4) VALUE 0. *> trailing
PROCEDURE DIVISION.
main.
    PERFORM step
    GOBACK.
step.
    ADD 1 TO counter
    IF counter > 1
        CONTINUE
    END-IF.
END PROGRAM free-demo.
";
    let ex = CobolExtractor.extract(src);
    assert!(ex
        .tokens
        .iter()
        .any(|t| t.text == "*> trailing" && t.class == TokenClass::Comment));
    let s = syms(src);
    assert_eq!(find(&s, "free-demo").1, SymbolKind::Module);
    assert_eq!(find(&s, "counter").3, "01 counter PIC 9(4) VALUE 0.");
    assert_eq!(find(&s, "main").3, "main.\n    PERFORM step\n    GOBACK.");
    assert_eq!(find(&s, "step").1, SymbolKind::Function);
    assert!(find(&s, "step").3.ends_with("END-IF."));
    for n in ["GOBACK", "END-IF", "CONTINUE"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
    assert!(is_free_format("\n  >>source free\n"));
    assert!(is_free_format("$SET SOURCEFORMAT\"FREE\"\n"));
    assert!(!is_free_format(FIXED));
    // No directive, but code in the sequence area.
    assert!(is_free_format(
        "       procedure division.\n bad-query-error.\n"
    ));
    assert!(is_free_format("move 3 to x.\n"));
    assert!(is_free_format("identification division.\n"));
    assert!(!is_free_format(
        "000100D    DISPLAY X.\n      * c\n\tIF A B\nAB0001 MOVE.\n"
    ));
}

#[test]
fn tokens_cover_source_exactly() {
    for (src, free) in [(FIXED, false), (">>SOURCE FREE\nA. *> x 'y'\n*>\n", true)] {
        let t = tokenize(src, free);
        let mut prev = 0;
        for tok in &t {
            assert!(tok.span.start >= prev);
            assert_eq!(
                &src[tok.span.start as usize..tok.span.end as usize],
                tok.text
            );
            prev = tok.span.end;
        }
    }
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}000100 IDENTIFICATION DIVISION.\n000200 PROGRAM-ID. ÉTÉ.\n000300 PROCEDURE DIVISION.\n000400 PARA-É.\n000500     DISPLAY 'é'.\n";
    let ex = CobolExtractor.extract(src);
    let p = ex.symbols.iter().find(|s| s.name == "PARA-É").unwrap();
    assert_eq!((p.span.start_line, p.span.start_col), (4, 8));
    assert_eq!((p.span.end_line, p.span.end_col), (5, 24));
    assert_eq!(
        &src[p.span.start as usize..p.span.end as usize],
        "PARA-É.\n000500     DISPLAY 'é'."
    );
    assert!(ex.symbols.iter().any(|s| s.name == "ÉTÉ"));
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "",
        "       END PROGRAM.\n",
        "       PROGRAM-ID.\n",
        "       PROGRAM-ID",
        "       IDENTIFICATION DIVISION.\n       PROCEDURE DIVISION.\n       A.\n       END PROGRAM X.\n       B.\n",
        "       DATA DIVISION.\n       01\n       77",
        ">>SOURCE FREE\n*>",
        ">>SOURCE FREE\nPROCEDURE DIVISION.\nx. *",
    ] {
        let ex = CobolExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(parts in proptest::collection::vec(prop_oneof![
        Just("       IDENTIFICATION DIVISION."), Just("       PROGRAM-ID. P."),
        Just("       END PROGRAM P."), Just("       PROCEDURE DIVISION."),
        Just("       DATA DIVISION."), Just("       X SECTION."), Just("       PARA."),
        Just("       01 A PIC X."), Just("       77 B."), Just("           MOVE 1 TO A."),
        Just("       FD F."), Just("\n"), Just("\n"), Just("\n"), Just(" é"), Just("*> c"),
        Just("."), Just("'x"), Just(">>SOURCE FREE\n"),
    ], 0..40)) {
        let src: String = parts.concat();
        let ex = CobolExtractor.extract(&src);
        assert_nested(&ex);
        for s in &ex.symbols {
            prop_assert!(s.span.start <= s.span.end && s.span.end as usize <= src.len());
            prop_assert!(src.is_char_boundary(s.span.start as usize));
            prop_assert!(src.is_char_boundary(s.span.end as usize));
        }
    }

    /// Joined or not, every token is its exact source text and tokens never
    /// overlap, in fixed and free format.
    #[test]
    fn digit_led_tokens_are_exact_and_disjoint(
        parts in proptest::collection::vec(prop_oneof![
            Just("1000"), Just("-"), Just("A"), Just("READ-NEXT"), Just("9"), Just("1E5"),
            Just("1.5"), Just("."), Just(" "), Just("\n"), Just("       "), Just("000100 "),
            Just("'x'"), Just("*> c"), Just("é"),
        ], 0..40),
        free in any::<bool>(),
    ) {
        let src: String = parts.concat();
        let mut prev_end = 0;
        for t in tokenize(&src, free) {
            prop_assert_eq!(&src[t.span.start as usize..t.span.end as usize], t.text.as_str());
            prop_assert!(t.span.start >= prev_end, "overlap at {:?}", t.span);
            prev_end = t.span.end;
        }
    }
}

#[test]
fn directive_lines_are_comments() {
    for d in [">>SOURCE FORMAT FREE", "$SET SOURCEFORMAT\"FREE\""] {
        let src = format!("{d}\nIDENTIFICATION DIVISION.\nPROGRAM-ID. p.\n");
        let ex = CobolExtractor.extract(&src);
        assert_eq!(ex.tokens[0].class, TokenClass::Comment);
        assert_eq!(ex.tokens[0].text, d);
        let s = syms(&src);
        assert_eq!(find(&s, "p").3, "IDENTIFICATION DIVISION.\nPROGRAM-ID. p.");
    }
    // Fixed format: a directive in Area A is a comment too.
    let s = syms(
        "       >>SOURCE FORMAT FIXED\n       IDENTIFICATION DIVISION.\n       PROGRAM-ID. q.\n",
    );
    assert!(find(&s, "q").3.starts_with("IDENTIFICATION"));
}

#[test]
fn change_tags_in_sequence_area_stay_fixed() {
    let src = "ABCDEF IDENTIFICATION DIVISION.\nABCDEF PROGRAM-ID. TAGGED.\nABCDEFD    DISPLAY 'X'.\nABCDEF-    'CONT'.\nABCDEF* note\nABCDEF/page\nABCDEF PROCEDURE DIVISION.\nABCDEF PARA-1.\nABCDEF     STOP RUN.\n";
    assert!(!is_free_format(src));
    let s = syms(src);
    assert_eq!(find(&s, "TAGGED").1, SymbolKind::Module);
    assert_eq!(find(&s, "PARA-1").1, SymbolKind::Function);
}

#[test]
fn programs_without_end_program_are_siblings() {
    let src = "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. C.\n       PROCEDURE DIVISION.\n       P1.\n           GOBACK.\n       IDENTIFICATION DIVISION.\n       PROGRAM-ID. D.\n       PROCEDURE DIVISION.\n       P2.\n           GOBACK.\n";
    let s = syms(src);
    assert!(!contains(&s, "C", "D"));
    assert!(find(&s, "C").3.ends_with("GOBACK."));
    assert!(!find(&s, "C").3.contains("PROGRAM-ID. D"));
    assert!(contains(&s, "D", "P2"));
}

#[test]
fn inner_program_closes_before_outer_end() {
    // INNER has no END PROGRAM but OUTER does: INNER ends before it.
    let src = "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. OUTER.\n       IDENTIFICATION DIVISION.\n       PROGRAM-ID. INNER.\n       PROCEDURE DIVISION.\n       P2.\n           GOBACK.\n       END PROGRAM OUTER.\n";
    let s = syms(src);
    assert!(contains(&s, "OUTER", "INNER"));
    assert!(find(&s, "INNER").3.ends_with("GOBACK."));
    assert!(find(&s, "OUTER").3.ends_with("END PROGRAM OUTER."));
}

#[test]
fn copybooks_have_symbols() {
    let s =
        syms("       01  CUST-REC.\n           05  CUST-ID  PIC 9(5).\n       77  FLAG PIC X.\n");
    assert_eq!(find(&s, "CUST-REC").2, "level-01");
    assert_eq!(find(&s, "FLAG").2, "level-77");
    let s = syms("       COMMON-PARA.\n           DISPLAY 'X'.\n");
    assert_eq!(find(&s, "COMMON-PARA").1, SymbolKind::Function);
}

#[test]
fn empty_names_are_not_symbols() {
    let s =
        syms("       IDENTIFICATION DIVISION.\n       PROGRAM-ID. ''.\n       END PROGRAM ''.\n");
    assert!(s.iter().all(|x| !x.0.is_empty()), "{s:#?}");
}

#[test]
fn area_a_limit() {
    // Columns 8-11 are Area A; column 12 is Area B (a statement).
    let src = "       PROCEDURE DIVISION.\n       IN-8.\n          IN-11.\n           IN-12.\n";
    let s = syms(src);
    assert!(names(&s).contains(&"IN-8"));
    assert!(names(&s).contains(&"IN-11"));
    assert!(!names(&s).contains(&"IN-12"));
}

#[test]
fn lone_verbs_are_not_paragraphs_in_free_format() {
    let src = ">>SOURCE FREE\nPROCEDURE DIVISION.\nmain.\n  DISPLAY 1\nEXIT.\nGOBACK.\nCONTINUE.\nELSE.\nSTOP.\nRUN.\nEND-IF.\nnext-one.\n";
    let s = syms(src);
    let fns: Vec<_> = s
        .iter()
        .filter(|x| x.1 == SymbolKind::Function)
        .map(|x| x.0.as_str())
        .collect();
    assert_eq!(fns, ["main", "next-one"]);
}

#[test]
fn fd_ends_a_data_item_and_unnamed_01_is_skipped() {
    let src = "       DATA DIVISION.\n       FILE SECTION.\n       01  A-REC PIC X.\n       FD  OUT-FILE.\n       01  PIC X(4).\n       01  FILLER PIC X.\n       SD  SORT-FILE.\n       01  B-REC PIC X.\n       RD  REP.\n       01  VALUE 1.\n";
    let s = syms(src);
    assert_eq!(find(&s, "A-REC").3, "01  A-REC PIC X.");
    assert_eq!(find(&s, "B-REC").3, "01  B-REC PIC X.");
    for n in ["PIC", "FILLER", "OUT-FILE", "VALUE"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
}

fn classes_of(toks: &[TokenDecl], text: &str) -> Vec<TokenClass> {
    toks.iter()
        .filter(|t| t.text == text)
        .map(|t| t.class)
        .collect()
}

/// #143: every listed word, bare, in upper and lower case, is a keyword.
#[test]
fn every_listed_keyword_is_classed_keyword() {
    for kw in KEYWORDS {
        for w in [kw.to_string(), kw.to_lowercase()] {
            let src = format!(">>SOURCE FORMAT FREE\n{w}\n");
            let toks = CobolExtractor.extract(&src).tokens;
            assert_eq!(classes_of(&toks, &w), [TokenClass::Keyword], "{w}");
        }
    }
}

/// Keywords are classed in fixed format too; data names (even ones that
/// contain a keyword, `END-OF-FILE`), literals and comments are not, and
/// symbols are unchanged.
#[test]
fn keywords_in_a_fixed_format_program() {
    let src = "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. HELLO.\n       PROCEDURE DIVISION.\n       MAIN-PARA.\n      * MOVE in a comment\n           MOVE 'IF' TO END-OF-FILE\n           STOP RUN.\n";
    let ex = CobolExtractor.extract(src);
    let toks = &ex.tokens;
    for kw in [
        "IDENTIFICATION",
        "DIVISION",
        "PROGRAM-ID",
        "PROCEDURE",
        "MOVE",
        "TO",
        "STOP",
    ] {
        assert!(
            classes_of(toks, kw)
                .iter()
                .all(|c| *c == TokenClass::Keyword),
            "{kw}"
        );
        assert!(!classes_of(toks, kw).is_empty(), "{kw}");
    }
    for name in ["HELLO", "MAIN-PARA", "END-OF-FILE", "RUN"] {
        assert_eq!(classes_of(toks, name), [TokenClass::Identifier], "{name}");
    }
    let names: Vec<_> = ex.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"HELLO") && names.contains(&"MAIN-PARA"),
        "{names:?}"
    );
    assert!(CobolExtractor
        .version()
        .starts_with("cobol-scan-1+kw1+dw3+tok"));
}

/// A fixed sample of the list (so dropping a word from `KEYWORDS` fails).
#[test]
fn sample_keywords_are_listed_and_classed() {
    for w in [
        "END-IF",
        "END-PERFORM",
        "WORKING-STORAGE",
        "PROGRAM-ID",
        "PERFORM",
        "MOVE",
        "THRU",
    ] {
        let src = format!(">>SOURCE FORMAT FREE\n{w}\n");
        let toks = CobolExtractor.extract(&src).tokens;
        assert_eq!(classes_of(&toks, w), [TokenClass::Keyword], "{w}");
    }
    assert_eq!(KEYWORDS.len(), 89);
}
