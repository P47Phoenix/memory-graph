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
    assert_eq!(k("Open"), (SymbolKind::Constant, "enum_member"));
    assert_eq!(k("Closed"), (SymbolKind::Constant, "enum_member"));
    // Method locals are not symbols.
    assert!(s.iter().all(|x| x.0 != "u"));
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

/// Run `f` on a thread with the indexer's 2 MB stack, so recursion that
/// grows with nesting depth overflows here as it would in production.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

/// #133: 5000 nested `namespace a {` used to overflow the stack.
#[test]
fn deep_nesting_is_capped_not_a_stack_overflow() {
    let n = 20_000;
    for (open, balanced) in [
        ("namespace a {", true),
        ("namespace a {", false),
        ("class A {", true),
        ("class A {", false),
        ("namespace a;", false),
    ] {
        let mut src = open.repeat(n);
        if balanced {
            src.push_str(&"}".repeat(n));
        }
        let ex = on_small_stack(move || CSharpExtractor.extract(&src));
        assert!(!ex.symbols.is_empty(), "{open}");
        assert!(
            ex.symbols.len() <= MAX_DEPTH,
            "{open}: {}",
            ex.symbols.len()
        );
        assert_eq!(ex.symbols[0].span.start, 0, "{open}");
    }
}

/// Nesting just under the cap is still reported in full.
#[test]
fn nesting_below_the_cap_is_unchanged() {
    let n = MAX_DEPTH - 1;
    let src = format!("{}{}", "namespace a {".repeat(n), "}".repeat(n));
    let ex = on_small_stack(move || CSharpExtractor.extract(&src));
    assert_eq!(ex.symbols.len(), n);
}

/// Nesting at the cap is reported in full; one level deeper drops only the
/// innermost namespace.
#[test]
fn nesting_at_and_past_the_cap() {
    for (n, expected) in [(MAX_DEPTH, MAX_DEPTH), (MAX_DEPTH + 1, MAX_DEPTH)] {
        let src = format!("{}{}", "namespace a {".repeat(n), "}".repeat(n));
        let ex = on_small_stack(move || CSharpExtractor.extract(&src));
        assert_eq!(ex.symbols.len(), expected, "n = {n}");
    }
}

/// Depth is restored after each body: many siblings are all scanned.
#[test]
fn siblings_do_not_accumulate_depth() {
    let n = MAX_DEPTH + 44;
    let src = format!("namespace N {{ {} }}", "class A { void M(){} } ".repeat(n));
    let ex = CSharpExtractor.extract(&src);
    let methods = ex.symbols.iter().filter(|s| s.name.ends_with("M")).count();
    assert_eq!(methods, n);
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
        parts in proptest::collection::vec(prop_oneof![Just("class"), Just("namespace"), Just("operator"), Just("A"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"), Just("="), Just("=>"), Just("<"), Just(">"), Just(","), Just("\"s"), Just("/*"), Just("<div>"), Just("int"), Just("get")], 0..60),
        code_first in any::<bool>(),
    ) {
        let soup = parts.join(" ");
        let code = "namespace N { class C { int P { get; set; } = 1; int[] a = { 1 }; public static C operator +(C a, C b) => a; void M() { } int F => 2; } }\n";
        let src = if code_first { format!("{code}{soup}") } else { format!("{soup}\n{code}") };
        let ex: &dyn Extractor = &CSharpExtractor;
            prop_assert_eq!(ex.extract(&src).symbols, without_memo(ex, &src), "{}", src);
    }
}

/// #143: reserved words are classed `keyword`; a contextual keyword (`var`)
/// and a verbatim identifier (`@class`) stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    let src = "public class C { void M() { var @class = 1; switch (1) { default: break; } } }";
    let toks = CSharpExtractor.extract(src).tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(class("public"), [TokenClass::Keyword]);
    assert_eq!(class("void"), [TokenClass::Keyword]);
    assert_eq!(class("default"), [TokenClass::Keyword]);
    // `@ class` with a space is no verbatim identifier.
    let spaced = CSharpExtractor.extract("x @ class").tokens;
    assert_eq!(spaced[2].class, TokenClass::Keyword);
    assert_eq!(class("var"), [TokenClass::Identifier]);
    assert_eq!(class("C"), [TokenClass::Identifier]);
    assert_eq!(
        class("class"),
        [TokenClass::Keyword, TokenClass::Identifier]
    );
    assert!(CSharpExtractor
        .version()
        .starts_with("csharp-scan-1+kw1+em1+tok"));
}

