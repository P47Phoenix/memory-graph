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
    assert_eq!(k("OPEN"), (SymbolKind::Constant, "enum_constant"));
    assert_eq!(find(&s, "OPEN").3, "OPEN(\"o\") { void x() {} }");
    assert_eq!(k("CLOSED"), (SymbolKind::Constant, "enum_constant"));
    // Not symbols: second declarator, enum constant body methods, anonymous
    // class members, initializer blocks, imports, locals.
    for n in ["b", "run", "x", "s", "java.util.*", "static"] {
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

/// Runs `f` on a thread with the indexer's 2 MB stack, so a recursion that
/// grows with nesting depth overflows here as it would in production.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn deep_nesting_is_capped_not_a_stack_overflow() {
    let n = 20_000;
    for balanced in [true, false] {
        let mut src = "class A {".repeat(n);
        if balanced {
            src.push_str(&"}".repeat(n));
        }
        let ex = on_small_stack(move || JavaExtractor.extract(&src));
        assert!(!ex.symbols.is_empty());
        assert!(ex.symbols.len() <= MAX_DEPTH + 1);
        assert_eq!(ex.symbols[0].span.start, 0);
    }
}

#[test]
fn record_constructors_annotation_constants_and_c_style_arrays() {
    let s = syms(
        "record P(int x) {\n  P { check(x); }\n  public P(String s) { this(1); }\n}\n\
         @interface M { int LIMIT = 3; String v(); }\n\
         class C { int x[]; String[] a[] = {}; }\n",
    );
    let ctors: Vec<_> = s
        .iter()
        .filter(|x| x.2 == "constructor")
        .map(|x| x.3.as_str())
        .collect();
    assert_eq!(
        ctors,
        ["P { check(x); }", "public P(String s) { this(1); }"]
    );
    assert_eq!(
        (find(&s, "LIMIT").1, find(&s, "LIMIT").2.as_str()),
        (SymbolKind::Constant, "constant")
    );
    assert_eq!(find(&s, "x").3, "int x[];");
    assert_eq!(find(&s, "x").2, "field");
    assert_eq!(find(&s, "a").3, "String[] a[] = {};");
    // Initializer blocks are still not symbols.
    let s = syms("class A { static { } { } }");
    assert_eq!(s.len(), 1);
}

