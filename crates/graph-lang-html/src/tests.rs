use super::*;
use proptest::prelude::*;

type Sym = (String, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = HtmlExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    ex.symbols
        .into_iter()
        .map(|s| {
            assert_eq!(s.kind, SymbolKind::Other);
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.lang_kind.unwrap(), text)
        })
        .collect()
}

pub(crate) fn assert_nested(ex: &Extraction) {
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

#[test]
fn elements_with_ids_and_special_tags() {
    let src = r#"<!DOCTYPE html>
<html><body>
<div id="main" class='x'>
  <p>27" monitor, don't <b id=bold>b</b></p>
  <img id="logo" src="a.png">
  <br/>
  <form id='f1'><input id="q"></form>
</div>
<script>if (a < b && "</div>") { x(); }</script>
<style>p { color: red }</style>
<template><span id="t"></span></template>
</body></html>
"#;
    let s = syms(src);
    let get = |n: &str| {
        s.iter()
            .find(|x| x.0 == n)
            .unwrap_or_else(|| panic!("{n} in {s:#?}"))
    };
    assert_eq!(get("main").1, "element");
    assert!(get("main").2.starts_with("<div id=\"main\"") && get("main").2.ends_with("</div>"));
    assert_eq!(get("bold").2, "<b id=bold>b</b>");
    assert_eq!(get("logo").2, "<img id=\"logo\" src=\"a.png\">");
    assert_eq!(get("f1").1, "form");
    assert_eq!(get("q").2, "<input id=\"q\">");
    assert_eq!(get("script").1, "script");
    assert!(get("script").2.ends_with("</script>"));
    assert_eq!(get("style").1, "style");
    assert_eq!(get("template").1, "template");
    assert_eq!(get("t").1, "element");
    assert_eq!(s.len(), 9);
}

#[test]
fn mismatched_and_unclosed_tags() {
    // `<p>` is closed implicitly by `</div>`: it keeps only its start tag.
    let s = syms("<div id=a><p id=b>text</div><span id=c>open");
    assert_eq!(s[0].2, "<div id=a><p id=b>text</div>");
    assert_eq!(s[1].2, "<p id=b>");
    assert_eq!(s[2].2, "<span id=c>");
    // Close tag with no opener is ignored.
    let s = syms("</div><i id=x></i>");
    assert_eq!(s[0].2, "<i id=x></i>");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}<p>é</p>\n  <div id=\"ü\"></div>";
    let ex = HtmlExtractor.extract(src);
    let d = &ex.symbols[0];
    assert_eq!(d.name, "ü");
    assert_eq!((d.span.start_line, d.span.start_col), (2, 3));
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "<",
        "<div",
        "<div id=",
        "</",
        "<script>",
        "<a id='x",
        "<<>>",
        "<!-- <div id=a>",
        "",
    ] {
        let ex = HtmlExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
    // Tags inside comments are not elements.
    assert!(syms("<!-- <div id=a></div> -->").is_empty());
}

proptest! {
    #[test]
    fn tag_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("<div id=a>"), Just("</div>"), Just("<p id='b'>"), Just("</p>"),
                Just("<br/>"), Just("<script>"), Just("</script>"), Just("<form>"),
                Just("</form>"), Just("<"), Just(">"), Just("\""), Just("'"), Just("x"),
                Just("<!--"), Just("-->"), Just("/"), Just("<img id=i>"), Just("\n"),
            ],
            0..40,
        )
    ) {
        let src = parts.concat();
        let ex = HtmlExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
        }
        assert_nested(&ex);
    }
}
