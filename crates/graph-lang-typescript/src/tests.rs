use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = TypeScriptExtractor.extract(src);
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

/// How many symbols `drop_partial_overlaps` dropped for `src` (#212).
fn drops(src: &str) -> usize {
    symbols_and_drops(&tokenize_with(src, TS_TOKENIZER)).1
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

/// #145: a type alias whose "type" is a declaration (`= class ... {}`)
/// stopped inside the declaration and partially overlapped its symbol.
#[test]
fn type_alias_never_ends_inside_a_declaration() {
    for src in [
        "type class = class class { }",
        "type A = class B { }",
        "type A = interface B { }",
        "type A = enum B { }",
        "type A = function f() { }",
        "type A = B | class C { }",
        "class type class = { } ;",
        "class type A = { } ;",
        "class interface A { } ;",
        "class enum A { } ;",
        "class module A { } ;",
    ] {
        assert_nested(&TypeScriptExtractor.extract(src));
    }
    // Real aliases are unaffected.
    let s = syms("type A = B | C;");
    assert_eq!(find(&s, "A").3, "type A = B | C;");
}

fn inside(s: &[Sym], outer: &str, inner: &str, src: &str) -> bool {
    let (o, i) = (&find(s, outer).3, &find(s, inner).3);
    let (os, is) = (src.find(o.as_str()).unwrap(), src.find(i.as_str()).unwrap());
    os <= is && is + i.len() <= os + o.len()
}

const SRC: &str = r#"import { Injectable } from '@angular/core';

export interface User<T = string> extends Base {
  id: T;
  greet(): void;
}

export type Id = string | number;
type Handler<E> = (event: E) => void;
type Shape =
  | { kind: 'circle'; r: number }
  | { kind: 'square'; s: number };

export const enum Color { Red = 'r', Green = 'g' }

declare module "express" {
  interface Request { user?: User }
}

export namespace Acme.Billing {
  export function total<T extends { n: number }>(items: T[]): number {
    return 0;
  }
}

@Injectable({ providedIn: 'root' })
export abstract class Repo<T> implements Store<T> {
  @Input() name: string;
  private readonly cache = new Map<string, T>();
  static count: number = 0;
  optional?: boolean
  readonly: boolean;

  constructor(private http: HttpClient) {}

  abstract find(id: string): Promise<T>;

  public async load<K>(key: K): Promise<T | undefined> {
    function inner(): void {}
    return undefined;
  }

  get size(): number { return 0; }
}

export const add = (a: number, b: number): number => a + b;
const typed: Handler<string> = (e) => {};
declare function fetchAll(url: string): Promise<void>;
"#;

#[test]
fn declarations() {
    let s = syms(SRC);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("User"), (SymbolKind::Type, "interface"));
    assert_eq!(k("Id"), (SymbolKind::Type, "type"));
    assert_eq!(k("Handler"), (SymbolKind::Type, "type"));
    assert_eq!(k("Shape"), (SymbolKind::Type, "type"));
    assert_eq!(k("Color"), (SymbolKind::Type, "enum"));
    assert_eq!(k("express"), (SymbolKind::Module, "module"));
    assert_eq!(k("Request"), (SymbolKind::Type, "interface"));
    assert_eq!(k("Acme.Billing"), (SymbolKind::Module, "namespace"));
    assert_eq!(k("total"), (SymbolKind::Function, "function"));
    assert_eq!(k("Repo"), (SymbolKind::Type, "abstract_class"));
    assert_eq!(k("name"), (SymbolKind::Variable, "field"));
    assert_eq!(k("cache"), (SymbolKind::Variable, "field"));
    assert_eq!(k("count"), (SymbolKind::Variable, "field"));
    assert_eq!(k("optional"), (SymbolKind::Variable, "field"));
    assert_eq!(k("readonly"), (SymbolKind::Variable, "field"));
    assert_eq!(k("constructor"), (SymbolKind::Method, "constructor"));
    assert_eq!(k("find"), (SymbolKind::Method, "method"));
    assert_eq!(k("load"), (SymbolKind::Method, "method"));
    assert_eq!(k("inner"), (SymbolKind::Function, "function"));
    assert_eq!(k("size"), (SymbolKind::Method, "get"));
    assert_eq!(k("add"), (SymbolKind::Function, "arrow_fn"));
    assert_eq!(k("typed"), (SymbolKind::Function, "arrow_fn"));
    assert_eq!(k("fetchAll"), (SymbolKind::Function, "function"));
    // Interface and enum members, decorators, imports are not symbols.
    for n in ["id", "greet", "Red", "Injectable", "Input", "user"] {
        assert!(s.iter().all(|x| x.0 != n), "{n} is a symbol: {s:#?}");
    }
    // Spans.
    assert!(find(&s, "User")
        .3
        .starts_with("export interface User<T = string>"));
    assert!(find(&s, "User").3.ends_with("greet(): void;\n}"));
    assert_eq!(find(&s, "Id").3, "export type Id = string | number;");
    assert_eq!(
        find(&s, "Handler").3,
        "type Handler<E> = (event: E) => void;"
    );
    assert!(find(&s, "Shape").3.ends_with("s: number };"));
    assert_eq!(
        find(&s, "Color").3,
        "export const enum Color { Red = 'r', Green = 'g' }"
    );
    assert!(find(&s, "express")
        .3
        .starts_with("declare module \"express\" {"));
    let repo = &find(&s, "Repo").3;
    assert!(repo.starts_with("export abstract class Repo<T>"), "{repo}");
    assert_eq!(find(&s, "name").3, "name: string;");
    assert_eq!(
        find(&s, "cache").3,
        "private readonly cache = new Map<string, T>();"
    );
    assert_eq!(find(&s, "count").3, "static count: number = 0;");
    assert_eq!(find(&s, "optional").3, "optional?: boolean");
    assert_eq!(find(&s, "find").3, "abstract find(id: string): Promise<T>;");
    assert!(find(&s, "load")
        .3
        .starts_with("public async load<K>(key: K): Promise<T | undefined> {"));
    assert_eq!(
        find(&s, "add").3,
        "export const add = (a: number, b: number): number => a + b;"
    );
    assert_eq!(
        find(&s, "fetchAll").3,
        "declare function fetchAll(url: string): Promise<void>;"
    );
    // Nesting.
    assert!(inside(&s, "Acme.Billing", "total", SRC));
    assert!(inside(&s, "express", "Request", SRC));
    assert!(inside(&s, "Repo", "load", SRC));
    assert!(inside(&s, "load", "inner", SRC));
}