/// `local_symbols` (#72): (name, lang_kind, span text) of each local.
fn locals(src: &str) -> Vec<(String, String, String)> {
    let toks = tokenize_with(src, CSHARP_TOKENIZER);
    local_symbols(&toks)
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn names(src: &str) -> Vec<String> {
    locals(src).into_iter().map(|l| l.0).collect()
}

#[test]
fn local_declarations() {
    // Multi-declarator: one symbol, the whole statement.
    assert_eq!(
        locals("int a, b;"),
        [("a".into(), "local".into(), "int a, b;".into())]
    );
    // Statement start after `}`.
    assert_eq!(names("} int x = 1;"), ["x"]);
    assert_eq!(names("{ } int y;"), ["y"]);
    // A missing `;` before the enclosing `}`: the span ends at the name.
    assert_eq!(locals("{ int x = 1 }")[0].2, "int x");
    // A `;` inside brackets does not end the statement.
    assert_eq!(
        locals("Action a = () => { f(); };")[0].2,
        "Action a = () => { f(); };"
    );
    assert_eq!(
        locals("int[] v = new[] { g(x[0]) };")[0].2,
        "int[] v = new[] { g(x[0]) };"
    );
    // Nullable and array types.
    assert_eq!(names("int? q;"), ["q"]);
    assert_eq!(names("int[,] m;"), ["m"]);
    // Header guards: each needs its follower.
    assert_eq!(names("foreach (var p in q) { }"), ["p"]);
    assert!(names("foreach (var p of q) { }").is_empty());
    assert_eq!(names("catch (Exception ex) { }"), ["ex"]);
    assert!(names("catch (Exception ex when x) { }").is_empty());
    assert_eq!(names("for (int i = 0; i < n; i++) { }"), ["i"]);
    assert!(names("for (int i; i < n; i++) { }").is_empty());
    assert_eq!(names("using (var c = Open()) { }"), ["c"]);
    assert!(names("using (Foo c) { }").is_empty());
    // Not declarations.
    for src in [
        "return x;",
        "await y;",
        "a = b;",
        "x.y = z;",
        "Foo();",
        "a < b;",
    ] {
        assert!(names(src).is_empty(), "{src}");
    }
}

/// The tokenizer emits `>>` and `>>>` as single `>`s, so nested generics
/// close with plain `>` tokens.
#[test]
fn nested_generic_closers_are_single_tokens() {
    for src in ["a >> b", "a >>> b"] {
        let toks = tokenize_with(src, CSHARP_TOKENIZER);
        assert!(
            toks.iter().all(|t| t.text != ">>" && t.text != ">>>"),
            "{toks:?}"
        );
    }
    assert_eq!(names("List<List<int>> x;"), ["x"]);
    assert_eq!(names("A<B<C<int>>> y = z;"), ["y"]);
    assert!(names("A<B<int> y;").is_empty());
}

/// `(name, kind, lang_kind, span text)` of the symbols whose span lies
/// inside `outer`'s span (the store nests by span).
fn members_of(src: &str, outer: &str) -> Vec<(String, String)> {
    let ex = CSharpExtractor.extract(src);
    assert_nested(&ex);
    let o = ex
        .symbols
        .iter()
        .find(|s| s.name == outer)
        .expect("outer symbol")
        .span;
    ex.symbols
        .iter()
        .filter(|s| s.name != outer && o.start <= s.span.start && s.span.end <= o.end)
        .map(|s| {
            assert_eq!(s.kind, SymbolKind::Constant, "{}", s.name);
            assert_eq!(s.lang_kind.as_deref(), Some("enum_member"));
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
fn enum_members_simple() {
    let src = "public enum Color { Red, Green, Blue }";
    assert_eq!(find(&syms(src), "Color").2, "enum");
    assert_eq!(
        members_of(src, "Color"),
        pairs(&[("Red", "Red"), ("Green", "Green"), ("Blue", "Blue")])
    );
}

#[test]
fn enum_members_initializers_attributes_and_comments() {
    let src = "[Flags]\nenum F : byte\n{\n    None = 0, // none\n    /* a */ A = 1 << 2,\n    [Obsolete(\"x, y\")] B = A | C,\n    C = (int)(1 + 2),\n    D = Helper.Get(1, 2),\n}\n";
    assert_eq!(
        members_of(src, "F"),
        pairs(&[
            ("None", "None = 0"),
            ("A", "A = 1 << 2"),
            ("B", "[Obsolete(\"x, y\")] B = A | C"),
            ("C", "C = (int)(1 + 2)"),
            ("D", "D = Helper.Get(1, 2)"),
        ])
    );
}

#[test]
fn enum_members_empty_and_nested() {
    assert!(members_of("enum E { }", "E").is_empty());
    assert!(members_of("enum E { , }", "E").is_empty());
    let src = "class Outer { int x; public enum Inner { X, Y = 2 } void M() { } }";
    assert_eq!(
        members_of(src, "Inner"),
        pairs(&[("X", "X"), ("Y", "Y = 2")])
    );
    let s = syms(src);
    assert_eq!(find(&s, "M").1, SymbolKind::Method);
    assert_eq!(find(&s, "x").2, "field");
}

#[test]
fn enum_members_in_member_symbols() {
    let toks = tokenize_with("enum E { A, B }", CSHARP_TOKENIZER);
    let names: Vec<_> = member_symbols(&toks)
        .into_iter()
        .map(|s| (s.name, s.lang_kind.unwrap()))
        .collect();
    assert_eq!(
        names,
        [("E", "enum"), ("A", "enum_member"), ("B", "enum_member")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );
}
