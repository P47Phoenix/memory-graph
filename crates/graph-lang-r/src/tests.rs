use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = RExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    for t in &ex.tokens {
        assert_eq!(t.text, src[t.span.start as usize..t.span.end as usize]);
    }
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

const SRC: &str = r#"# utilities
my.fun <- function(x, y = 2) {
  inner = function(z) z * 2  # nested
  inner(x) + y
}
`odd name` <<- \(v) v + 1
square = function(n) n^2
function(a) a - 1 -> dec
if (x == 1) y <- 2
Person <- setRefClass("Person", fields = list(name = "character"),
  methods = list(greet = function() cat("hi", name)))
setClass("Shape", representation("VIRTUAL"))
Counter <- R6::R6Class("Counter",
  public = list(
    n = 0,
    add = function(by = 1) {
      self$n <- self$n + by
    }
  )
)
setGeneric("area", function(shape) standardGeneric("area"))
setMethod("area", "Square", function(shape) shape@s^2)
obj$handler <- function(e) NULL
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("my.fun"), (SymbolKind::Function, "function"));
    assert_eq!(k("inner"), (SymbolKind::Function, "function"));
    assert_eq!(k("odd name"), (SymbolKind::Function, "function"));
    assert_eq!(k("square"), (SymbolKind::Function, "function"));
    assert_eq!(k("dec"), (SymbolKind::Function, "function"));
    assert_eq!(k("Person"), (SymbolKind::Type, "setRefClass"));
    assert_eq!(k("greet"), (SymbolKind::Method, "function"));
    assert_eq!(k("Shape"), (SymbolKind::Type, "setClass"));
    assert_eq!(k("Counter"), (SymbolKind::Type, "R6Class"));
    assert_eq!(k("add"), (SymbolKind::Method, "function"));
    assert_eq!(k("obj$handler"), (SymbolKind::Function, "function"));
    let areas: Vec<_> = s.iter().filter(|x| x.0 == "area").collect();
    assert_eq!(areas.len(), 2);
    assert_eq!(
        (areas[0].1, areas[0].2.as_str()),
        (SymbolKind::Method, "generic")
    );
    assert_eq!(areas[1].2, "method");
    assert_eq!(find(&s, "inner").3, "inner = function(z) z * 2");
    assert!(find(&s, "my.fun").3.ends_with("inner(x) + y\n}"));
    assert_eq!(find(&s, "odd name").3, "`odd name` <<- \\(v) v + 1");
    assert_eq!(find(&s, "square").3, "square = function(n) n^2");
    assert_eq!(find(&s, "dec").3, "function(a) a - 1 -> dec");
    assert!(find(&s, "Counter").3.starts_with("Counter <- R6::R6Class("));
    assert!(find(&s, "Counter").3.ends_with("  )\n)"));
    assert_eq!(
        find(&s, "Shape").3,
        "setClass(\"Shape\", representation(\"VIRTUAL\"))"
    );
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert!(!names.contains(&"y") && !names.contains(&"n"), "{names:?}");
    assert_eq!(s.iter().filter(|x| x.0 == "Counter").count(), 1);
}

#[test]
fn r_markdown_scans_only_r_chunks() {
    let src = "---\ntitle: \"don't\"\n---\n\nSome prose with f <- function() 1 in it.\n\n```{r setup, echo=FALSE}\nhelper <- function(x) {\n  x\n}\n```\n\n```{python}\ndef py(): pass\nnot_r <- function() 2\n```\n\n```{r}\nlast = function() 3\n```\n";
    let s = syms(src);
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["helper", "last"]);
    assert_eq!(find(&s, "helper").3, "helper <- function(x) {\n  x\n}");
    let ex = RExtractor.extract(src);
    let h = ex.symbols.iter().find(|x| x.name == "helper").unwrap();
    assert_eq!((h.span.start_line, h.span.start_col), (8, 1));
    // Prose is tokenized too.
    assert!(ex.tokens.iter().any(|t| t.text == "prose"));
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}# é\nf_é <- function() \"ü\"\n";
    let ex = RExtractor.extract(src);
    let f = &ex.symbols[0];
    assert_eq!(f.name, "f_é");
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "f_é <- function() \"ü\""
    );
    assert_eq!((f.span.start_line, f.span.start_col), (2, 1));
    assert_eq!(f.span.end_col, 22);
    let doc = "\u{feff}é\n```{r}\ng <- function() 1\n```\n";
    let ex = RExtractor.extract(doc);
    assert_eq!(ex.symbols[0].span.start_line, 3);
    for t in &ex.tokens {
        assert_eq!(t.text, doc[t.span.start as usize..t.span.end as usize]);
    }
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "f <- function(",
        "f <- function(x) {",
        "<- function() 1",
        "function() 1 ->",
        "setClass(",
        "X <- R6Class(",
        "setGeneric()",
        "```{r}\nf <- function(",
        "```{r}",
        "`",
        ") } ] <- = \\",
    ] {
        let ex = RExtractor.extract(src);
        assert_nested(&ex);
        for s in &ex.symbols {
            assert!(s.span.end as usize <= src.len());
        }
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("f"), Just("<-"), Just("<<-"), Just("="), Just("->"), Just("function"),
                Just("\\"), Just("("), Just(")"), Just("{"), Just("}"), Just(","), Just("\n"),
                Just("setClass"), Just("R6Class"), Just("setMethod"), Just("\"s\""), Just("`b`"),
                Just("::"), Just("R6"), Just("$"), Just("# c\n"), Just("\n```{r}\n"), Just("\n```\n"),
                Just("é"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = RExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for t in &ex.tokens {
            prop_assert_eq!(&t.text, &src[t.span.start as usize..t.span.end as usize]);
        }
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
    }
}
