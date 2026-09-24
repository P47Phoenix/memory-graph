use super::*;
use proptest::prelude::*;

fn syms(src: &str) -> Vec<(String, SymbolKind, String, String)> {
    let ex = CSharpExtractor.extract(src);
    assert!(!ex.has_errors);
    ex.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn find<'a>(
    s: &'a [(String, SymbolKind, String, String)],
    name: &str,
) -> &'a (String, SymbolKind, String, String) {
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

const SRC: &str = r#"using System;
// comment
namespace Acme.Billing
{
    [Serializable]
    public sealed class Invoice<T> : Base<T>, IDisposable where T : class
    {
        public const int Max = 10;
        private readonly List<int> _lines = new();
        public event EventHandler Changed;
        public string Name { get; set; } = "x";
        public int Count => _lines.Count;
        public int this[int i] => _lines[i];
        public Invoice(int n) : base(n) { }
        ~Invoice() { }
        public (int, int) Pair() => (1, 2);
        public T Get<U>(U u) where U : struct { return default; }
        public static Invoice<T> operator +(Invoice<T> a, Invoice<T> b) => a;
        Func<int> _f = () => 1;
        #region Inner
        private enum State { Open, Closed }
        #endregion
        interface INested { void Run(); }
    }
    public delegate void Handler(object s);
    public record Person(string Name);
    public record struct Point(int X, int Y);
}
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("Acme.Billing"), (SymbolKind::Module, "namespace"));
    assert_eq!(k("Invoice").0, SymbolKind::Type);
    assert_eq!(k("Max"), (SymbolKind::Constant, "const"));
    assert_eq!(k("_lines"), (SymbolKind::Variable, "field"));
    assert_eq!(k("Changed"), (SymbolKind::Variable, "event"));
    assert_eq!(k("Name"), (SymbolKind::Variable, "property"));
    assert_eq!(k("Count"), (SymbolKind::Variable, "property"));
    assert_eq!(k("this"), (SymbolKind::Variable, "indexer"));
    assert_eq!(k("Pair"), (SymbolKind::Method, "method"));
    assert_eq!(k("Get"), (SymbolKind::Method, "method"));
    assert_eq!(k("operator +"), (SymbolKind::Method, "operator"));
    assert_eq!(k("_f"), (SymbolKind::Variable, "field"));
    assert_eq!(k("State"), (SymbolKind::Type, "enum"));
    assert_eq!(k("INested"), (SymbolKind::Type, "interface"));
    assert_eq!(k("Run"), (SymbolKind::Method, "method"));
    assert_eq!(k("Handler"), (SymbolKind::Type, "delegate"));
    assert_eq!(k("Person"), (SymbolKind::Type, "record"));
    assert_eq!(k("Point"), (SymbolKind::Type, "record"));
    let ctors: Vec<_> = s
        .iter()
        .filter(|x| x.0 == "Invoice")
        .map(|x| x.2.as_str())
        .collect();
    assert_eq!(ctors, ["class", "constructor", "finalizer"]);
    // Enum members and method locals are not symbols.
    assert!(s.iter().all(|x| x.0 != "Open" && x.0 != "u"));
    // Spans: attributes included; initializers included.
    assert!(find(&s, "Invoice")
        .3
        .starts_with("[Serializable]\n    public sealed class Invoice<T>"));
    assert!(find(&s, "Invoice").3.ends_with('}'));
    assert_eq!(
        find(&s, "Name").3,
        "public string Name { get; set; } = \"x\";"
    );
    assert_eq!(find(&s, "Pair").3, "public (int, int) Pair() => (1, 2);");
    assert_nested(&CSharpExtractor.extract(SRC));
}

#[test]
fn file_scoped_namespace_and_top_level_statements() {
    let src = "using X;\nConsole.WriteLine(\"hi\");\nnamespace A.B;\nclass C { void M() { var x = 1; } }\n";
    let s = syms(src);
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["A.B", "C", "M"]);
    assert!(find(&s, "A.B").3.ends_with('}'));
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}class Ü {\n  int é;\n}";
    let ex = CSharpExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 3));
    assert_eq!(&src[f.span.start as usize..f.span.end as usize], "int é;");
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "class A {",
        "class A { void M( { }",
        "}}} class",
        "namespace { }",
        "class A { int x = ; } }",
        "class A<T { }",
        "public ~() {}",
        "[",
        "",
    ] {
        let ex = CSharpExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

#[test]
fn expression_bodied_members_are_named_before_the_arrow() {
    let s = syms(
        "class A { int Count => 42; int Name => Get(x); string S => \"a\"; int T => a.b.C; \
         Func<int,int> F => x => x; int M() => 1; }\ninterface I { int P => 1; }",
    );
    let got: Vec<_> = s.iter().map(|x| (x.0.as_str(), x.2.as_str())).collect();
    assert_eq!(
        got,
        [
            ("A", "class"),
            ("Count", "property"),
            ("Name", "property"),
            ("S", "property"),
            ("T", "property"),
            ("F", "property"),
            ("M", "method"),
            ("I", "interface"),
            ("P", "property"),
        ]
    );
}

#[test]
fn brace_initialized_fields_and_operators() {
    let s = syms(
        "class A { int[] a = { 1, 2 }; Action d = delegate { }; Dictionary<string,int> m = new() { [\"a\"] = 1 }; \
         public static bool operator ==(A x, A y) => true; public static bool operator !=(A x, A y) { return false; } \
         public static implicit operator int(A a) => 0; }",
    );
    for f in ["a", "d", "m"] {
        assert_eq!(find(&s, f).2, "field", "{f}");
    }
    assert_eq!(find(&s, "a").3, "int[] a = { 1, 2 };");
    let ops: Vec<_> = s
        .iter()
        .filter(|x| x.2 == "operator")
        .map(|x| x.0.as_str())
        .collect();
    assert_eq!(ops, ["operator ==", "operator !=", "operator int"]);
}

#[test]
fn unbalanced_preprocessor_branches_keep_later_symbols() {
    let src = "namespace N {\nclass A {\n#if DEBUG\n  void D1() {\n#else\n  void D1() { int q;\n#endif\n  }\n  void After() { }\n}\nclass B { }\n}\n";
    let s = syms(src);
    // Both `#if` branches are kept, so braces are unbalanced: `After` lands
    // in `D1`'s (unscanned) body, but the rest of the file is not lost.
    for n in ["N", "A", "D1", "B"] {
        find(&s, n);
    }
}

#[test]
fn verbatim_strings_do_not_confuse_braces() {
    let s = syms("class A { string s = @\"}{\"; void M() { var t = $\"{x}\"; } }");
    assert_eq!(s.len(), 3);
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("class"), Just("namespace"), Just("struct"), Just("enum"), Just("A"),
                Just("B"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"),
                Just(";"), Just("="), Just(">"), Just("<"), Just("=>"), Just("this"),
                Just("operator"), Just("event"), Just("const"), Just("delegate"),
                Just("\"s\""), Just("@\""), Just("//c\n"), Just("#if X\n"), Just("~"), Just(","),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = CSharpExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            // Spans start and end on token boundaries.
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
    }
}
