use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms_with(ex: &dyn Extractor, src: &str) -> Vec<Sym> {
    let e = ex.extract(src);
    assert!(!e.has_errors);
    assert_nested(&e);
    e.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn c(src: &str) -> Vec<Sym> {
    syms_with(&CExtractor, src)
}

fn cpp(src: &str) -> Vec<Sym> {
    syms_with(&CppExtractor, src)
}

fn find<'a>(s: &'a [Sym], name: &str, lang: &str) -> &'a Sym {
    s.iter()
        .find(|x| x.0 == name && x.2 == lang)
        .unwrap_or_else(|| panic!("{name} ({lang}) not found in {s:#?}"))
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

const C_SRC: &str = r#"#include <stdio.h>
#define MAX 10
#define SQUARE(x) \
    ((x) * (x))
/* comment { */
struct point { int x; int y; struct inner { int z; } in; };
union value { int i; float f; };
enum color { RED, GREEN = 2 };
typedef struct { int a; } anon_t;
typedef struct node { struct node *next; } node_t;
typedef int (*callback)(int, void *);
typedef unsigned long ulong;
int counter = 0;
static const char *names[] = { "a", "b" };
const int LIMIT = 5;
extern int shared;
int prototype(int a);
struct point origin = { 0, 0 };
static int add(int a, int b)
{
    int local = a + b;
    return local;
}
struct point *make(void) { return 0; }
API(const char *) api_fn(void) { return 0; }
"#;

#[test]
fn c_declarations() {
    let s = c(C_SRC);
    let k = |n: &str, l: &str| find(&s, n, l).1;
    assert_eq!(k("MAX", "macro"), SymbolKind::Other);
    assert_eq!(
        find(&s, "SQUARE", "macro").3,
        "#define SQUARE(x) \\\n    ((x) * (x))"
    );
    assert_eq!(k("point", "struct"), SymbolKind::Type);
    assert_eq!(
        find(&s, "point", "struct").3,
        "struct point { int x; int y; struct inner { int z; } in; };"
    );
    assert_eq!(k("inner", "struct"), SymbolKind::Type);
    assert_eq!(k("value", "union"), SymbolKind::Type);
    assert_eq!(k("color", "enum"), SymbolKind::Type);
    assert_eq!(
        find(&s, "anon_t", "typedef").3,
        "typedef struct { int a; } anon_t;"
    );
    assert_eq!(
        find(&s, "node", "struct").3,
        "struct node { struct node *next; }"
    );
    assert_eq!(k("node_t", "typedef"), SymbolKind::Type);
    assert_eq!(k("callback", "typedef"), SymbolKind::Type);
    assert_eq!(k("ulong", "typedef"), SymbolKind::Type);
    assert_eq!(k("counter", "variable"), SymbolKind::Variable);
    assert_eq!(k("names", "variable"), SymbolKind::Variable);
    assert_eq!(k("LIMIT", "const"), SymbolKind::Constant);
    assert_eq!(k("origin", "variable"), SymbolKind::Variable);
    assert_eq!(k("add", "function"), SymbolKind::Function);
    assert!(find(&s, "add", "function").3.starts_with("static int add("));
    assert!(find(&s, "add", "function").3.ends_with('}'));
    assert_eq!(k("make", "function"), SymbolKind::Function);
    // A macro-wrapped return type does not name the function.
    assert_eq!(k("api_fn", "function"), SymbolKind::Function);
    assert!(find(&s, "api_fn", "function")
        .3
        .starts_with("API(const char *) api_fn"));
    assert!(s.iter().all(|x| x.0 != "API"));
    // #269: enumerators are symbols.
    assert_eq!(k("RED", "enumerator"), SymbolKind::Constant);
    assert_eq!(find(&s, "GREEN", "enumerator").3, "GREEN = 2");
    // Fields, locals, prototypes, externs and includes are not symbols.
    for n in ["x", "y", "z", "local", "prototype", "shared", "stdio", "a"] {
        assert!(s.iter().all(|x| x.0 != n), "{n} in {s:#?}");
    }
}

