use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = JavaScriptExtractor.extract(src);
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

const SRC: &str = r#"// util
export async function load(url) {
  function inner() { return '}'; }
  return fetch(url);
}
export default class Cart extends Base {
  #items = [];
  constructor(a) { super(a); }
  static create() { return new Cart(); }
  get size() { return this.#items.length; }
  set size(v) {}
  async *walk() { yield 1; }
}
const add = (a, b) => a + b;
let sq = x => x * x
var h = function named() { return "{"; };
const k = async () => {
  const deep = () => 1;
};
const notFn = 5;
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("load"), (SymbolKind::Function, "function"));
    assert_eq!(k("inner"), (SymbolKind::Function, "function"));
    assert_eq!(k("Cart"), (SymbolKind::Type, "class"));
    assert_eq!(k("constructor"), (SymbolKind::Method, "constructor"));
    assert_eq!(k("create"), (SymbolKind::Method, "method"));
    assert_eq!(k("walk"), (SymbolKind::Method, "method"));
    assert_eq!(k("add"), (SymbolKind::Function, "arrow_fn"));
    assert_eq!(k("sq"), (SymbolKind::Function, "arrow_fn"));
    assert_eq!(k("h"), (SymbolKind::Function, "fn_expr"));
    assert_eq!(k("named"), (SymbolKind::Function, "function"));
    assert_eq!(k("k"), (SymbolKind::Function, "arrow_fn"));
    assert_eq!(k("deep"), (SymbolKind::Function, "arrow_fn"));
    let sizes: Vec<_> = s
        .iter()
        .filter(|x| x.0 == "size")
        .map(|x| x.2.as_str())
        .collect();
    assert_eq!(sizes, ["get", "set"]);
    assert!(s.iter().all(|x| x.0 != "notFn"));
    assert!(find(&s, "load").3.starts_with("export async function load"));
    assert!(find(&s, "Cart").3.starts_with("export default class Cart"));
    assert_eq!(find(&s, "add").3, "const add = (a, b) => a + b;");
    assert_eq!(find(&s, "sq").3, "let sq = x => x * x");
    assert_eq!(
        find(&s, "h").3,
        "var h = function named() { return \"{\"; };"
    );
    assert_eq!(find(&s, "walk").3, "async *walk() { yield 1; }");
}

#[test]
fn arrow_in_call_arguments() {
    let s = syms("app.get('/', (req, res) => res.send('x'));\nconst f = y => g(y), z = 1;\n");
    assert_eq!(find(&s, "f").3, "const f = y => g(y)");
}

