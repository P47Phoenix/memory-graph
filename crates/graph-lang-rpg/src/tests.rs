use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = RpgExtractor.extract(src);
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

const FREE: &str = "**FREE
ctl-opt nomain;
// A comment
/copy qrpglesrc,protos
dcl-c MAX_ITEMS 100;
dcl-s counter int(10) inz(0);
dcl-ds order_t qualified template;
  id int(10);
  qty packed(7:2);
end-ds;
dcl-ds curOrder likeds(order_t);
dcl-pr getOrder extpgm('GETORD');
  id int(10) const;
end-pr;
dcl-pr noParms end-pr;

DCL-PROC addItem EXPORT;
  DCL-PI *N IND;
    qty packed(7:2) value;
  END-PI;
  dcl-s local int(10);
  exsr bump;
  return *on;

  begsr bump;
    counter += 1;
  endsr;
END-PROC;
**CTDATA names
dcl-s notCode int(10);
";

#[test]
fn fully_free() {
    let s = syms(FREE);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("MAX_ITEMS"), (SymbolKind::Constant, "constant"));
    assert_eq!(find(&s, "MAX_ITEMS").3, "dcl-c MAX_ITEMS 100;");
    assert_eq!(k("counter"), (SymbolKind::Variable, "standalone"));
    assert_eq!(k("order_t"), (SymbolKind::Type, "ds"));
    assert_eq!(
        find(&s, "order_t").3,
        "dcl-ds order_t qualified template;\n  id int(10);\n  qty packed(7:2);\nend-ds;"
    );
    assert_eq!(find(&s, "curOrder").3, "dcl-ds curOrder likeds(order_t);");
    assert_eq!(k("getOrder"), (SymbolKind::Other, "prototype"));
    assert!(find(&s, "getOrder").3.ends_with("end-pr;"));
    assert_eq!(find(&s, "noParms").3, "dcl-pr noParms end-pr;");
    assert_eq!(k("addItem"), (SymbolKind::Function, "procedure"));
    assert!(find(&s, "addItem")
        .3
        .starts_with("DCL-PROC addItem EXPORT;"));
    assert!(find(&s, "addItem").3.ends_with("END-PROC;"));
    assert_eq!(k("*N"), (SymbolKind::Other, "interface"));
    assert!(find(&s, "*N").3.ends_with("END-PI;"));
    assert_eq!(k("bump"), (SymbolKind::Function, "subroutine"));
    assert_eq!(
        find(&s, "bump").3,
        "begsr bump;\n    counter += 1;\n  endsr;"
    );
    assert!(contains(&s, "addItem", "*N"));
    assert!(contains(&s, "addItem", "local"));
    assert!(contains(&s, "addItem", "bump"));
    // Subfields, compile-time data, directives are not symbols.
    for n in ["id", "qty", "notCode", "protos", "nomain"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
}

// Columns: 6 form type, 7-21 name, 24-25 declaration type / B-E,
// C spec factor 1 at 12, opcode at 26.
const FIXED: &str = "      * fixed-form RPG IV
     H DFTACTGRP(*NO)
     D MaxRows         C                   CONST(50)
     D rowCount        S             10I 0 INZ(0)
     D Customer        DS                  QUALIFIED
     D  custId                       10I 0
     D  custName                     50A
     D thisIsAVeryLongPrototypeName...
     D                 PR                  EXTPGM('LONG')
     D  parm1                        10A
     C     START         TAG
     C                   EXSR      Calc
     C     Calc          BEGSR
     C                   EVAL      rowCount = rowCount + 1
     C                   ENDSR
     P Compute         B                   EXPORT
     D Compute         PI            10I 0
     D  x                            10I 0 VALUE
      /free
        dcl-s tmp int(10);
        return x * 2;
      /end-free
     P Compute         E
";

