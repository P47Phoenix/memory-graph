use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = FSharpExtractor.extract(src);
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

const SRC: &str = r#"// comment
namespace Acme.Shapes

open System

(* block (* nested *) comment *)
type Point = { X: float; Y: float }

type Shape =
    | Circle of float
    | Square of float

type Color = Red = 1 | Green = 2

type IShape =
    abstract Area: unit -> float
    abstract member Name: string

[<AbstractClass>]
type Base(x: int) =
    let mutable count = 0
    let helper y = y + x
    new() = Base(0)
    member this.Area() = float x
    member val Label = "" with get, set
    override _.ToString() = "base"
    static member Create n = Base(n)
    abstract Draw: unit -> unit
    default this.Draw() = ()
    interface System.IDisposable with
        member this.Dispose() = ()

and Other = int

type Alias = int list

exception Boom of string

module Geometry =
    [<Literal>]
    let Pi = 3.14
    let inline area r = Pi * r * r
    let rec fact n =
        if n = 0 then 1
        else n * fact (n - 1)
    and helper2 n = n
    let (+.) a b = a + b
    let a, b = 1, 2
    let typed: int = 5

    module Inner =
        let go () = ()
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str(), x.3.as_str())
    };
    let ns = k("Acme.Shapes");
    assert_eq!((ns.0, ns.1), (SymbolKind::Module, "namespace"));
    assert!(ns.2.ends_with("let go () = ()"));
    assert_eq!(
        k("Point"),
        (
            SymbolKind::Type,
            "record",
            "type Point = { X: float; Y: float }"
        )
    );
    assert_eq!(k("Shape").1, "union");
    assert!(k("Shape").2.ends_with("| Square of float"));
    assert_eq!(k("Color").1, "union");
    assert_eq!(k("IShape").1, "interface");
    assert_eq!(
        k("Area"),
        (
            SymbolKind::Method,
            "abstract",
            "abstract Area: unit -> float"
        )
    );
    assert_eq!(k("Name").1, "abstract");
    let base = k("Base");
    assert_eq!((base.0, base.1), (SymbolKind::Type, "class"));
    assert!(base.2.starts_with("[<AbstractClass>]\ntype Base"));
    assert!(base.2.ends_with("member this.Dispose() = ()"));
    assert_eq!(
        k("count"),
        (SymbolKind::Variable, "let", "let mutable count = 0")
    );
    assert_eq!(
        k("helper"),
        (SymbolKind::Method, "let", "let helper y = y + x")
    );
    assert_eq!(k("new"), (SymbolKind::Method, "new", "new() = Base(0)"));
    assert_eq!(
        k("Label"),
        (
            SymbolKind::Variable,
            "member val",
            "member val Label = \"\" with get, set"
        )
    );
    assert_eq!(k("ToString").1, "override");
    assert_eq!(k("Create").1, "static member");
    assert_eq!(k("Draw").1, "abstract");
    assert_eq!(k("Dispose").0, SymbolKind::Method);
    assert_eq!(k("Other"), (SymbolKind::Type, "type", "and Other = int"));
    assert_eq!(k("Alias").1, "type");
    assert_eq!(
        k("Boom"),
        (SymbolKind::Type, "exception", "exception Boom of string")
    );
    let geo = k("Geometry");
    assert_eq!((geo.0, geo.1), (SymbolKind::Module, "module"));
    assert_eq!(
        k("Pi"),
        (
            SymbolKind::Constant,
            "let",
            "[<Literal>]\n    let Pi = 3.14"
        )
    );
    assert_eq!(k("area").0, SymbolKind::Function);
    assert!(k("fact").2.ends_with("fact (n - 1)"));
    assert_eq!(
        k("helper2"),
        (SymbolKind::Function, "let", "and helper2 n = n")
    );
    assert_eq!(k("(+.)").0, SymbolKind::Function);
    assert_eq!(k("typed").0, SymbolKind::Variable);
    assert_eq!(k("Inner").0, SymbolKind::Module);
    assert_eq!(k("go").0, SymbolKind::Function);
    assert!(s.iter().all(|x| x.0 != "a" && x.0 != "System"));
}

#[test]
fn top_level_module_without_equals() {
    let s = syms("module App.Main\n\nlet run () = 0\n\n#if DEBUG\nlet x = 1\n#endif\n");
    let m = find(&s, "App.Main");
    assert_eq!(m.1, SymbolKind::Module);
    assert!(m.3.ends_with("let x = 1"));
    assert_eq!(find(&s, "run").1, SymbolKind::Function);
}

#[test]
fn struct_active_patterns_and_with_members() {
    let s = syms(
        "type P =\n    struct\n        val X: int\n    end\ntype R = { A: int }\n    with member this.B = 1\nlet (|Even|Odd|) n = if n % 2 = 0 then Even else Odd\n",
    );
    assert_eq!(find(&s, "P").2, "struct");
    let b = find(&s, "B");
    assert_eq!((b.1, b.2.as_str()), (SymbolKind::Method, "member"));
    assert_eq!(b.3, "member this.B = 1");
    assert_eq!(find(&s, "R").2, "record");
    let ap = find(&s, "(|Even|Odd|)");
    assert_eq!(ap.1, SymbolKind::Function);
}

#[test]
fn deep_nesting_does_not_overflow() {
    let mut src = String::new();
    for i in 0..6000 {
        src.push_str(&" ".repeat(i));
        src.push_str("module M =\n");
    }
    let ex = FSharpExtractor.extract(&src);
    assert!(ex.symbols.len() > 60);
    assert_nested(&ex);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}module Ü\nlet é = \"ö\"\n";
    let ex = FSharpExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 1));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "let é = \"ö\""
    );
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "namespace",
        "module",
        "module X =",
        "type",
        "type X =",
        "let",
        "let (",
        "let ( a",
        "[<",
        "[<A>]",
        "type T =\n  member",
        "type T =\n  static",
        "and x",
    ] {
        let ex = FSharpExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("namespace"), Just("module"), Just("type"), Just("let"), Just("and"),
                Just("member"), Just("static"), Just("new"), Just("val"), Just("abstract"),
                Just("A"), Just("="), Just("("), Just(")"), Just("{"), Just("}"), Just("|"),
                Just("[<"), Just(">]"), Just("\n"), Just("\n    "), Just("// c\n"),
                Just("(* c *)"), Just("\"s\""), Just("\"\"\"x\n"), Just("#if X\n"),
                Just("with"), Just("."),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = FSharpExtractor.extract(&src);
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

/// #143: reserved words are classed `keyword`; double-backticked names and
/// reserved-for-future words stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    use graph_core::TokenClass::{Identifier, Keyword};
    let toks = FSharpExtractor
        .extract("module M\nlet rec f x = match x with ``type`` -> not sig | _ -> fixed\n")
        .tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    for w in ["module", "let", "rec", "match", "with", "fixed"] {
        assert_eq!(class(w), [Keyword], "{w}");
    }
    for w in ["type", "not", "sig", "M", "f"] {
        assert_eq!(class(w), [Identifier], "{w}");
    }
    assert!(FSharpExtractor
        .version()
        .starts_with("fsharp-scan-1+kw1+tok"));
}