#[test]
fn member_accesses_and_identifiers_named_like_keywords() {
    let s = syms("const type = 1;\nlet module = x.module;\nfoo.namespace = 2;\nobj.type = 3;\nlet enum_ = interface_;\n");
    assert!(s.is_empty(), "{s:#?}");
}

#[test]
fn javascript_behaviour_is_kept() {
    let src = "export default class Cart extends Base {\n  #items = [];\n  static create() { return 1; }\n}\nconst sq = x => x * x\n";
    let s = syms(src);
    assert_eq!(find(&s, "Cart").2, "class");
    assert_eq!(find(&s, "create").1, SymbolKind::Method);
    assert_eq!(find(&s, "#items").2, "field");
    assert_eq!(find(&s, "sq").3, "const sq = x => x * x");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}interface Ü {\n  x: 1\n}\nclass Ä {\n  é: number;\n}";
    let ex = TypeScriptExtractor.extract(src);
    let f = ex.symbols.iter().find(|s| s.name == "é").unwrap();
    assert_eq!((f.span.start_line, f.span.start_col), (5, 3));
    assert_eq!(
        &src[f.span.start as usize..f.span.end as usize],
        "é: number;"
    );
    assert_eq!(ex.symbols[0].name, "Ü");
    assert_eq!(ex.symbols[0].span.start_col, 1);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "interface A {",
        "type A<T = ;",
        "type = ",
        "namespace {",
        "declare module \"x",
        "module 'é",
        "enum E {",
        "class A<T { }",
        "class A { m(): { }",
        "function f<T(",
        "const f = <T>(",
        "abstract class",
        "[",
        "",
    ] {
        let ex = TypeScriptExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("class"), Just("interface"), Just("type"), Just("enum"), Just("namespace"),
                Just("module"), Just("abstract"), Just("function"), Just("const"), Just("A"),
                Just("B"), Just("{"), Just("}"), Just("("), Just(")"), Just("["), Just("]"),
                Just(";"), Just("="), Just(">"), Just("<"), Just(":"), Just("|"), Just("=>"),
                Just("@"), Just("?"), Just(","), Just("\"s\""), Just("//c\n"), Just("\n"),
                Just("export"), Just("declare"), Just("default"), Just("let"), Just("x"),
                Just("."), Just("async"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = TypeScriptExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
        prop_assert_eq!(drops(&src), 0, "{}", src);
    }
}