const CPP_SRC: &str = r#"#pragma once
namespace acme::billing {
template <typename T, typename U = std::vector<T>>
class Invoice : public Base<T>, private Other {
public:
    Invoice(int n) : count_(n), items_{1, 2} {}
    ~Invoice();
    virtual void run() const override;
    static constexpr int kMax = 4;
    T get(U u) { return u.front(); }
    bool operator==(const Invoice &o) const = default;
    Invoice &operator=(const Invoice &) = delete;
    int operator()(int x) { return x; }
    explicit operator bool() const { return true; }
    std::function<void(int)> callback;
    std::shared_ptr<const Invoice> self_;
    const Invoice &ref_;
private:
    int count_ = 0;
    std::vector<int> items_;
    struct Line { double amount; };
    enum class State : int { Open, Closed };
    using Id = long;
    friend class Helper;
};
Invoice::~Invoice() {}
template <typename T>
void Invoice<T>::run() const { if (x) { } }
int free_fn(int a) { return a; }
std::ostream &operator<<(std::ostream &os, const Invoice<int> &i) { return os; }
namespace {
int hidden = 1;
}
}
extern "C" {
int c_api(void) { return 0; }
}
"#;

#[test]
fn cpp_declarations() {
    let s = cpp(CPP_SRC);
    let k = |n: &str, l: &str| find(&s, n, l).1;
    assert_eq!(k("acme::billing", "namespace"), SymbolKind::Module);
    assert!(find(&s, "acme::billing", "namespace").3.ends_with('}'));
    assert_eq!(k("Invoice", "class"), SymbolKind::Type);
    assert!(find(&s, "Invoice", "class")
        .3
        .starts_with("template <typename T, typename U = std::vector<T>>\nclass Invoice"));
    assert!(find(&s, "Invoice", "class").3.ends_with("};"));
    assert_eq!(k("Invoice", "constructor"), SymbolKind::Method);
    assert_eq!(
        find(&s, "Invoice", "constructor").3,
        "Invoice(int n) : count_(n), items_{1, 2} {}"
    );
    let dtors: Vec<_> = s.iter().filter(|x| x.2 == "destructor").collect();
    assert_eq!(dtors.len(), 2, "{dtors:#?}");
    assert_eq!(dtors[1].3, "Invoice::~Invoice() {}");
    assert!(dtors.iter().all(|d| d.1 == SymbolKind::Method));
    let runs: Vec<_> = s.iter().filter(|x| x.0 == "run").collect();
    assert_eq!(runs.len(), 2);
    assert!(runs
        .iter()
        .all(|r| r.1 == SymbolKind::Method && r.2 == "method"));
    assert!(runs[1]
        .3
        .starts_with("template <typename T>\nvoid Invoice<T>::run()"));
    assert_eq!(k("kMax", "const"), SymbolKind::Constant);
    assert_eq!(k("get", "method"), SymbolKind::Method);
    assert_eq!(k("operator==", "operator"), SymbolKind::Method);
    assert_eq!(k("operator=", "operator"), SymbolKind::Method);
    assert_eq!(k("operator()", "operator"), SymbolKind::Method);
    assert_eq!(k("operator bool", "operator"), SymbolKind::Method);
    assert_eq!(k("operator<<", "operator"), SymbolKind::Function);
    assert_eq!(k("callback", "field"), SymbolKind::Variable);
    assert_eq!(k("self_", "field"), SymbolKind::Variable);
    assert_eq!(k("ref_", "field"), SymbolKind::Variable);
    assert_eq!(k("count_", "field"), SymbolKind::Variable);
    assert_eq!(k("items_", "field"), SymbolKind::Variable);
    assert_eq!(k("Line", "struct"), SymbolKind::Type);
    assert_eq!(k("amount", "field"), SymbolKind::Variable);
    assert_eq!(k("State", "enum"), SymbolKind::Type);
    assert_eq!(k("Id", "using"), SymbolKind::Type);
    assert_eq!(k("free_fn", "function"), SymbolKind::Function);
    assert_eq!(k("hidden", "variable"), SymbolKind::Variable);
    assert_eq!(k("c_api", "function"), SymbolKind::Function);
    assert_eq!(find(&s, "Open", "enumerator").1, SymbolKind::Constant);
    for n in ["Helper", "x", "public", "void"] {
        assert!(s.iter().all(|x| x.0 != n), "{n} in {s:#?}");
    }
}

#[test]
fn standalone_macro_lines_do_not_name_declarations() {
    let src = "DIAG_PUSH\nIGNORE_WARNING(\"-Wx\")\nclass Iter { };\nIGNORE_WARNING(\"-Wy\")\nEXPORT_ATTR\nstd::string name() { return x; }\nTEST(Suite, Case) {\n}\n";
    let s = cpp(src);
    assert_eq!(find(&s, "Iter", "class").3, "class Iter { };");
    assert!(find(&s, "name", "function")
        .3
        .starts_with("EXPORT_ATTR\nstd::string name()"));
    find(&s, "TEST", "function");
    assert!(s
        .iter()
        .all(|x| x.0 != "IGNORE_WARNING" && x.0 != "DIAG_PUSH"));
}

