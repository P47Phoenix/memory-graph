use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = ShellExtractor.extract(src);
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

const SRC: &str = r#"#!/usr/bin/env bash
# { not a brace
export PATH="$HOME/bin:$PATH" EDITOR=vim
readonly VERSION=1.2
declare -rx ROOT=$(cd "$(dirname "$0")" && pwd)
declare -a list=(a b)
export -f helper

greet() {
  local name=${1:-world}
  echo "hello } $name"
}

function build {
  inner() ( echo sub )
  case "$1" in
    a|b) echo ab ;;
    (c) echo c ;;
  esac
}

function foo-bar::baz() (
  case $x in
    y) true ;;
  esac
)
if true; then cleanup() { :; }; fi
echo greet() # not a definition
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("PATH"), (SymbolKind::Variable, "export"));
    assert_eq!(k("EDITOR"), (SymbolKind::Variable, "export"));
    assert_eq!(k("VERSION"), (SymbolKind::Constant, "readonly"));
    assert_eq!(k("ROOT"), (SymbolKind::Constant, "declare"));
    assert_eq!(k("greet"), (SymbolKind::Function, "function"));
    assert_eq!(find(&s, "PATH").3, "export PATH=\"$HOME/bin:$PATH\"");
    assert_eq!(find(&s, "EDITOR").3, "EDITOR=vim");
    assert_eq!(
        find(&s, "ROOT").3,
        "declare -rx ROOT=$(cd \"$(dirname \"$0\")\" && pwd)"
    );
    assert_eq!(
        find(&s, "greet").3,
        "greet() {\n  local name=${1:-world}\n  echo \"hello } $name\"\n}"
    );
    let build = find(&s, "build");
    assert!(build.3.starts_with("function build {") && build.3.ends_with("esac\n}"));
    assert_eq!(find(&s, "inner").3, "inner() ( echo sub )");
    let fb = find(&s, "foo-bar::baz");
    assert!(fb.3.ends_with("esac\n)"), "{}", fb.3);
    assert_eq!(find(&s, "cleanup").3, "cleanup() { :; }");
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert!(!names.contains(&"list") && !names.contains(&"helper"));
    assert_eq!(s.iter().filter(|x| x.0 == "greet").count(), 1, "{names:?}");
}

#[test]
fn export_stops_at_glued_separators() {
    let s = syms("export B=2; do_thing arg\nif true; then export Y=2; fi\n(readonly Z=1)\nexport W=$(a; b)|c\n");
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["B", "Y", "Z", "W"]);
    assert_eq!(find(&s, "B").3, "export B=2");
    assert_eq!(find(&s, "Y").3, "export Y=2");
    assert_eq!(find(&s, "Z").3, "readonly Z=1");
    assert_eq!(find(&s, "W").3, "export W=$(a; b)");
}

#[test]
fn nested_functions_nest() {
    let ex = ShellExtractor.extract(SRC);
    let span = |n: &str| ex.symbols.iter().find(|s| s.name == n).unwrap().span;
    let (outer, inner) = (span("build"), span("inner"));
    assert!(outer.start < inner.start && inner.end < outer.end);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}# é\nf_é() { echo ü; }\n";
    let ex = ShellExtractor.extract(src);
    let f = &ex.symbols[0];
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "f_é() { echo ü; }"
    );
    assert_eq!((f.span.start_line, f.span.start_col), (2, 1));
    assert_eq!(f.span.end_col, 18);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "f() {",
        "function",
        "function f",
        "f() ( case x in a) ",
        "export",
        "declare -",
        "readonly A=$(",
        "}}} f() { ) }",
        "esac ) f() ( )",
    ] {
        let ex = ShellExtractor.extract(src);
        assert_nested(&ex);
        for s in &ex.symbols {
            assert!(s.span.end as usize <= src.len());
        }
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("f"), Just("()"), Just("{"), Just("}"), Just("("), Just(")"), Just("function"),
                Just("case"), Just("esac"), Just(";"), Just("\n"), Just("export"), Just("readonly"),
                Just("declare"), Just("-r"), Just("A=1"), Just("B="), Just("$("), Just("'q'"),
                Just("\"d\""), Just("# c\n"), Just("<<EOF\nx\nEOF\n"), Just("é"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = ShellExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
    }
}
