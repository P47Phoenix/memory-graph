use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = HaskellExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    assert_exact_tokens(src, &ex);
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

/// Token texts, byte ranges, lines and columns match the source.
fn assert_exact_tokens(src: &str, ex: &Extraction) {
    let mut prev = 0;
    for t in &ex.tokens {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        assert!(s >= prev, "tokens overlap or are unsorted at {t:?}");
        prev = e;
        assert_eq!(&src[s..e], t.text);
        let before = &src[..s];
        let line = before.matches('\n').count() as u32 + 1;
        let line_start = before.rfind('\n').map_or(0, |p| p + 1);
        let col = src[line_start..s]
            .chars()
            .filter(|&c| c != '\u{feff}')
            .count() as u32
            + 1;
        assert_eq!((t.span.start_line, t.span.start_col), (line, col), "{t:?}");
    }
}

const SRC: &str = r#"{-# LANGUAGE GADTs #-}
-- | Docs
module Data.Shape
  ( Shape (..)
  , area
  ) where

import Data.List (sort)

data Shape
  = Circle Double
  | Rect Double Double
  deriving (Show, Eq)

newtype Name = Name String

type Point = (Double, Double)

type family Elem c

class (Eq a) => Container f a where
  empty :: f a
  insert :: a -> f a -> f a
  insert _ x = x

instance Show a => Show (Tree a) where
  show _ = "tree"

area :: Shape -> Double
area (Circle r) = pi * r * r
area (Rect w h)
  | w > 0 = w * h
  | otherwise = 0
  where
    unused = 1

(<+>) :: Int -> Int -> Int
a <+> b = a + b

x `plus` y = x + y

main :: IO ()
main = do
  print 1
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str(), x.3.as_str())
    };
    let m = k("Data.Shape");
    assert_eq!((m.0, m.1), (SymbolKind::Module, "module"));
    assert!(m.2.starts_with("module Data.Shape") && m.2.ends_with("print 1"));
    assert_eq!(k("Shape").1, "data");
    assert!(k("Shape").2.ends_with("deriving (Show, Eq)"));
    assert_eq!(
        k("Name"),
        (SymbolKind::Type, "newtype", "newtype Name = Name String")
    );
    assert_eq!(k("Point").1, "type");
    assert_eq!(
        k("Elem"),
        (SymbolKind::Type, "type family", "type family Elem c")
    );
    let c = k("Container");
    assert_eq!((c.0, c.1), (SymbolKind::Type, "class"));
    assert_eq!(k("empty"), (SymbolKind::Method, "method", "empty :: f a"));
    assert_eq!(
        k("insert"),
        (
            SymbolKind::Method,
            "method",
            "insert :: a -> f a -> f a\n  insert _ x = x"
        )
    );
    let i = k("Show (Tree a)");
    assert_eq!((i.0, i.1), (SymbolKind::Type, "instance"));
    assert_eq!(k("show").0, SymbolKind::Method);
    let area = k("area");
    assert_eq!((area.0, area.1), (SymbolKind::Function, "function"));
    assert!(area.2.starts_with("area :: Shape") && area.2.ends_with("unused = 1"));
    assert_eq!(k("<+>").2, "(<+>) :: Int -> Int -> Int\na <+> b = a + b");
    assert_eq!(k("plus").2, "x `plus` y = x + y");
    assert!(k("main").2.starts_with("main :: IO ()\nmain = do"));
    // Imports, locals and constructors are not symbols.
    for n in ["sort", "unused", "Circle", "import"] {
        assert!(s.iter().all(|x| x.0 != n), "{n}");
    }
}

#[test]
fn literate_bird_tracks() {
    let src = "Some prose about \"f\".\n\n> module L where\n> f :: Int\n> f = 1\n\nMore é prose.\n";
    let s = syms(src);
    assert_eq!(find(&s, "L").1, SymbolKind::Module);
    assert_eq!(find(&s, "f").3, "f :: Int\n> f = 1");
    assert!(s.iter().all(|x| x.0 != "Some" && x.0 != "More"));
}