proptest! {
    /// #203: whole declaration fragments in any order, some left open (a
    /// trailing `|`, `&`, `=`), joined by newlines, never produce spans the
    /// store would reject.
    #[test]
    fn declaration_fragments_keep_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("type A = B |"), Just("type A = B &"), Just("type A ="), Just("type A = B;"),
                Just("type A<T> = (x: T) =>"), Just("export"), Just("declare"), Just("default"),
                Just("export const enum E { X }"), Just("const enum E { X }"),
                Just("declare module \"m\" { }"), Just("namespace N.M { }"),
                Just("declare namespace N { }"), Just("export interface I { a: T }"),
                Just("abstract class K { m(): void; f: T | }"), Just("class C { x: A |"),
                Just("export function f(): T |"), Just("}"), Just("{"), Just("const f = (): T =>"),
                Just("declare function g(): void;"), Just("export default class D { }"),
            ],
            0..12,
        )
    ) {
        let src = parts.join("\n");
        assert_nested(&TypeScriptExtractor.extract(&src));
        // #212/#213: the overlap filter is a safety net; the scanners must
        // not need it, even with fragments left open.
        prop_assert_eq!(drops(&src), 0, "{}", src);
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
    for (open, close) in [
        ("class A { m(): void {", "} }"),
        ("const f = (): void => {", "};"),
        ("namespace N {", "}"),
    ] {
        for balanced in [true, false] {
            let mut src = open.repeat(n);
            if balanced {
                src.push_str(&close.repeat(n));
            }
            let ex = on_small_stack(move || TypeScriptExtractor.extract(&src));
            if balanced {
                assert!(!ex.symbols.is_empty(), "{open}");
                assert_eq!(ex.symbols[0].span.start, 0);
            }
        }
    }
}

#[test]
fn keyword_member_accesses_and_arrow_params_are_not_declarations() {
    // `.module` / `.namespace` / `.interface` / `.enum` / `.type` followed by
    // what would otherwise complete a declaration.
    for src in [
        "obj.module\nfoo { }\n",
        "obj.namespace\nfoo { }\n",
        "obj.interface\nFoo { }\n",
        "obj.enum\nE { }\n",
        "obj.type\nT = 1;\n",
        // `type X =>`: an arrow, not a type alias.
        "let a = type\nb => b;\n",
    ] {
        let s = syms(src);
        assert!(
            s.iter().all(|x| !matches!(
                x.2.as_str(),
                "module" | "namespace" | "interface" | "enum" | "type"
            )),
            "{src}: {s:#?}"
        );
    }
}

#[test]
fn overload_signatures_without_semicolons() {
    let src = "class C {\n  m(a: string): void\n  m(a: number): void\n  m(a) { return a; }\n}\n";
    let s = syms(src);
    let ms: Vec<_> = s
        .iter()
        .filter(|x| x.0 == "m")
        .map(|x| (x.1, x.3.as_str()))
        .collect();
    assert_eq!(
        ms,
        [
            (SymbolKind::Method, "m(a: string): void"),
            (SymbolKind::Method, "m(a: number): void"),
            (SymbolKind::Method, "m(a) { return a; }"),
        ]
    );
}

/// #143: JavaScript's reserved words are classed `keyword` in TypeScript;
/// TypeScript's contextual words (`type`, `interface`) and a property name
/// (`x.import`) stay identifiers.
#[test]
fn keywords_are_classed_keyword() {
    use graph_core::TokenClass;
    let src = "export interface I { a: number }\ntype T = string;\nconst x = y.import;";
    let toks = TypeScriptExtractor.extract(src).tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(class("export"), [TokenClass::Keyword]);
    assert_eq!(class("const"), [TokenClass::Keyword]);
    assert_eq!(class("interface"), [TokenClass::Identifier]);
    assert_eq!(class("type"), [TokenClass::Identifier]);
    assert_eq!(class("import"), [TokenClass::Identifier]);
    assert!(TypeScriptExtractor
        .version()
        .starts_with("typescript-scan-4+kw1+tok"));
}