#[test]
fn fixed_form() {
    let s = syms(FIXED);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("MaxRows"), (SymbolKind::Constant, "constant"));
    assert_eq!(k("rowCount"), (SymbolKind::Variable, "standalone"));
    assert_eq!(
        find(&s, "rowCount").3,
        "D rowCount        S             10I 0 INZ(0)"
    );
    assert_eq!(k("Customer"), (SymbolKind::Type, "ds"));
    assert_eq!(
        find(&s, "Customer").3,
        "D Customer        DS                  QUALIFIED\n     D  custId                       10I 0\n     D  custName                     50A"
    );
    assert_eq!(
        k("thisIsAVeryLongPrototypeName"),
        (SymbolKind::Other, "prototype")
    );
    let pr = &find(&s, "thisIsAVeryLongPrototypeName").3;
    assert!(pr.starts_with("D thisIsAVeryLongPrototypeName..."), "{pr}");
    assert!(pr.ends_with("parm1                        10A"), "{pr}");
    assert_eq!(k("START"), (SymbolKind::Other, "tag"));
    assert_eq!(k("Calc"), (SymbolKind::Function, "subroutine"));
    assert!(find(&s, "Calc").3.starts_with("C     Calc          BEGSR"));
    assert!(find(&s, "Calc").3.ends_with("ENDSR"));
    let procs: Vec<_> = s.iter().filter(|x| x.0 == "Compute").collect();
    assert_eq!(procs.len(), 2);
    assert_eq!(
        (procs[0].1, procs[0].2.as_str()),
        (SymbolKind::Function, "procedure")
    );
    assert!(procs[0].3.ends_with("P Compute         E"));
    assert_eq!(
        (procs[1].1, procs[1].2.as_str()),
        (SymbolKind::Other, "interface")
    );
    assert!(procs[0].3.contains(&procs[1].3));
    assert_eq!(k("tmp"), (SymbolKind::Variable, "standalone"));
    assert_eq!(find(&s, "tmp").3, "dcl-s tmp int(10);");
    for n in ["custId", "custName", "parm1", "x", "DFTACTGRP"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
}

#[test]
fn mixed_free_in_fixed_file() {
    let src = "     H NOMAIN
       dcl-proc greet export;
         dcl-pi *n varchar(50);
           who varchar(40) const;
         end-pi;
         return 'Hi ' + who;
       end-proc;
     D after           S              5P 0
";
    let s = syms(src);
    assert!(contains(&s, "greet", "*N"));
    assert!(find(&s, "greet").3.ends_with("end-proc;"));
    assert_eq!(find(&s, "after").1, SymbolKind::Variable);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}**FREE\n// é\ndcl-s café int(10);\ndcl-proc naïve;\nend-proc;\n";
    let ex = RpgExtractor.extract(src);
    let p = ex.symbols.iter().find(|s| s.name == "naïve").unwrap();
    assert_eq!((p.span.start_line, p.span.start_col), (4, 1));
    assert_eq!((p.span.end_line, p.span.end_col), (5, 10));
    let c = ex.symbols.iter().find(|s| s.name == "café").unwrap();
    assert_eq!((c.span.start_line, c.span.end_col), (3, 20));
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "",
        "**FREE",
        "**FREE\nend-proc;\nend-ds;\nendsr;\n",
        "**FREE\ndcl-proc a;\ndcl-ds x;\nend-proc;\nend-ds;\n",
        "**FREE\nbegsr s;\ndcl-proc p;\nendsr;\nend-proc;\n",
        "**FREE\ndcl-pi",
        "**FREE\ndcl-s;\ndcl-ds *;\n",
        "     P x               B\n     D y               DS\n     P x               E\n",
        "     D a...\n     D b...\n",
        "     C                   BEGSR\n     C                   ENDSR\n",
    ] {
        let ex = RpgExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
    }
    // An unclosed block spans its header statement only.
    let s = syms("**FREE\ndcl-ds x;\n  a int(10);\ndcl-proc p;\nend-proc;\n");
    assert_eq!(find(&s, "x").3, "dcl-ds x;");
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(parts in proptest::collection::vec(prop_oneof![
        Just("dcl-proc p;"), Just("end-proc;"), Just("dcl-ds d;"), Just("end-ds;"),
        Just("dcl-pr q;"), Just("end-pr;"), Just("dcl-pi *n;"), Just("end-pi;"),
        Just("dcl-s v int(10);"), Just("dcl-c k 1;"), Just("begsr s;"), Just("endsr;"),
        Just("     P x               B"), Just("     P x               E"),
        Just("     D y               DS"), Just("     D  f                         5A"),
        Just("     C     T             TAG"), Just("     C     S             BEGSR"),
        Just("     C                   ENDSR"), Just("     D long..."),
        Just("\n"), Just("\n"), Just("\n"), Just(" é"), Just("// c"), Just("'x"), Just("**CTDATA"),
    ], 0..40), free in any::<bool>()) {
        let mut src: String = parts.concat();
        if free {
            src.insert_str(0, "**FREE\n");
        }
        let ex = RpgExtractor.extract(&src);
        assert_nested(&ex);
        for s in &ex.symbols {
            prop_assert!(s.span.start <= s.span.end && s.span.end as usize <= src.len());
            prop_assert!(src.is_char_boundary(s.span.start as usize));
            prop_assert!(src.is_char_boundary(s.span.end as usize));
        }
    }
}

