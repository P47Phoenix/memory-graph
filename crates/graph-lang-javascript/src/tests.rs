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
                Just("/"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = JavaScriptExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
        }
        assert_nested(&ex);
    }
}