#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    let e = CppExtractor.extract(&"namespace a { ".repeat(20000));
    assert!(!e.symbols.is_empty() && e.symbols.len() <= MAX_DEPTH);
    let e = CExtractor.extract(&"struct S { ".repeat(20000));
    assert!(e.symbols.len() <= MAX_DEPTH);
    let src = format!("int {}fp{};", "(*".repeat(20000), ")".repeat(20000));
    let e = CExtractor.extract(&src);
    assert!(!e.has_errors);
}

#[test]
fn only_the_first_preprocessor_branch_is_scanned() {
    let src = "void f(int a) {\n#ifdef X\n  if (a) {\n#else\n  if (!a) {\n#endif\n    g();\n  }\n}\n#if Y\n#define M 1\n#elif Z\n#define M 2\n#else\nint only_else;\n#endif\nint after = 1;\n";
    let s = c(src);
    assert!(find(&s, "f", "function").3.ends_with("}\n}"), "{s:#?}");
    find(&s, "after", "variable");
    assert_eq!(s.iter().filter(|x| x.0 == "M").count(), 2);
    assert!(s.iter().all(|x| x.0 != "only_else"));
}

#[test]
fn cpp_in_a_c_header_is_scanned_as_cpp() {
    let src = "class Foo {\npublic:\n  void bar() {}\n  ~Foo();\n};\n";
    let s = c(src);
    assert_eq!(find(&s, "Foo", "class").1, SymbolKind::Type);
    assert_eq!(find(&s, "bar", "method").1, SymbolKind::Method);
    assert_eq!(find(&s, "~Foo", "destructor").1, SymbolKind::Method);
    assert!(s.iter().all(|x| x.2 != "variable"));
    assert_eq!(CExtractor.language(), "c");
    // Plain C headers stay C (`class` is an identifier there).
    assert_eq!(c("int class;\n")[0].2, "variable");
}

#[test]
fn pointer_declarators_and_forward_declarations() {
    let s = c("void (*signal(int sig, void (*h)(int)))(int) { return 0; }\nint (*fp)(int);\nstruct S;\nstruct T t;\n");
    assert_eq!(find(&s, "signal", "function").1, SymbolKind::Function);
    assert_eq!(find(&s, "fp", "variable").1, SymbolKind::Variable);
    find(&s, "t", "variable");
    assert!(s.iter().all(|x| x.0 != "void" && x.0 != "S" && x.0 != "h"));
    let s = cpp("enum class E : int;\nstruct S;\nclass C;\ntemplate <class T> struct U;\n");
    assert!(s.is_empty(), "{s:#?}");
    let s = cpp("namespace A { struct B { void f(); }; }\nvoid A::B::f() {}\n");
    let f: Vec<_> = s.iter().filter(|x| x.0 == "f").collect();
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].2, "method");
}

#[test]
fn h_files_stay_c() {
    assert_eq!(CExtractor.extensions(), ["c", "h"]);
    assert!(!CppExtractor.extensions().contains(&"h"));
    // `class` is an ordinary identifier in C.
    let s = c("int class = 1;\n");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].0, "class");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}struct Ü {\n  int é;\n};\nint é2 = 1;";
    let ex = CExtractor.extract(src);
    assert_eq!(ex.symbols[0].span.start_col, 1);
    let v = ex.symbols.iter().find(|s| s.name == "é2").unwrap();
    assert_eq!((v.span.start_line, v.span.start_col), (4, 1));
    assert_eq!(
        &src[v.span.start as usize..v.span.end as usize],
        "int é2 = 1;"
    );
    let ex = CppExtractor.extract("class Ä { void ö() {} };");
    let m = ex.symbols.iter().find(|s| s.name == "ö").unwrap();
    assert_eq!(m.span.start_col, 11);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "struct A {",
        "int f( { }",
        "}}} struct",
        "namespace { ",
        "class A { int x = ; } }",
        "template <class T",
        "#define",
        "# \\",
        "operator",
        "A::~() {}",
        "typedef",
        "extern \"C\" {",
        "<<<>>> ( ;",
        "",
    ] {
        for ex in [&CExtractor as &dyn Extractor, &CppExtractor] {
            let e = ex.extract(src);
            assert!(!e.has_errors, "{src}");
            assert_nested(&e);
        }
    }
}

