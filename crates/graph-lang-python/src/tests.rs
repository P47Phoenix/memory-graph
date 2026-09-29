use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = PythonExtractor.extract(src);
    assert!(!ex.has_errors, "{src}");
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

const SRC: &str = r#"#!/usr/bin/env python3
"""Module docs.
def not_a_function():
"""
import os

MAX_SIZE = 10
TIMEOUT: float = 2.5
lower = 1
add = lambda a, b: a + b


@dataclass
class Invoice(Base, metaclass=Meta):
    """Docs."""
    RATE = 3
    key = lambda self: self.id

    def __init__(self, n):
        self.n = n  # comment

    @property
    async def total(
        self,
        tax,
    ):
        def helper():
            return {
      "unindented": "in brackets",
            }
        return helper()

    class Inner:
        def m(self): pass

# trailing comment


async def fetch(url):
    return url
def one_liner(): return 1
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("MAX_SIZE"), (SymbolKind::Constant, "constant"));
    assert_eq!(k("TIMEOUT"), (SymbolKind::Constant, "constant"));
    assert_eq!(k("add"), (SymbolKind::Function, "lambda"));
    assert_eq!(k("Invoice"), (SymbolKind::Type, "class"));
    assert_eq!(k("key"), (SymbolKind::Function, "lambda"));
    assert_eq!(k("__init__"), (SymbolKind::Method, "method"));
    assert_eq!(k("total"), (SymbolKind::Method, "method"));
    assert_eq!(k("helper"), (SymbolKind::Function, "function"));
    assert_eq!(k("Inner"), (SymbolKind::Type, "class"));
    assert_eq!(k("m"), (SymbolKind::Method, "method"));
    assert_eq!(k("fetch"), (SymbolKind::Function, "function"));
    assert_eq!(k("one_liner"), (SymbolKind::Function, "function"));
    for n in ["lower", "RATE", "not_a_function", "os", "n"] {
        assert!(s.iter().all(|x| x.0 != n), "{n} is a symbol: {s:#?}");
    }
    // Spans.
    assert_eq!(find(&s, "MAX_SIZE").3, "MAX_SIZE = 10");
    assert_eq!(find(&s, "TIMEOUT").3, "TIMEOUT: float = 2.5");
    assert_eq!(find(&s, "add").3, "add = lambda a, b: a + b");
    let inv = &find(&s, "Invoice").3;
    assert!(inv.starts_with("@dataclass\nclass Invoice("), "{inv}");
    assert!(inv.ends_with("def m(self): pass"), "{inv}");
    assert_eq!(
        find(&s, "__init__").3,
        "def __init__(self, n):\n        self.n = n"
    );
    let total = &find(&s, "total").3;
    assert!(total.starts_with("@property\n    async def total("));
    assert!(total.ends_with("return helper()"));
    assert!(find(&s, "helper").3.ends_with('}'));
    assert_eq!(find(&s, "fetch").3, "async def fetch(url):\n    return url");
    assert_eq!(find(&s, "one_liner").3, "def one_liner(): return 1");
}

#[test]
fn syntax_errors_fall_back_to_tokens() {
    for src in [
        "def f(:\n    pass\n",
        "x = [1, 2\n",
        "x = (1]\n",
        "def f():\n    a = 1\n      b = 2\n",
        "def f():\n        a = 1\n    b = 2\n",
        "def f():\nreturn 1\n",
        "class A:\n",
        "  x = 1\n",
        ")",
    ] {
        let ex = PythonExtractor.extract(src);
        assert!(ex.has_errors, "{src:?}");
        assert!(ex.symbols.is_empty());
        assert!(!ex.tokens.is_empty());
        // Best-effort symbols are still available on request.
        let _ = symbols(&ex.tokens);
    }
    for src in [
        "",
        "# only a comment\n",
        "x = \\\n    1\n",
        "if a:\n    pass\nelif b:\n    pass\nelse:\n    pass\n",
        "s = '''\n  text\n'''\n",
        "d = {\n'a': 1,\n}\n",
        "class A: pass\n",
    ] {
        assert!(!PythonExtractor.extract(src).has_errors, "{src:?}");
    }
}

#[test]
fn continuation_lines_do_not_start_statements() {
    let s = syms("X = 1 + \\\n    2\nclass A:\n    y = [\n  1]\n    def m(self):\n        pass\n");
    assert_eq!(find(&s, "X").3, "X = 1 + \\\n    2");
    assert_eq!(find(&s, "m").1, SymbolKind::Method);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}class Ü:\n  def é(self):\n    return 'ß'\n";
    let ex = PythonExtractor.extract(src);
    assert!(!ex.has_errors);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 3));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "def é(self):\n    return 'ß'"
    );
    assert_eq!(ex.symbols[0].span.start_col, 1);
    assert_eq!(ex.symbols[0].kind, SymbolKind::Type);
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("def"), Just("class"), Just("async"), Just("lambda"), Just("A"), Just("X"),
                Just("f"), Just(":"), Just("("), Just(")"), Just("["), Just("]"), Just("{"),
                Just("}"), Just("="), Just("@"), Just(","), Just("\n"), Just("\n    "),
                Just("\n  "), Just("\\\n"), Just("'s'"), Just("'''"), Just("#c\n"), Just("pass"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = PythonExtractor.extract(&src);
        let best_effort = symbols(&ex.tokens);
        for s in ex.symbols.iter().chain(&best_effort) {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
    }
}

#[test]
fn backslash_continuation_into_a_dedent_stays_in_the_block() {
    let src = "def f():\n    x = 1 + \\\n2\n    return x\ny = 1\n";
    let s = syms(src);
    assert_eq!(find(&s, "f").3, "def f():\n    x = 1 + \\\n2\n    return x");
}

#[test]
fn inconsistent_tabs_and_spaces_are_errors() {
    // One tab is one column: like Python's TabError.
    let ex = PythonExtractor.extract("def f():\n\tx = 1\n        y = 2\n");
    assert!(ex.has_errors);
    assert!(ex.symbols.is_empty());
    // Consistent tabs are fine.
    let s = syms("class A:\n\tdef m(self):\n\t\treturn 1\n");
    assert_eq!(find(&s, "m").1, SymbolKind::Method);
}