#[test]
fn literate_latex() {
    let src = "\u{feff}Intro ü\n\\begin{code}\ng x = x\n\\end{code}\nOutro {- unclosed\n";
    let s = syms(src);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].3, "g x = x");
    let ex = HaskellExtractor.extract(src);
    assert!(ex.tokens.iter().any(|t| t.text == "Intro"));
}

#[test]
fn haddock_example_is_not_literate() {
    let src =
        "module M where\n\n{- | Adds one.\n\n> foo 1 == 2\n-}\nfoo :: Int -> Int\nfoo x = x + 1\n";
    let s = syms(src);
    assert_eq!(find(&s, "M").1, SymbolKind::Module);
    assert_eq!(find(&s, "foo").3, "foo :: Int -> Int\nfoo x = x + 1");
    // A column-0 signature outside bird tracks also means plain Haskell.
    let s = syms("foo :: Int\nfoo = 1\n> not a bird track\n");
    assert_eq!(find(&s, "foo").3, "foo :: Int\nfoo = 1");
}

#[test]
fn template_haskell_splices_are_not_functions() {
    let src = "module T where\nmakeLenses ''Foo\nderiveJSON defaultOptions ''Foo\n$(return [])\nmkThing 'bar\nf :: Int\nf = g ''Foo\n";
    let s = syms(src);
    let names: Vec<&str> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["T", "f"]);
    assert_eq!(find(&s, "f").3, "f :: Int\nf = g ''Foo");
}

#[test]
fn bom_prefixed_literate_bird_tracks() {
    let src = "\u{feff}> f :: Int\n> f = 1\nProse é.\n";
    let s = syms(src);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].3, "f :: Int\n> f = 1");
    let ex = HaskellExtractor.extract(src);
    let f = &ex.symbols[0];
    assert_eq!((f.span.start_line, f.span.start_col), (1, 3));
}

#[test]
fn deep_nesting_does_not_overflow() {
    let mut src = String::from("main = do\n");
    for i in 1..6000 {
        src.push_str(&" ".repeat(i));
        src.push_str("do\n");
    }
    let s = syms(&src);
    assert_eq!(s.len(), 1);
    let mut src = String::from("class C a where\n");
    for i in 1..6000 {
        src.push_str(&" ".repeat(i));
        src.push_str("class C a where\n");
    }
    let ex = HaskellExtractor.extract(&src);
    assert_nested(&ex);
}

#[test]
fn no_module_header() {
    let s = syms("f = 1\ng = 2\n");
    assert_eq!(s.len(), 2);
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}module Ü where\né :: Int\né = 1\n";
    let ex = HaskellExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 1));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "é :: Int\né = 1"
    );
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "module",
        "module where",
        "class",
        "instance where",
        "data = |",
        "(",
        "( + ) ::",
        "x `",
        "type family",
        "> ",
        "\\begin{code}\n{-",
    ] {
        let ex = HaskellExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
        assert_exact_tokens(src, &ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("module"), Just("where"), Just("data"), Just("class"), Just("instance"),
                Just("type"), Just("A"), Just("f"), Just("::"), Just("="), Just("=>"),
                Just("("), Just(")"), Just("<+>"), Just("`"), Just("|"), Just("\n"),
                Just("\n  "), Just("-- c\n"), Just("{- c -}"), Just("\"s\""), Just("\n> "),
                Just("\n\\begin{code}\n"), Just("é"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = HaskellExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
        assert_exact_tokens(&src, &ex);
    }
}

/// #143: reserved words are classed `keyword`; special identifiers stay
/// identifiers, as do names with a prime.
#[test]
fn keywords_are_classed_keyword() {
    use graph_core::TokenClass::{Identifier, Keyword};
    let toks = HaskellExtractor
        .extract("module M where\nimport qualified D as E\nf x = let y' = x in case y' of _ -> foreign\n")
        .tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    for w in [
        "module", "where", "import", "let", "in", "case", "of", "foreign",
    ] {
        assert_eq!(class(w), [Keyword], "{w}");
    }
    for w in ["qualified", "as", "M", "f"] {
        assert_eq!(class(w), [Identifier], "{w}");
    }
    assert!(class("y'").iter().all(|k| *k == Identifier));
    assert!(HaskellExtractor
        .version()
        .starts_with("haskell-scan-1+kw1+tok"));
}