#[test]
fn unbalanced_preprocessor_branches_keep_later_symbols() {
    let src =
        "int a;\n#if X\nvoid f() {\n#else\nvoid f() { int q;\n#endif\n}\nstruct S { int x; };\n";
    let s = c(src);
    for (n, l) in [("a", "variable"), ("f", "function")] {
        find(&s, n, l);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("struct"), Just("class"), Just("namespace"), Just("enum"), Just("typedef"),
                Just("template"), Just("operator"), Just("A"), Just("B"), Just("{"), Just("}"),
                Just("("), Just(")"), Just("["), Just("]"), Just(";"), Just("="), Just(">"),
                Just("<"), Just(":"), Just("::"), Just("~"), Just("*"), Just(","), Just("\"s\""),
                Just("//c\n"), Just("\n#define M 1\n"), Just("\n#if X\n"), Just("\\\n"),
                Just("extern"), Just("public"), Just("using"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        for ex in [&CExtractor as &dyn Extractor, &CppExtractor] {
            let e = ex.extract(&src);
            prop_assert!(!e.has_errors);
            for s in &e.symbols {
                prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
                prop_assert!(e.tokens.iter().any(|t| t.span.start == s.span.start));
                prop_assert!(e.tokens.iter().any(|t| t.span.end == s.span.end));
                prop_assert!(!s.name.is_empty());
            }
            assert_nested(&e);
        }
    }
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
        parts in proptest::collection::vec(prop_oneof![Just("struct"), Just("class"), Just("namespace"), Just("template"), Just("operator"), Just("A"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"), Just("="), Just("<"), Just(">"), Just(":"), Just("::"), Just(","), Just("\"s"), Just("/*"), Just("<div>"), Just("int"), Just("f")], 0..60),
        code_first in any::<bool>(),
    ) {
        let soup = parts.join(" ");
        let code = "struct S { int a; } s = { 1 };\ntemplate <class T> class V : public B<T> { V() : x{1} {} int f(int a) const; operator int(); };\nnamespace n { int g(void) { return 0; } }\nint (*fp)(int) = 0;\n";
        let src = if code_first { format!("{code}{soup}") } else { format!("{soup}\n{code}") };
        for ex in [&CExtractor as &dyn Extractor, &CppExtractor] {
            prop_assert_eq!(ex.extract(&src).symbols, without_memo(ex, &src), "{}", src);
        }
    }
}
/// A failed header scan passes `b` with `params` and `init_list` set; the
/// resync from `:` reaches `b` with neither, where `b { }` is a body. The
/// failure memo must key on the flags, not just the position.
#[test]
fn header_failure_memo_keys_on_flags() {
    for src in [
        "A ( ) : b { } }",
        "A ( ) : struct S { } }",
        "A ( ) :\nstruct S { int x; } }",
        "operator = { } }",
    ] {
        for ex in [&CExtractor as &dyn Extractor, &CppExtractor] {
            assert_eq!(ex.extract(src).symbols, without_memo(ex, src), "{src}");
        }
    }
}

/// #143: reserved words are classed `keyword`, in C and in C++ (with the
/// C++ list); preprocessor directive names stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    use graph_core::TokenClass::{Identifier, Keyword};
    let class = |toks: &[graph_core::TokenDecl], text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    let c = CExtractor
        .extract(
            "#if X
static int f(void) { return sizeof(int); }
#else
int class;
#endif
",
        )
        .tokens;
    for w in ["static", "int", "void", "return", "sizeof"] {
        assert!(class(&c, w).iter().all(|k| *k == Keyword), "{w}");
    }
    for w in ["if", "else", "endif", "class", "f", "X"] {
        assert_eq!(class(&c, w), [Identifier], "{w}");
    }
    let cpp = CppExtractor
        .extract(
            "class A { public: virtual void f() override; }; int x = not_eq;
",
        )
        .tokens;
    for w in ["class", "public", "virtual", "void", "int", "not_eq"] {
        assert_eq!(class(&cpp, w), [Keyword], "{w}");
    }
    for w in ["override", "A", "f", "x"] {
        assert_eq!(class(&cpp, w), [Identifier], "{w}");
    }
    assert!(CExtractor.version().starts_with("c-scan-1+kw1+em1+tok"));
    assert!(CppExtractor.version().starts_with("cpp-scan-1+kw1+em1+tok"));
}

