use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = GdscriptExtractor.extract(src);
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

const SRC: &str = "@tool
extends Node2D
class_name Player
# comment

signal hit(damage: int)
signal died
enum State { IDLE, RUN }
const SPEED := 10.0
@export var health: int = 100
@onready
var sprite = $Sprite
static var count = 0
var hp: int:
\tget:
\t\treturn health
\tset(v):
\t\thealth = v

func _ready() -> void:
\tvar local = 1
\tif local:
\t\tpass

static func make(a,
\t\tb):
\treturn null

class Inner extends Object:
\tvar x = 1
\tfunc go():
\t\tpass

func last(): pass
";

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str(), x.3.as_str())
    };
    let player = find(&s, "Player");
    assert_eq!(
        (player.1, player.2.as_str()),
        (SymbolKind::Type, "class_name")
    );
    assert!(player.3.starts_with("@tool") && player.3.ends_with("func last(): pass"));
    assert_eq!(
        k("hit"),
        (SymbolKind::Other, "signal", "signal hit(damage: int)")
    );
    assert_eq!(k("died"), (SymbolKind::Other, "signal", "signal died"));
    assert_eq!(
        k("State"),
        (SymbolKind::Type, "enum", "enum State { IDLE, RUN }")
    );
    assert_eq!(
        k("SPEED"),
        (SymbolKind::Constant, "const", "const SPEED := 10.0")
    );
    assert_eq!(
        k("health"),
        (SymbolKind::Variable, "var", "@export var health: int = 100")
    );
    assert_eq!(
        k("sprite"),
        (
            SymbolKind::Variable,
            "var",
            "@onready\nvar sprite = $Sprite"
        )
    );
    assert_eq!(
        k("count"),
        (SymbolKind::Variable, "var", "static var count = 0")
    );
    assert!(k("hp").2.ends_with("health = v"));
    assert_eq!(k("_ready").0, SymbolKind::Method);
    assert!(k("_ready").2.ends_with("pass"));
    assert_eq!(
        k("make"),
        (
            SymbolKind::Method,
            "static func",
            "static func make(a,\n\t\tb):\n\treturn null"
        )
    );
    assert_eq!(k("Inner").1, "class");
    assert!(k("Inner").2.ends_with("\t\tpass"));
    assert_eq!(k("x").0, SymbolKind::Variable);
    assert_eq!(k("go").0, SymbolKind::Method);
    assert_eq!(k("last").2, "func last(): pass");
    // Locals in bodies are not symbols.
    assert!(s.iter().all(|x| x.0 != "local"));
}

#[test]
fn no_class_name_means_no_file_type() {
    let s = syms("extends Node\nfunc f():\n\tpass\n");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].1, SymbolKind::Method);
}

#[test]
fn semicolons_split_statements() {
    let s = syms("var a = 1; var b = 2\n");
    assert_eq!(find(&s, "a").3, "var a = 1");
    assert_eq!(find(&s, "b").3, "var b = 2");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}class_name Ü\nvar é = \"ö\"\n";
    let ex = GdscriptExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 1));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "var é = \"ö\""
    );
    assert_eq!(ex.symbols[0].span.start_col, 1);
    assert_eq!(ex.symbols[0].name, "Ü");
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "func",
        "func (",
        "class X:",
        "@",
        "@export(",
        "static",
        "var = 1",
        ")))\n  func f(:\n",
        "class_name",
        "enum {A}",
    ] {
        let ex = GdscriptExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("func"), Just("class"), Just("class_name"), Just("var"), Just("const"),
                Just("signal"), Just("enum"), Just("static"), Just("@export"), Just("A"),
                Just(":"), Just("("), Just(")"), Just("{"), Just("}"), Just(";"), Just("\n"),
                Just("\n\t"), Just("\n  "), Just("#c\n"), Just("\"s\""), Just("\"\"\"x\n"),
                Just("="), Just("@"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = GdscriptExtractor.extract(&src);
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