/// Review of #164: interface and type-literal members with reserved names
/// (after `;`, optional `?:`) and methods with a return type are names.
#[test]
fn keyword_classing_members() {
    use graph_core::TokenClass::{Identifier as I, Keyword as K};
    let classes = |src: &str, word: &str| {
        TypeScriptExtractor
            .extract(src)
            .tokens
            .iter()
            .filter(|t| t.text == word)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        classes("interface I { a: T; default?: U; }", "default"),
        [I]
    );
    assert_eq!(classes("type T = { a: 1; new: 2 };", "new"), [I]);
    assert_eq!(
        classes("class A { delete(): boolean { return true; } }", "delete"),
        [I]
    );
    assert_eq!(
        classes("class A { delete(): boolean { return true; } }", "return"),
        [K]
    );
    assert_eq!(classes("switch (x) { default: break; }", "default"), [K]);
}

/// #203: a type alias left open by a trailing operator read the next
/// statement's `export` / `declare` as a type operand, so its span ended
/// inside the declaration whose span starts at that word.
#[test]
fn type_alias_stops_before_the_next_statement() {
    for (src, alias) in [
        ("type A = B |\nexport class K { }\n", "type A = B |"),
        ("type A = B |\ndeclare module \"m\" { }\n", "type A = B |"),
        ("type A = B &\ndeclare namespace N { }\n", "type A = B &"),
        ("type A = B |\nexport const enum E { }\n", "type A = B |"),
        ("type A = B |\nabstract class K { }\n", "type A = B |"),
        ("type A = B |\nnamespace N { }\n", "type A = B |"),
        ("type A = B |\nimport x from \"y\";\n", "type A = B |"),
        ("type A = B |\nasync function f() { }\n", "type A = B |"),
        ("type A = B |\ntype C = D;\n", "type A = B |"),
    ] {
        let s = syms(src);
        assert_eq!(find(&s, "A").3, alias, "{src}");
    }
    // Contextual words are still types where no name follows them.
    let s = syms("type A = B | declare;\ntype M = module | type;\n");
    assert_eq!(find(&s, "A").3, "type A = B | declare;");
    assert_eq!(find(&s, "M").3, "type M = module | type;");
    // Property names and import types are not statements.
    for alias in [
        "type P = React.default;",
        "type Q = x.const | y.export;",
        "type I = import(\"./m\").T;",
        "type J = typeof import(\"./m\");",
        "type K = A | import(\"./m\").T;",
    ] {
        let name = &alias[5..6];
        assert_eq!(find(&syms(alias), name).3, alias);
    }
}

/// #212/#213: the overlap filter's drops are visible to tests, and the
/// shapes the fragment proptest found no longer need it: an arrow body or
/// a field initializer stops before a word that opens the next statement.
#[test]
fn known_overlap_drops_are_counted() {
    assert_eq!(drops("type A = B;\nclass C { m(): void {} }"), 0);
    for src in [
        "const f = (): T =>\nexport\ntype A = B |",
        "class C { x: A |\ntype A = B |\ndefault\n}",
        "const f = () => x |\nexport\ntype A = B;",
        "const f = (): T =>\ndeclare\ntype A = B;",
        "class C { x = a |\n@dec\nm() {} }",
        "class C { x: A |\ntype A = B |\ndeclare\n{\n}\n}",
    ] {
        assert_eq!(drops(src), 0, "{src}");
        syms(src);
    }
    // The alias keeps its span; the arrow with no body is not a symbol.
    let s = syms("const f = (): T =>\nexport\ntype A = B |");
    assert_eq!(find(&s, "A").3, "export\ntype A = B |");
    assert!(s.iter().all(|x| x.0 != "f"), "{s:#?}");
    // The stray field stops before `default`.
    let s = syms("class C { x: A |\ntype A = B |\ndefault\n}");
    assert_eq!(find(&s, "x").3, "x: A |");
    // Async arrows are still arrows.
    let s = syms("const f = () => async () => 1;");
    assert_eq!(find(&s, "f").3, "const f = () => async () => 1;");
}

/// #213: `abstract new` constructor types, generic function types and a
/// decorator after an open alias.
#[test]
fn constructor_generic_and_decorator_types() {
    for (src, name, text) in [
        (
            "type U = abstract new () => X;",
            "U",
            "type U = abstract new () => X;",
        ),
        (
            "type F = <const T>(x: T) => T;",
            "F",
            "type F = <const T>(x: T) => T;",
        ),
        ("type G = <T>(x: T) => T;", "G", "type G = <T>(x: T) => T;"),
        ("type L = Array<T>;", "L", "type L = Array<T>;"),
        (
            "const v: abstract new () => X = () => null;",
            "v",
            "const v: abstract new () => X = () => null;",
        ),
        ("type A = B |\n@dec\nexport class K {}", "A", "type A = B |"),
    ] {
        let s = syms(src);
        assert_eq!(find(&s, name).3, text, "{src}");
    }
    let s = syms("type A = B |\n@dec\nexport class K {}");
    assert_eq!(find(&s, "K").3, "export class K {}");
    // `abstract class` is still a declaration after an open alias.
    let s = syms("type A = B |\nabstract class K { }\n");
    assert_eq!(find(&s, "A").3, "type A = B |");
}

