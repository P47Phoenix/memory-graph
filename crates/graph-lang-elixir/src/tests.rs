use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = ElixirExtractor.extract(src);
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

const SRC: &str = r#"# comment
defmodule MyApp.User do
  @moduledoc """
  def not_a_function do end
  """
  defstruct [:name, :age]

  def new(name), do: %__MODULE__{name: name}

  def valid?(%{age: age}) when age > 0 do
    Enum.map([1], fn x -> x end)
    if age, do: true, else: false
  end

  defp secret(x) do
    x.end
    :do
  end

  defmacro m(x) do
    quote do: unquote(x)
  end

  def head(a, b \\ 1)

  defmodule Inner do
    def go, do: :ok
  end
end

defprotocol Size do
  def size(data)
end

defimpl Size, for: BitString do
  def size(s), do: byte_size(s)
end

defmodule Err do
  defexception message: "oops"
end
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str(), x.3.as_str())
    };
    let user = find(&s, "MyApp.User");
    assert_eq!((user.1, user.2.as_str()), (SymbolKind::Type, "defmodule"));
    assert!(user.3.starts_with("defmodule MyApp.User do") && user.3.ends_with("  end\nend"));
    assert_eq!(
        k("%MyApp.User{}"),
        (SymbolKind::Type, "defstruct", "defstruct [:name, :age]")
    );
    assert_eq!(
        k("new"),
        (
            SymbolKind::Method,
            "def",
            "def new(name), do: %__MODULE__{name: name}"
        )
    );
    let v = k("valid?");
    assert_eq!((v.0, v.1), (SymbolKind::Method, "def"));
    assert!(v.2.ends_with("else: false\n  end"));
    let sec = k("secret");
    assert_eq!(sec.1, "defp");
    assert!(sec.2.ends_with(":do\n  end"));
    assert!(k("m").2.ends_with("unquote(x)\n  end"));
    assert_eq!(k("m").1, "defmacro");
    assert_eq!(k("head").2, "def head(a, b \\\\ 1)");
    assert_eq!(k("Inner").0, SymbolKind::Type);
    assert_eq!(k("go").2, "def go, do: :ok");
    assert_eq!(k("Size").1, "defprotocol");
    assert_eq!(k("size").2, "def size(data)");
    let imp = k("Size for BitString");
    assert_eq!((imp.0, imp.1), (SymbolKind::Type, "defimpl"));
    assert_eq!(
        k("%Err{}"),
        (
            SymbolKind::Type,
            "defexception",
            "defexception message: \"oops\""
        )
    );
    // The heredoc's `def` is not a symbol.
    assert!(s.iter().all(|x| x.0 != "not_a_function"));
}

#[test]
fn top_level_def_is_a_function() {
    let s = syms("def f(x) do\n  x\nend\n");
    assert_eq!(s[0].1, SymbolKind::Function);
    assert_eq!(s[0].3, "def f(x) do\n  x\nend");
}

#[test]
fn deep_nesting_does_not_overflow() {
    let n = 6000;
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("defmodule M{i} do\n"));
    }
    src.push_str(&"end\n".repeat(n));
    let ex = ElixirExtractor.extract(&src);
    assert!(ex.symbols.len() > 60);
    assert_nested(&ex);
    let src = format!(
        "def f do\n{}{}end\n",
        "if x do\n".repeat(n),
        "end\n".repeat(n)
    );
    let s = syms(&src);
    assert_eq!(s.len(), 1);
    assert!(s[0].3.ends_with("end\nend"));
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}defmodule Ü do\n  def é(x), do: \"ö\"\nend";
    let ex = ElixirExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (2, 3));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "def é(x), do: \"ö\""
    );
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "defmodule",
        "defmodule A do",
        "defmodule A do\n  def f do\n",
        "end end def",
        "def (",
        "defimpl P, for:",
        "defstruct",
        "defmodule A do\n  def f, do: 1\nend end",
    ] {
        let ex = ElixirExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("defmodule"), Just("def"), Just("defp"), Just("defimpl"), Just("defstruct"),
                Just("do"), Just("end"), Just("fn"), Just("do:"), Just(":end"), Just("A"),
                Just("f"), Just("("), Just(")"), Just("["), Just("]"), Just(","), Just("\n"),
                Just("\n  "), Just("#c\n"), Just("\"s\""), Just("\"\"\"x\n"), Just(";"),
                Just("for:"), Just("."),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = ElixirExtractor.extract(&src);
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

/// #143: reserved words are classed `keyword`; atoms, keyword-list keys,
/// fields and macros (`def`, `if`) stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    let src = "defmodule M do\n  def f(x) when x in [nil, true] do\n    if x, do: :end, else: m.end\n  end\nend\n";
    let toks = ElixirExtractor.extract(src).tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(class("when"), [TokenClass::Keyword]);
    assert_eq!(class("in"), [TokenClass::Keyword]);
    assert_eq!(class("nil"), [TokenClass::Keyword]);
    assert_eq!(class("true"), [TokenClass::Keyword]);
    assert_eq!(
        class("do"),
        [
            TokenClass::Keyword,
            TokenClass::Keyword,
            TokenClass::Identifier
        ]
    );
    assert_eq!(class("else"), [TokenClass::Identifier]);
    assert_eq!(
        class("end"),
        [
            TokenClass::Identifier,
            TokenClass::Identifier,
            TokenClass::Keyword,
            TokenClass::Keyword
        ]
    );
    assert_eq!(class("def"), [TokenClass::Identifier]);
    assert_eq!(class("if"), [TokenClass::Identifier]);
    assert!(ElixirExtractor
        .version()
        .starts_with("elixir-scan-1+kw1+tok"));
}