/// #143: every listed word, written bare, is classed `keyword`.
#[test]
fn every_c_keyword_is_classed_keyword() {
    for w in C_KEYWORDS {
        let toks = CExtractor.extract(w).tokens;
        assert_eq!(toks.len(), 1, "{w}");
        assert_eq!(toks[0].class, graph_core::TokenClass::Keyword, "{w}");
    }
}

/// #143: every listed word, written bare, is classed `keyword`.
#[test]
fn every_cpp_keyword_is_classed_keyword() {
    for w in CPP_KEYWORDS {
        let toks = CppExtractor.extract(w).tokens;
        assert_eq!(toks.len(), 1, "{w}");
        assert_eq!(toks[0].class, graph_core::TokenClass::Keyword, "{w}");
    }
}

/// #143: a `.h` header sniffed as C++ gets the C++ list, so `class` is a
/// keyword there (it is an identifier in plain C).
#[test]
fn header_sniffed_as_cpp_uses_the_cpp_keywords() {
    let toks = CExtractor
        .extract(
            "namespace X {
class A {};
}
",
        )
        .tokens;
    let class: Vec<_> = toks
        .iter()
        .filter(|t| t.text == "class")
        .map(|t| t.class)
        .collect();
    assert_eq!(class, [graph_core::TokenClass::Keyword]);
}

