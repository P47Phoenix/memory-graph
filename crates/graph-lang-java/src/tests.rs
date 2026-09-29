use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = JavaExtractor.extract(src);
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

fn contains(outer: &Sym, inner: &Sym, src: &str) -> bool {
    let o = src.find(&outer.3).unwrap();
    let i = src.find(&inner.3).unwrap();
    o <= i && i + inner.3.len() <= o + outer.3.len()
}

const SRC: &str = r#"// header
package io.acme.billing;

import java.util.*;

/** Docs. */
@Service
@RequestMapping(path = "/x")
public final class Invoice<T extends Comparable<T>> extends Base implements Api {
    public static final int MAX = 10;
    private final Map<String, List<Integer>> lines = new HashMap<>();
    int a, b;
    String[] names = { "}" };
    private Runnable r = new Runnable() { public void run() { } };
    static { init(); }

    @Autowired
    public Invoice(int n) { super(n); }

    public <U> List<U> map(Function<T, U> f) throws IOException {
        String s = """
            } text block {
            """;
        return null;
    }

    abstract void later();

    enum State { OPEN("o") { void x() {} }, CLOSED("c"); final String code; State(String c) { code = c; } }

    interface Listener { int LIMIT = 3; void on(Event e); default void off() {} }

    record Point(int x, int y) implements Shape { Point { } double len() { return 0; } }
}

@interface Marker { String value() default ""; }
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("io.acme.billing"), (SymbolKind::Module, "package"));
    assert_eq!(k("Invoice"), (SymbolKind::Type, "class"));
    assert_eq!(k("MAX"), (SymbolKind::Constant, "constant"));
    assert_eq!(k("lines"), (SymbolKind::Variable, "field"));
    assert_eq!(k("a"), (SymbolKind::Variable, "field"));
    assert_eq!(k("names"), (SymbolKind::Variable, "field"));
    assert_eq!(k("r"), (SymbolKind::Variable, "field"));
    assert_eq!(k("map"), (SymbolKind::Method, "method"));
    assert_eq!(k("later"), (SymbolKind::Method, "method"));
    assert_eq!(k("State"), (SymbolKind::Type, "enum"));
    assert_eq!(k("code"), (SymbolKind::Variable, "field"));
    assert_eq!(k("Listener"), (SymbolKind::Type, "interface"));
    assert_eq!(k("LIMIT"), (SymbolKind::Constant, "constant"));
    assert_eq!(k("on"), (SymbolKind::Method, "method"));
    assert_eq!(k("off"), (SymbolKind::Method, "method"));
    assert_eq!(k("Point"), (SymbolKind::Type, "record"));
    assert_eq!(k("len"), (SymbolKind::Method, "method"));
    assert_eq!(k("Marker"), (SymbolKind::Type, "annotation"));
    assert_eq!(k("value"), (SymbolKind::Method, "method"));
    let invoices: Vec<_> = s
        .iter()
        .filter(|x| x.0 == "Invoice")
        .map(|x| x.2.as_str())
        .collect();
    assert_eq!(invoices, ["class", "constructor"]);
    let states: Vec<_> = s
        .iter()
        .filter(|x| x.0 == "State")
        .map(|x| x.2.as_str())
        .collect();
    assert_eq!(states, ["enum", "constructor"]);
    // Not symbols: second declarator, enum constants, anonymous class
    // members, initializer blocks, imports, locals.
    for n in [
        "b",
        "OPEN",
        "CLOSED",
        "run",
        "x",
        "s",
        "java.util.*",
        "static",
    ] {
        assert!(s.iter().all(|x| x.0 != n), "{n} is a symbol: {s:#?}");
    }
    // Spans.
    assert!(find(&s, "Invoice")
        .3
        .starts_with("@Service\n@RequestMapping(path = \"/x\")\npublic final class Invoice<"));
    assert_eq!(
        find(&s, "lines").3,
        "private final Map<String, List<Integer>> lines = new HashMap<>();"
    );
    assert_eq!(find(&s, "a").3, "int a, b;");
    assert_eq!(
        find(&s, "r").3,
        "private Runnable r = new Runnable() { public void run() { } };"
    );
    assert_eq!(find(&s, "later").3, "abstract void later();");
    assert!(find(&s, "map").3.ends_with("return null;\n    }"));
    let ctor = s.iter().find(|x| x.2 == "constructor").unwrap();
    assert_eq!(
        ctor.3,
        "@Autowired\n    public Invoice(int n) { super(n); }"
    );
    // The package spans the rest of the file; everything nests in it.
    let pkg = find(&s, "io.acme.billing");
    assert!(pkg.3.starts_with("package io.acme.billing;"));
    assert!(pkg.3.ends_with("default \"\"; }"));
    for x in &s {
        assert!(contains(pkg, x, SRC), "{} outside the package", x.0);
    }
    assert!(contains(find(&s, "Invoice"), find(&s, "Listener"), SRC));
    assert!(contains(find(&s, "Listener"), find(&s, "LIMIT"), SRC));
}

#[test]
fn no_package_and_top_level_types() {
    let s = syms("import a.B;\npublic class A { void m() { class Local {} } }\nclass B {}\n");
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["A", "m", "B"]);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}class Ü {\n  int é;\n}";
    let ex = JavaExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 3));
    assert_eq!(&src[f.span.start as usize..f.span.end as usize], "int é;");
    assert_eq!(ex.symbols[0].span.start_col, 1);
    assert_eq!(ex.symbols[0].name, "Ü");
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "class A {",
        "class A { void m( { }",
        "}}} class",
        "package ;",
        "package a.b",
        "class A { int x = ; } }",
        "class A<T { }",
        "@",
        "@interface",
        "enum E { A, B",
        "class A { void m() { } int }",
        "[",
        "",
    ] {
        let ex = JavaExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
    // An unclosed class still yields later members.
    let s = syms("class A {\n  void m() { }\n  int f;\n");
    for n in ["A", "m", "f"] {
        find(&s, n);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("class"), Just("package"), Just("interface"), Just("enum"), Just("record"),
                Just("A"), Just("B"), Just("{"), Just("}"), Just("("), Just(")"), Just("["),
                Just("]"), Just(";"), Just("="), Just(">"), Just("<"), Just("@"), Just(","),
                Just("static"), Just("final"), Just("import"), Just("."),
                Just("\"s\""), Just("\"\"\""), Just("//c\n"), Just("/*"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = JavaExtractor.extract(&src);
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
