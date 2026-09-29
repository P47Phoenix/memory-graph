use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = ScalaExtractor.extract(src);
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

const SRC: &str = r##"package com.acme.billing

import scala.util.Try

/** Docs. */
@deprecated("x")
sealed trait Shape {
  def area: Double
  val sides: Int
}

final case class Circle(r: Double) extends Shape {
  override def area: Double = math.Pi * r * r
  val sides = 0
  private[billing] def scaled(k: Double): Circle = {
    val local = r * k
    Circle(local)
  }
  def +(o: Circle): Circle = Circle(r + o.r)
}

object Shapes extends App {
  type Registry = Map[String, Shape]
  lazy val all: List[Shape] = List(
    Circle(1),
    Circle(2)
  )
  var count = 0
  def main(args: Array[String]): Unit = println("""raw { string""")
  case object Empty
}

enum Color:
  case Red, Green
  def hex: String = "#"

class Plain(x: Int)

def topLevel(x: Int): Int =
  x + 1

val answer = 42
"##;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("com.acme.billing"), (SymbolKind::Module, "package"));
    assert!(find(&s, "com.acme.billing").3.ends_with("val answer = 42"));
    assert_eq!(k("Shape"), (SymbolKind::Type, "trait"));
    assert!(find(&s, "Shape")
        .3
        .starts_with("@deprecated(\"x\")\nsealed trait Shape {"));
    assert!(find(&s, "Shape").3.ends_with("val sides: Int\n}"));
    assert_eq!(k("area"), (SymbolKind::Method, "def"));
    assert_eq!(find(&s, "area").3, "def area: Double");
    assert_eq!(k("Circle"), (SymbolKind::Type, "case class"));
    assert_eq!(k("scaled"), (SymbolKind::Method, "def"));
    assert!(find(&s, "scaled")
        .3
        .starts_with("private[billing] def scaled("));
    assert!(find(&s, "scaled").3.ends_with("Circle(local)\n  }"));
    assert_eq!(k("+"), (SymbolKind::Method, "def"));
    assert_eq!(k("Shapes"), (SymbolKind::Module, "object"));
    assert_eq!(k("Registry"), (SymbolKind::Type, "type"));
    assert_eq!(k("all"), (SymbolKind::Variable, "val"));
    assert!(find(&s, "all").3.ends_with("Circle(2)\n  )"));
    assert_eq!(k("count"), (SymbolKind::Variable, "var"));
    // A def in an object is a function (objects are modules).
    assert_eq!(k("main"), (SymbolKind::Function, "def"));
    assert_eq!(k("Empty"), (SymbolKind::Module, "case object"));
    assert_eq!(k("Color"), (SymbolKind::Type, "enum"));
    assert!(find(&s, "Color").3.ends_with("def hex: String = \"#\""));
    assert_eq!(k("hex"), (SymbolKind::Method, "def"));
    assert_eq!(k("Plain"), (SymbolKind::Type, "class"));
    assert_eq!(k("topLevel"), (SymbolKind::Function, "def"));
    assert_eq!(
        find(&s, "topLevel").3,
        "def topLevel(x: Int): Int =\n  x + 1"
    );
    assert_eq!(k("answer"), (SymbolKind::Variable, "val"));
    let sides: Vec<_> = s.iter().filter(|x| x.0 == "sides").collect();
    assert_eq!(sides.len(), 2);
    // Locals, parameters, enum cases and imports are not symbols.
    assert_eq!(k("Red"), (SymbolKind::Constant, "case"));
    assert_eq!(find(&s, "Red").3, "case Red");
    assert_eq!(find(&s, "Green").3, "Green");
    for n in ["local", "r", "Try", "x", "args"] {
        assert!(s.iter().all(|x| x.0 != n), "{n}");
    }
}

#[test]
fn braced_packages_and_package_objects() {
    let s = syms("package a {\n  package object b { val x = 1 }\n  class C; class D\n}\n");
    assert_eq!(find(&s, "a").2, "package");
    assert_eq!(find(&s, "b").2, "package object");
    assert_eq!(find(&s, "b").1, SymbolKind::Module);
    assert_eq!(find(&s, "x").1, SymbolKind::Variable);
    assert_eq!(find(&s, "C").3, "class C");
    assert_eq!(find(&s, "D").3, "class D");
    assert!(find(&s, "a").3.ends_with('}'));
}

#[test]
fn backquoted_and_setter_names() {
    let s = syms(
        "class A {\n  def `type`: Int = 1\n  def x_=(v: Int): Unit = ()\n  def ::(h: Int) = h\n}",
    );
    for n in ["`type`", "x_=", "::"] {
        assert_eq!(find(&s, n).1, SymbolKind::Method, "{n}");
    }
}

#[test]
fn givens_and_enum_cases() {
    let src = "trait Ord[T]\ngiven intOrd: Ord[Int] with {\n  def compare(a: Int, b: Int) = a - b\n}\ngiven Ord[Long] with\n  def compare(a: Long, b: Long) = 0\ngiven strOrd: Ord[String] with\n  def compare(a: String, b: String) = 0\nenum Planet(mass: Double):\n  case Earth extends Planet(5.9)\n  case Mars extends Planet(0.6)\n  def heavy = mass > 1\n";
    let s = syms(src);
    assert_eq!(find(&s, "intOrd").1, SymbolKind::Type);
    assert_eq!(find(&s, "intOrd").2, "given");
    assert!(find(&s, "intOrd").3.ends_with("a - b\n}"));
    assert_eq!(find(&s, "strOrd").2, "given");
    let cmp: Vec<_> = s.iter().filter(|x| x.0 == "compare").collect();
    assert_eq!(cmp.len(), 3);
    assert!(cmp.iter().all(|c| c.1 == SymbolKind::Method), "{cmp:#?}");
    assert_eq!(find(&s, "Earth").1, SymbolKind::Constant);
    assert_eq!(find(&s, "Earth").3, "case Earth extends Planet(5.9)");
    assert_eq!(find(&s, "Mars").2, "case");
    assert_eq!(find(&s, "heavy").1, SymbolKind::Method);
}

#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    let ex = ScalaExtractor.extract(&"object O { ".repeat(20000));
    assert!(!ex.symbols.is_empty() && ex.symbols.len() <= MAX_DEPTH);
    // Siblings, not nesting: all found.
    let ex = ScalaExtractor.extract(&"class C:\n".repeat(20000));
    assert_eq!(ex.symbols.len(), 20000);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}object Ü {\n  val é = 1\n}";
    let ex = ScalaExtractor.extract(src);
    assert_eq!(ex.symbols[0].span.start_col, 1);
    let v = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((v.span.start_line, v.span.start_col), (2, 3));
    assert_eq!(
        &src[v.span.start as usize..v.span.end as usize],
        "val é = 1"
    );
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "class A {",
        "def",
        "package",
        "object { }",
        "@",
        "private[",
        "}}} class",
        "class A(",
        "def f(x: Int = { ) = 1",
        "",
    ] {
        let ex = ScalaExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("package"), Just("object"), Just("class"), Just("trait"), Just("def"),
                Just("val"), Just("case"), Just("private"), Just("@"), Just("A"), Just("B"),
                Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"), Just(";"),
                Just("="), Just(":"), Just("\n"), Just("\n  "), Just("\"\"\"r\"\"\""),
                Just("//c\n"), Just("."), Just("+"), Just("`"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = ScalaExtractor.extract(&src);
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