thread_local! {
    /// See `memo_on`.
    pub(crate) static MEMO: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Symbols of `src` from `ex` with the scan memos off.
fn without_memo(ex: &dyn Extractor, src: &str) -> Vec<SymbolDecl> {
    MEMO.with(|m| m.set(false));
    let out = ex.extract(src).symbols;
    MEMO.with(|m| m.set(true));
    out
}

proptest! {
    /// The memos never change what is found: a random prefix (which fills
    /// them with failed and successful scans, often unterminated) followed
    /// by real code gives the same symbols with the memos on and off.
    #[test]
    fn memos_do_not_change_symbols(
        parts in proptest::collection::vec(prop_oneof![Just("class"), Just("interface"), Just("A"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"), Just("="), Just("<"), Just(">"), Just(","), Just("\"s"), Just("/*"), Just("<div>"), Just("int"), Just("@A"), Just("new")], 0..60),
        code_first in any::<bool>(),
    ) {
        let soup = parts.join(" ");
        let code = "package p;\nclass C { int[] a = { 1 }; Runnable r = new Runnable() { public void run() {} }; void m() {} int x = f(1); }\n";
        let src = if code_first { format!("{code}{soup}") } else { format!("{soup}\n{code}") };
        let ex: &dyn Extractor = &JavaExtractor;
            prop_assert_eq!(ex.extract(&src).symbols, without_memo(ex, &src), "{}", src);
    }
}
/// `semi_after` answers from its memo exactly as a fresh scan would, for
/// starts inside a range it already walked (the extractor itself only
/// re-reads failed walks, so this checks the memo directly).
#[test]
fn semi_after_memo_matches_fresh_scans() {
    let tokens = JavaExtractor
        .extract("a = f(1) + { 2 } ; b = c } d = e ;")
        .tokens;
    let code = code_index(&tokens, &[TokenClass::Comment]);
    let s = Scanner {
        tokens: &tokens,
        code: &code,
        out: Vec::new(),
        depth: 0,
        closes: code_close_table(&tokens, &code),
        header_fails: RefCell::default(),
        semis: RefCell::default(),
    };
    let hi = code.len();
    let fresh: Vec<_> = (0..hi)
        .map(|c| {
            s.semis.borrow_mut().clear();
            s.semi_after(c, hi)
        })
        .collect();
    s.semis.borrow_mut().clear();
    for order in [(0..hi).collect::<Vec<_>>(), (0..hi).rev().collect()] {
        for c in order {
            assert_eq!(s.semi_after(c, hi), fresh[c], "from {c}");
        }
    }
    assert!(fresh.iter().any(Option::is_some) && fresh.iter().any(Option::is_none));
}

/// #143: reserved words are classed `keyword`; contextual words stay
/// identifiers (this language has no escaped identifiers).
#[test]
fn keywords_are_classed_keyword() {
    let toks = JavaExtractor
        .extract("public class C { void m(int _) { var record = null; switch (1) { default: } } }")
        .tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    for w in ["public", "class", "void", "null", "_", "default", "switch"] {
        assert_eq!(class(w), [graph_core::TokenClass::Keyword], "{w}");
    }
    for w in ["var", "record", "C"] {
        assert_eq!(class(w), [graph_core::TokenClass::Identifier], "{w}");
    }
    assert!(JavaExtractor
        .version()
        .starts_with("java-scan-1+kw1+em1+tok"));
}

/// `(name, span text)` of the `enum_constant` symbols nested in `outer`.
fn constants_of(src: &str, outer: &str) -> Vec<(String, String)> {
    let ex = JavaExtractor.extract(src);
    assert_nested(&ex);
    let o = ex
        .symbols
        .iter()
        .find(|s| s.name == outer)
        .expect("outer symbol")
        .span;
    ex.symbols
        .iter()
        .filter(|s| s.lang_kind.as_deref() == Some("enum_constant"))
        .filter(|s| o.start <= s.span.start && s.span.end <= o.end)
        .map(|s| {
            assert_eq!(s.kind, SymbolKind::Constant, "{}", s.name);
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name.clone(), text)
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn enum_constants_simple_and_trailing_comma() {
    let want = pairs(&[("RED", "RED"), ("GREEN", "GREEN"), ("BLUE", "BLUE")]);
    for src in [
        "enum Color { RED, GREEN, BLUE }",
        "enum Color { RED, GREEN, BLUE, }",
        "enum Color { RED, GREEN, BLUE; }",
        "enum Color { RED, GREEN, BLUE,; }",
    ] {
        assert_eq!(constants_of(src, "Color"), want, "{src}");
    }
}

#[test]
fn enum_constants_arguments_bodies_and_annotations() {
    let src = "enum Op {\n    // c\n    PLUS(\"+\", 1) { int apply(int a) { return a; } },\n    @Deprecated @a.b.C(x = {1, 2}) MINUS(\"-\"),\n    /** doc */ TIMES;\n    private final String sym;\n    Op(String s, int p) { sym = s; }\n    Op(String s) { this(s, 0); }\n    abstract int apply(int a);\n}\n";
    assert_eq!(
        constants_of(src, "Op"),
        pairs(&[
            ("PLUS", "PLUS(\"+\", 1) { int apply(int a) { return a; } }"),
            ("MINUS", "@Deprecated @a.b.C(x = {1, 2}) MINUS(\"-\")"),
            ("TIMES", "TIMES"),
        ])
    );
    let s = syms(src);
    // Members after the `;` are unchanged; the constant body's method is
    // not a symbol (constant bodies are not scanned).
    assert_eq!(find(&s, "sym").2, "field");
    assert_eq!(s.iter().filter(|x| x.2 == "constructor").count(), 2);
    let applies: Vec<_> = s.iter().filter(|x| x.0 == "apply").collect();
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].3, "abstract int apply(int a);");
}

#[test]
fn enum_constants_empty_and_nested() {
    assert!(constants_of("enum E { }", "E").is_empty());
    assert!(constants_of("enum E { ; }", "E").is_empty());
    assert!(constants_of("enum E { , }", "E").is_empty());
    let src = "class Outer { int x; enum Inner { X, Y(2) } void m() { } }";
    assert_eq!(
        constants_of(src, "Inner"),
        pairs(&[("X", "X"), ("Y", "Y(2)")])
    );
    let s = syms(src);
    assert_eq!(find(&s, "m").1, SymbolKind::Method);
    assert_eq!(find(&s, "x").2, "field");
    let src = "class A { interface I { enum E { P } } }";
    assert_eq!(constants_of(src, "E"), pairs(&[("P", "P")]));
}

#[test]
fn enum_constants_differing_by_case_are_distinct() {
    assert_eq!(
        constants_of("enum E { A, a(2) }", "E"),
        pairs(&[("A", "A"), ("a", "a(2)")])
    );
}

#[test]
fn enum_constants_unbalanced_drop_the_rest() {
    let s = syms("enum E { A, B(2, ; int f; }");
    assert_eq!(find(&s, "A").2, "enum_constant");
    assert!(s.iter().all(|x| x.0 != "B" && x.0 != "f"), "{s:?}");
    // An unclosed enum body still yields its constants.
    let s = syms("enum E { A, B");
    assert_eq!(find(&s, "B").3, "B");
}
