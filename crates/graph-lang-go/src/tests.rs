use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = GoExtractor.extract(src);
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

const SRC: &str = r#"// Package billing does things.
package billing

import (
	"fmt"
	"strings"
)

const Max = 10

const (
	A = iota
	B
	C, D = 1,
		2
)

var (
	registry = map[string]int{
		"a": 1,
	}
	mu sync.Mutex
)

var raw = `{ not a brace`

type Invoice struct {
	Lines []Line
	total int
}

type Store interface {
	Get(id string) (*Invoice, error)
}

type ID = string

type List[T any] struct{ items []T }

type (
	Amount int64
	Handler func(int) error
)

func New(n int) *Invoice {
	if n > 0 {
		return &Invoice{}
	}
	return nil
}

func (i *Invoice) Total() int { return i.total }

func (l List[T]) Len() int {
	return len(l.items)
}

func Empty() interface{} { return nil }

func Anon() struct{ X int } {
	return struct{ X int }{1}
}
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("billing"), (SymbolKind::Module, "package"));
    assert!(find(&s, "billing").3.starts_with("package billing"));
    assert!(find(&s, "billing").3.ends_with("{1}\n}"));
    assert_eq!(k("Max"), (SymbolKind::Constant, "const"));
    assert_eq!(find(&s, "Max").3, "const Max = 10");
    assert_eq!(k("A"), (SymbolKind::Constant, "const"));
    assert_eq!(find(&s, "A").3, "A = iota");
    assert_eq!(find(&s, "B").3, "B");
    assert_eq!(find(&s, "C").3, "C, D = 1,\n\t\t2");
    assert!(s.iter().all(|x| x.0 != "D"));
    assert_eq!(k("registry"), (SymbolKind::Variable, "var"));
    assert_eq!(
        find(&s, "registry").3,
        "registry = map[string]int{\n\t\t\"a\": 1,\n\t}"
    );
    assert_eq!(k("mu"), (SymbolKind::Variable, "var"));
    assert_eq!(find(&s, "raw").3, "var raw = `{ not a brace`");
    assert_eq!(k("Invoice"), (SymbolKind::Type, "struct"));
    assert!(find(&s, "Invoice").3.starts_with("type Invoice struct {"));
    assert!(find(&s, "Invoice").3.ends_with("total int\n}"));
    assert_eq!(k("Store"), (SymbolKind::Type, "interface"));
    assert_eq!(k("ID"), (SymbolKind::Type, "type"));
    assert_eq!(k("List"), (SymbolKind::Type, "struct"));
    assert_eq!(k("Amount"), (SymbolKind::Type, "type"));
    assert_eq!(k("Handler"), (SymbolKind::Type, "type"));
    assert_eq!(k("New"), (SymbolKind::Function, "func"));
    assert!(find(&s, "New").3.starts_with("func New(n int) *Invoice {"));
    assert!(find(&s, "New").3.ends_with("return nil\n}"));
    assert_eq!(k("Total"), (SymbolKind::Method, "method"));
    assert_eq!(
        find(&s, "Total").3,
        "func (i *Invoice) Total() int { return i.total }"
    );
    assert_eq!(k("Len"), (SymbolKind::Method, "method"));
    assert_eq!(
        find(&s, "Empty").3,
        "func Empty() interface{} { return nil }"
    );
    assert!(find(&s, "Anon").3.ends_with("{1}\n}"));
    // Imports, fields, interface methods and locals are not symbols.
    for n in ["fmt", "Lines", "total", "Get", "items", "n"] {
        assert!(s.iter().all(|x| x.0 != n), "{n}");
    }
    // Methods are siblings of their receiver type, not nested in it.
    let inv = find(&s, "Invoice").3.len();
    assert!(inv < 60);
}

/// Issue #137: a method's owner is its receiver's type name, whatever the
/// receiver's form; everything else has no owner.
#[test]
fn methods_are_owned_by_their_receiver_type() {
    let src = "package p\n\
               func (i *Invoice) A() {}\n\
               func (l List[K, V]) B() {}\n\
               func (Plain) C() {}\n\
               func (p *Pair[map[string]int]) D() {}\n\
               func (r Rect) E() {}\n\
               func () F() {}\n\
               func G() {}\n";
    let ex = GoExtractor.extract(src);
    let owner = |n: &str| {
        ex.symbols
            .iter()
            .find(|s| s.name == n)
            .unwrap_or_else(|| panic!("{n}"))
            .owner
            .clone()
    };
    assert_eq!(owner("A").as_deref(), Some("Invoice"));
    assert_eq!(owner("B").as_deref(), Some("List"));
    assert_eq!(owner("C").as_deref(), Some("Plain"));
    assert_eq!(owner("D").as_deref(), Some("Pair"));
    assert_eq!(owner("E").as_deref(), Some("Rect"));
    assert_eq!(owner("F"), None);
    assert_eq!(owner("G"), None);
    assert_eq!(owner("p"), None);
    // The version changed with the new output, so stored files re-index.
    assert!(GoExtractor.version().starts_with("go-scan-2+"));
}

#[test]
fn semicolons_and_bodyless_funcs() {
    let s = syms("package p; var a = 1; var b = 2\nfunc asm(x int) int\nfunc g() {}");
    assert_eq!(find(&s, "a").3, "var a = 1");
    assert_eq!(find(&s, "b").3, "var b = 2");
    assert_eq!(find(&s, "asm").3, "func asm(x int) int");
    assert_eq!(find(&s, "g").3, "func g() {}");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}package p\n\nfunc Ünï() {}\nvar é = 1";
    let ex = GoExtractor.extract(src);
    assert_eq!(ex.symbols[0].span.start_col, 1);
    let v = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((v.span.start_line, v.span.start_col), (4, 1));
    assert_eq!(
        &src[v.span.start as usize..v.span.end as usize],
        "var é = 1"
    );
    let f = ex.symbols.iter().find(|s| s.name == "Ünï").unwrap();
    assert_eq!((f.span.start_line, f.span.end_col), (3, 14));
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "package",
        "func",
        "func (",
        "func f( {",
        "type (",
        "const ( A = ",
        "var x = [",
        "}}} type",
        "func (r T)",
        "",
    ] {
        let ex = GoExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("package"), Just("func"), Just("type"), Just("const"), Just("var"),
                Just("import"), Just("struct"), Just("interface"), Just("A"), Just("B"),
                Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"),
                Just("="), Just(","), Just("\n"), Just("`r`"), Just("\"s\""), Just("//c\n"),
                Just("*"), Just("1"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = GoExtractor.extract(&src);
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