/// `(name, span text)` of the `enumerator` symbols, in source order.
fn enumerators(s: &[Sym]) -> Vec<(String, String)> {
    s.iter()
        .filter(|x| x.2 == "enumerator")
        .map(|x| {
            assert_eq!(x.1, SymbolKind::Constant, "{}", x.0);
            (x.0.clone(), x.3.clone())
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// The symbol whose span most tightly contains the enumerator `name`.
fn parent_of(src: &str, ex: &dyn Extractor, name: &str) -> String {
    let syms = ex.extract(src).symbols;
    let m = syms
        .iter()
        .find(|s| s.name == name && s.lang_kind.as_deref() == Some("enumerator"))
        .expect("enumerator")
        .span;
    syms.iter()
        .filter(|s| s.lang_kind.as_deref() != Some("enumerator"))
        .filter(|s| s.span.start <= m.start && m.end <= s.span.end)
        .min_by_key(|s| s.span.end - s.span.start)
        .map(|s| format!("{}:{}", s.lang_kind.as_deref().unwrap_or(""), s.name))
        .unwrap_or_default()
}

#[test]
fn enumerators_c_forms() {
    let want = pairs(&[("A", "A"), ("B", "B"), ("C", "C")]);
    for src in [
        "enum E { A, B, C };",
        "enum E { A, B, C, };",
        "enum E { A, B, C } e;",
    ] {
        assert_eq!(enumerators(&c(src)), want, "{src}");
    }
    // Initializers with expressions, casts, macros and calls.
    let src = "enum flags {
  F_NONE = 0,
  F_A = 1 << 0,
  F_B = (F_A | 2),
  F_C = MAKE(1, 2),
  F_D = sizeof(int) * 2
};
";
    assert_eq!(
        enumerators(&c(src)),
        pairs(&[
            ("F_NONE", "F_NONE = 0"),
            ("F_A", "F_A = 1 << 0"),
            ("F_B", "F_B = (F_A | 2)"),
            ("F_C", "F_C = MAKE(1, 2)"),
            ("F_D", "F_D = sizeof(int) * 2"),
        ])
    );
    assert_eq!(parent_of(src, &CExtractor, "F_C"), "enum:flags");
    // Members that differ only by case are distinct symbols.
    assert_eq!(
        enumerators(&c("enum E { red, Red, RED };")),
        pairs(&[("red", "red"), ("Red", "Red"), ("RED", "RED")])
    );
}

#[test]
fn enumerators_typedef_and_anonymous() {
    // `typedef enum { ... } Name;`: the enum has no name, so its
    // enumerators nest in the typedef.
    let src = "typedef enum { LOW, HIGH = 5 } level_t;
";
    let s = c(src);
    assert_eq!(
        enumerators(&s),
        pairs(&[("LOW", "LOW"), ("HIGH", "HIGH = 5")])
    );
    assert_eq!(parent_of(src, &CExtractor, "HIGH"), "typedef:level_t");
    // A named enum in a typedef keeps its own symbol.
    let src = "typedef enum lvl { L1 } lvl_t;";
    assert_eq!(parent_of(src, &CExtractor, "L1"), "enum:lvl");
    // A bare anonymous enum: the enumerators nest in the enclosing scope.
    let src = "enum { BUF = 64, MAXN };
int f(void) { return 0; }
";
    let s = c(src);
    assert_eq!(
        enumerators(&s),
        pairs(&[("BUF", "BUF = 64"), ("MAXN", "MAXN")])
    );
    assert_eq!(parent_of(src, &CExtractor, "BUF"), "");
    let src = "namespace n { struct S { enum { K = 1 }; int x; }; }";
    assert_eq!(parent_of(src, &CppExtractor, "K"), "struct:S");
    assert_eq!(find(&cpp(src), "x", "field").1, SymbolKind::Variable);
}

#[test]
fn enumerators_cpp_forms() {
    let src = "namespace ns {
enum class Color : std::uint8_t { Red, Green = 2 };
enum struct Dir { Up, Down };
enum Plain : int { P1 };
enum class Fwd : int;
class K { enum class Mode { On [[deprecated(\"no\")]] = 1, Off }; };
}
";
    let s = cpp(src);
    assert_eq!(
        enumerators(&s),
        pairs(&[
            ("Red", "Red"),
            ("Green", "Green = 2"),
            ("Up", "Up"),
            ("Down", "Down"),
            ("P1", "P1"),
            ("On", "On [[deprecated(\"no\")]] = 1"),
            ("Off", "Off"),
        ])
    );
    assert_eq!(parent_of(src, &CppExtractor, "Green"), "enum:Color");
    assert_eq!(parent_of(src, &CppExtractor, "Down"), "enum:Dir");
    assert_eq!(parent_of(src, &CppExtractor, "On"), "enum:Mode");
    // A forward declaration has no enumerators and no symbol.
    assert!(s.iter().all(|x| x.0 != "Fwd"));
    // GCC attributes after the name are skipped when naming.
    assert_eq!(
        enumerators(&c("enum E { OLD __attribute__((deprecated)) = 1, NEW };")),
        pairs(&[
            ("OLD", "OLD __attribute__((deprecated)) = 1"),
            ("NEW", "NEW")
        ])
    );
}

#[test]
fn enumerators_with_preprocessor_lines_and_macros() {
    // Preprocessor lines are not code: only the first `#if` branch is
    // scanned, as everywhere else.
    let src = "enum opt {
  O_A,
#ifdef HAVE_B
  O_B,
#else
  O_NOT_B,
#endif
  // comment
  O_C /* trailing */
};
";
    assert_eq!(
        enumerators(&c(src)),
        pairs(&[("O_A", "O_A"), ("O_B", "O_B"), ("O_C", "O_C")])
    );
    // An X-macro invocation is not an enumerator.
    // An X-macro invocation is not an enumerator, but an identifier after
    // it is (its span starts at the identifier).
    let src = "enum e {\n  LIST(X)\n  E_LAST\n};\n";
    assert_eq!(enumerators(&c(src)), pairs(&[("E_LAST", "E_LAST")]));
    let src = "enum Z { E(a) E(b) LAST };";
    assert_eq!(enumerators(&c(src)), pairs(&[("LAST", "LAST")]));
    let src = "enum X { DECL(a), x1 };";
    assert_eq!(enumerators(&c(src)), pairs(&[("x1", "x1")]));
    // An unbalanced `(` drops the entry it is in and every later one.
    assert!(enumerators(&c("enum V { v1 = (, v2 };")).is_empty());
    assert!(enumerators(&cpp("enum V { v1 = (, v2 };")).is_empty());
}

#[test]
fn enumerators_empty_and_unbalanced() {
    assert!(enumerators(&c("enum E { };")).is_empty());
    assert!(enumerators(&c("enum E { , };")).is_empty());
    // An unbalanced bracket ends the enumerator list; no panic, no overlap.
    for src in [
        "enum E { A, B = (1, C };",
        "enum E { A, B = [1, C",
        "enum E { A, B",
        "enum { A = ( };",
    ] {
        let s = c(src);
        assert!(
            s.iter().filter(|x| x.2 == "enumerator").all(|x| x.0 != "C"),
            "{src}: {s:#?}"
        );
        let _ = cpp(src);
    }
    // A stray closer is an ordinary token: the scan goes on.
    assert_eq!(
        enumerators(&c("enum E { A, B = 1), C };")),
        pairs(&[("A", "A"), ("B", "B = 1)"), ("C", "C")])
    );
}