#[test]
fn compile_time_data_markers() {
    for marker in ["**CTDATA arr", "**ftrans", "**ALTSEQ", "**", "** names"] {
        let src = format!("**FREE\ndcl-s before int(10);\n{marker}\ndcl-s after int(10);\n");
        let s = syms(&src);
        assert!(names(&s).contains(&"before"), "{marker}");
        assert!(!names(&s).contains(&"after"), "{marker}: {s:#?}");
    }
    // Fixed form too.
    let s = syms("     D before          S             10I 0\n**CTDATA x\n     D after           S             10I 0\n");
    assert!(names(&s).contains(&"before") && !names(&s).contains(&"after"));
    // A `**` exponent continuing a free-form expression is not a marker.
    let src = "**FREE\ndcl-proc p;\n  x = y\n**2;\n  dcl-s later int(10);\nend-proc;\n";
    let s = syms(src);
    assert!(names(&s).contains(&"later"), "{s:#?}");
    assert!(find(&s, "p").3.ends_with("end-proc;"));
}

#[test]
fn likeds_and_likerec_are_one_statement() {
    let src = "**FREE\ndcl-ds a likeds(t);\ndcl-ds b LIKEREC(r);\ndcl-s c int(10);\ndcl-ds d;\n  f int(10);\nend-ds;\n";
    let s = syms(src);
    assert_eq!(find(&s, "a").3, "dcl-ds a likeds(t);");
    assert_eq!(find(&s, "b").3, "dcl-ds b LIKEREC(r);");
    assert_eq!(find(&s, "c").1, SymbolKind::Variable);
    assert_eq!(find(&s, "d").3, "dcl-ds d;\n  f int(10);\nend-ds;");
}

#[test]
fn long_names_over_several_lines() {
    let src = "     D partOne...\n     D   partTwo...\n     D                 DS                  QUALIFIED\n     D  sub                          5A\n";
    let s = syms(src);
    let ds = find(&s, "partOnepartTwo");
    assert_eq!(ds.1, SymbolKind::Type);
    assert!(ds.3.starts_with("D partOne..."), "{}", ds.3);
    assert!(ds.3.ends_with("5A"));
}

#[test]
fn subroutine_stops_at_procedure_boundary() {
    // `begsr` without `endsr` before the procedure ends: header only.
    let src = "**FREE\ndcl-proc p;\n  begsr s;\n    x = 1;\nend-proc;\ndcl-proc q;\n  endsr;\nend-proc;\n";
    let s = syms(src);
    assert_eq!(find(&s, "s").3, "begsr s;");
    assert!(contains(&s, "p", "s"));
    let src = "**FREE\nbegsr s;\ndcl-proc q;\nend-proc;\nendsr;\n";
    let s = syms(src);
    assert_eq!(find(&s, "s").3, "begsr s;");
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
            let src = format!("**FREE\n{w};\n");
            let toks = RpgExtractor.extract(&src).tokens;
            assert_eq!(classes_of(&toks, &w), [TokenClass::Keyword], "{w}");
        }
    }
}

/// Built-ins, special words, qualified subfields and declared names stay
/// identifiers; symbols are unchanged.
#[test]
fn rpg_keyword_escapes() {
    let src = "**FREE\ndcl-s read ind;\ndcl-proc Main;\n  if %open(f) and not *in99;\n    eval ds.update = 1;\n  endif;\nend-proc;\n";
    let ex = RpgExtractor.extract(src);
    let toks = &ex.tokens;
    for kw in [
        "dcl-s", "dcl-proc", "if", "and", "not", "eval", "endif", "end-proc",
    ] {
        assert_eq!(classes_of(toks, kw), [TokenClass::Keyword], "{kw}");
    }
    assert_eq!(classes_of(toks, "read"), [TokenClass::Identifier]);
    assert_eq!(classes_of(toks, "open"), [TokenClass::Identifier]);
    assert_eq!(classes_of(toks, "update"), [TokenClass::Identifier]);
    let names: Vec<_> = ex.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"read") && names.contains(&"Main"),
        "{names:?}"
    );
    assert!(RpgExtractor.version().starts_with("rpg-scan-2+kw1+tok"));
}