/// Review of #213: statement words used as operands (`as const`,
/// `import.meta`, identifiers named `let`/`type`/...) do not end a type, an
/// arrow body or a field initializer.
#[test]
fn statement_words_as_operands_do_not_end_expressions() {
    for (src, name, text) in [
        (
            "class F { arr = [1, 2] as const; m() {} }",
            "arr",
            "arr = [1, 2] as const;",
        ),
        (
            "class F { h = () => ({ a: 1 }) as const; }",
            "h",
            "h = () => ({ a: 1 }) as const;",
        ),
        (
            "const g = () => [1] as const;",
            "g",
            "const g = () => [1] as const;",
        ),
        (
            "const s = () => x satisfies const;",
            "s",
            "const s = () => x satisfies const;",
        ),
        (
            "const u = x => import.meta.url;",
            "u",
            "const u = x => import.meta.url;",
        ),
        (
            "class F { u = import.meta.url; }",
            "u",
            "u = import.meta.url;",
        ),
        (
            "const i = () => import(\"m\");",
            "i",
            "const i = () => import(\"m\");",
        ),
        ("class F { v = typeof let; }", "v", "v = typeof let;"),
        ("class F { w = a.default; }", "w", "w = a.default;"),
        ("class F { t = type; }", "t", "t = type;"),
        (
            "class F { n = module.exports; }",
            "n",
            "n = module.exports;",
        ),
        ("class F { d = declare; }", "d", "d = declare;"),
        ("class F { a = abstract; }", "a", "a = abstract;"),
        ("const k = () => async;", "k", "const k = () => async;"),
        (
            "type C = <const T>(x: T) => T;",
            "C",
            "type C = <const T>(x: T) => T;",
        ),
        (
            "const p: X<const> = () => 1;",
            "p",
            "const p: X<const> = () => 1;",
        ),
    ] {
        let s = syms(src);
        assert_eq!(find(&s, name).3, text, "{src}");
        assert_no_wrong_symbols(&s, src);
    }
    // Real statements still end an open expression.
    let s = syms("class C { x = a |\nexport\n}\nexport const y = 1;");
    assert_eq!(find(&s, "x").3, "x = a |");
}

/// No symbol is named after a statement or declaration word, and every
/// field ends at its `;` or at the end of a line.
fn assert_no_wrong_symbols(s: &[Sym], src: &str) {
    for (name, _, lang, text) in s {
        assert!(
            ![
                "export",
                "import",
                "default",
                "const",
                "let",
                "var",
                "declare",
                "abstract",
                "namespace",
                "module",
                "type",
                "async",
                "as"
            ]
            .contains(&name.as_str()),
            "{src}: wrong symbol {name}"
        );
        if lang == "field" {
            let rest = &src[src.find(text.as_str()).unwrap() + text.len()..];
            assert!(
                text.ends_with(';') || rest.is_empty() || rest.starts_with(['\n', '\r', ' ', '}']),
                "{src}: field {text:?} ends mid-line"
            );
        }
    }
}

proptest! {
    /// Fields and arrows built from statement words used as operands keep
    /// their full spans and never give a symbol named after the word.
    #[test]
    fn operand_words_keep_full_spans(
        word in prop_oneof![
            Just("const"), Just("import.meta"), Just("default"), Just("let"), Just("var"),
            Just("type"), Just("module"), Just("namespace"), Just("declare"), Just("abstract"),
            Just("async"), Just("export"),
        ],
        lead in prop_oneof![Just("x as "), Just("y satisfies "), Just("typeof "), Just("a."), Just("a?.")],
    ) {
        let src = format!("class F {{ f = {lead}{word}; m() {{}} }}\nconst g = () => {lead}{word};");
        let s = syms(&src);
        prop_assert_eq!(&find(&s, "f").3, &format!("f = {lead}{word};"));
        prop_assert_eq!(&find(&s, "g").3, &format!("const g = () => {lead}{word};"));
        assert_no_wrong_symbols(&s, &src);
    }
}
