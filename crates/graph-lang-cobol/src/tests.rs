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
    assert!(CobolExtractor.version().starts_with("cobol-scan-1+kw1+tok"));
}