#[test]
fn review_regressions() {
    // Declarations in an arrow body stay inside the arrow (was a partial overlap).
    let s = syms("const f = () => function\n g(){ }\n");
    assert_eq!(find(&s, "f").3, "const f = () => function");
    assert!(s.iter().all(|x| x.0 != "g"));
    // Empty arrow bodies are not symbols (was a partial overlap).
    syms("const g = (x) =>const g = (x) =>,");
    syms("f(a => , b => ; c => ) d => ]");
    // A quote inside a regex does not hide later symbols.
    let s = syms("const s = x => x.replace(/'/g, ''); function c(){}\nconst q = s.split(/\"/); function after2(){}");
    find(&s, "c");
    find(&s, "after2");
    // Heritage clauses are not the class body.
    let s = syms("class A extends mix({a(){}}) { m(){} }");
    assert_eq!(find(&s, "A").3, "class A extends mix({a(){}}) { m(){} }");
    find(&s, "m");
    assert!(s.iter().all(|x| x.0 != "a"));
    // Chained / continued expression bodies span lines.
    let s = syms("const f = async () =>\n  fetch(u)\n  .then(r => r.json())\n  .catch(e => e);\nconst n = x =>\n  x ? 1\n  : 2;\n");
    assert!(find(&s, "f").3.ends_with(".catch(e => e);"));
    assert!(find(&s, "n").3.ends_with(": 2;"));
    // JSX closing tags are not regex literals.
    let s = syms("const App = () => (\n  <div onClick={() => { go() }}>{items.map(i => <li>{i}</li>)}</div>\n);\nfunction z(){}");
    assert!(find(&s, "App").3.ends_with(");"));
    find(&s, "z");
    // Private members keep `#`.
    let s = syms("class P { #p() {} p() {} }");
    find(&s, "#p");
    find(&s, "p");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}const é = () => 1;\nfunction ü() {}";
    let ex = JavaScriptExtractor.extract(src);
    let u = ex.symbols.iter().find(|s| s.name == "ü").unwrap();
    assert_eq!((u.span.start_line, u.span.start_col), (2, 1));
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "function f( {",
        "class { }",
        "class A {",
        "const x = (",
        "const f = () =>",
        "var r = /[}]/; function g() {}",
        "}}}{{{",
        "",
    ] {
        let ex = JavaScriptExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("function"), Just("class"), Just("const"), Just("let"), Just("a"),
                Just("b"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"),
                Just(";"), Just("="), Just("=>"), Just(","), Just("async"), Just("export"),
                Just("get"), Just("static"), Just("*"), Just("\n"), Just("'s'"), Just("//c\n"),
                Just("/"), Just("=> ,"), Just("=> )"), Just("=> ;"), Just("function\n"), Just("#"), Just("extends"), Just(".then"), Just("/x/"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = JavaScriptExtractor.extract(&src);
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
    for (open, close) in [("class A { m() {", "} }"), ("const f = () => {", "};")] {
        for balanced in [true, false] {
            let mut src = open.repeat(n);
            if balanced {
                src.push_str(&close.repeat(n));
            }
            let ex = on_small_stack(move || JavaScriptExtractor.extract(&src));
            if balanced {
                // Outer declarations are kept, at most MAX_DEPTH levels deep.
                assert!(!ex.symbols.is_empty(), "{open}");
                assert!(ex.symbols.len() <= 2 * MAX_DEPTH + 2, "{open}");
                assert_eq!(ex.symbols[0].span.start, 0);
            }
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
        parts in proptest::collection::vec(prop_oneof![Just("class"), Just("extends"), Just("A"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"), Just("="), Just("=>"), Just("<"), Just(">"), Just(","), Just("\"s"), Just("`"), Just("function")], 0..60),
        code_first in any::<bool>(),
    ) {
        let soup = parts.join(" ");
        let code = "class A extends B { m() {} }\nclass C extends mix({ a() {} }) { n() {} }\nfunction f() { class D { } }\n";
        let src = if code_first { format!("{code}{soup}") } else { format!("{soup}\n{code}") };
        let ex: &dyn Extractor = &JavaScriptExtractor;
            prop_assert_eq!(ex.extract(&src).symbols, without_memo(ex, &src), "{}", src);
    }
}

/// #143: reserved words are classed `keyword`; a contextual word (`let`) and
/// reserved words used as property names (`a.new`, `a?.delete`, `#if`,
/// `{ default: 1 }`) stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    use graph_core::TokenClass;
    let src = "function f() { let o = { default: 1, class: 2 }; return a.new + a?.delete; }\nclass A { #if = 1 }";
    let toks = JavaScriptExtractor.extract(src).tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(class("function"), [TokenClass::Keyword]);
    assert_eq!(class("return"), [TokenClass::Keyword]);
    assert_eq!(class("let"), [TokenClass::Identifier]);
    for w in ["default", "new", "delete", "if"] {
        assert_eq!(class(w), [TokenClass::Identifier], "{w}");
    }
    assert_eq!(
        class("class"),
        [TokenClass::Identifier, TokenClass::Keyword]
    );
    assert!(JavaScriptExtractor
        .version()
        .starts_with("javascript-scan-1+kw1+tok"));
}
