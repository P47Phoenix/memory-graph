use super::*;
use proptest::prelude::*;

type Sym = (String, String, String);

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

fn syms(src: &str) -> Vec<Sym> {
    let ex = AspxExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    ex.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.lang_kind.unwrap(), text)
        })
        .collect()
}

const PAGE: &str = r#"<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Default.aspx.cs" Inherits="Web.Default" %>
<%@ Register TagPrefix="uc" TagName="Header" Src="~/Header.ascx" %>
<%-- <asp:Label id="hidden" runat="server" /> --%>
<html>
<body>
  <form id="form1" runat="server">
    <uc:Header id="hdr" runat="server" />
    <asp:Label ID="lblName" runat="server" Text='<%# Eval("Name") %>'></asp:Label>
    <asp:Button ID="btnSave" runat="server" OnClick="Save_Click" Text="Save" />
    <div class="x"><%= DateTime.Now %></div>
    <% if (User.IsAdmin) { %>
      <asp:Panel runat="server"><p id="adm">admin</p></asp:Panel>
    <% } %>
    <a href="<%: Url("/x") %>">x</a>
    <asp:Literal runat="server" Text="<%$ Resources:Main, Title %>" />
  </form>
</body>
</html>
"#;

#[test]
fn directives_controls_and_blocks() {
    let s = syms(PAGE);
    let get = |n: &str, k: &str| {
        s.iter()
            .find(|x| x.0 == n && x.1 == k)
            .unwrap_or_else(|| panic!("{n}/{k} in {s:#?}"))
    };
    assert!(get("Page", "directive")
        .2
        .ends_with("Inherits=\"Web.Default\" %>"));
    get("Register", "directive");
    assert!(get("form1", "control").2.ends_with("</form>"));
    get("hdr", "control");
    assert_eq!(
        get("lblName", "control").2,
        "<asp:Label ID=\"lblName\" runat=\"server\" Text='<%# Eval(\"Name\") %>'></asp:Label>"
    );
    assert_eq!(get("Eval", "binding").2, "<%# Eval(\"Name\") %>");
    get("btnSave", "control");
    assert_eq!(get("DateTime", "expression").2, "<%= DateTime.Now %>");
    assert_eq!(get("if", "code_block").2, "<% if (User.IsAdmin) { %>");
    get("code_block", "code_block");
    get("asp:Panel", "control");
    get("adm", "element");
    get("Url", "expression");
    get("Resources", "expression_builder");
    get("asp:Literal", "control");
    // Commented-out controls are not symbols.
    assert!(s.iter().all(|x| x.0 != "hidden"));
}

/// #72: server-side C# in `<script runat="server">` and `<% %>` blocks.
#[test]
fn server_script_csharp_symbols() {
    let src = "<%@ Page Language=\"C#\" %>\n<script runat=\"server\">  public class M\n    {\n        public int[] P {get;set;}\n    }\n\n    M Model {get;set;}\n\n    void Page_Load(object sender, EventArgs e)\n    {\n        if (a < b && c > \"</x>\") { }\n    }\n</script>\n<h1 id=\"t\">x</h1>\n<% int n = 0; %>\n";
    let ex = AspxExtractor.extract(src);
    assert_nested(&ex);
    // Token spans are exact, in file coordinates (C# tokens in the body).
    for t in &ex.tokens {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        assert_eq!(&src[s..e], t.text);
        let line = 1 + src[..s].matches('\n').count() as u32;
        let col = 1 + src[..s].rsplit('\n').next().unwrap().chars().count() as u32;
        assert_eq!((t.span.start_line, t.span.start_col), (line, col), "{t:?}");
        let line = 1 + src[..e].matches('\n').count() as u32;
        let col = 1 + src[..e].rsplit('\n').next().unwrap().chars().count() as u32;
        assert_eq!((t.span.end_line, t.span.end_col), (line, col), "{t:?}");
    }
    // The C# string is one literal token, not markup.
    assert!(ex.tokens.iter().any(|t| t.text == "\"</x>\""));
    let s = syms(src);
    let get = |n: &str, k: &str| {
        s.iter()
            .find(|x| x.0 == n && x.1 == k)
            .unwrap_or_else(|| panic!("{n}/{k} in {s:#?}"))
    };
    let module = get("server_script", "server_script");
    assert!(module.2.starts_with("public class M") && module.2.ends_with("}"));
    assert!(get("M", "class").2.ends_with("{get;set;}\n    }"));
    get("P", "property");
    get("Model", "property");
    assert!(get("Page_Load", "method").2.starts_with("void Page_Load("));
    get("t", "element");
    let kind = |n: &str| {
        AspxExtractor
            .extract(src)
            .symbols
            .into_iter()
            .find(|x| x.name == n)
            .unwrap()
            .kind
    };
    assert_eq!(kind("server_script"), SymbolKind::Module);
    // Code blocks on a C# page: their locals, nested in the block.
    assert_eq!(get("n", "local").2, "int n = 0;");
    assert_eq!(get("int", "code_block").2, "<% int n = 0; %>");
    // Unclosed server scripts, and bodies with server tags, stay markup.
    for src in [
        "<script runat=\"server\" language=\"C#\"> class A {} ",
        "<script runat=\"server\" language=\"C#\"> class A { <%= x %> } </script>",
        "<script language=\"C#\"> class A {} </script>",
        "<script runat=\"server\" language=\"C#\" /> class A {} </script>",
        "<script runat=\"server\" language=\"C#\"> class A {} </script",
        "<script runat=\"server\" language=\"C#\"> class A {} </script <b>",
    ] {
        let s = syms(src);
        assert!(s.iter().all(|x| x.1 != "server_script"), "{src}: {s:?}");
    }
}

/// Only C# scripts are spliced: the script's `language`, else the
/// directive's; no language at all means VB (the Web Forms default).
#[test]
fn server_script_language() {
    let body = "<script runat=\"server\">\nPublic Class M\n  Sub Page_Load()\n  End Sub\nEnd Class\n</script>";
    let has = |src: &str| syms(src).iter().any(|x| x.1 == "server_script");
    assert!(!has(body), "no language: VB");
    assert!(!has(&format!("<%@ Page Language=\"VB\" %>{body}")));
    for lang in ["C#", "cs", "CSharp", "c#"] {
        assert!(
            has(&format!("<%@ Page Language=\"{lang}\" %>{body}")),
            "{lang}"
        );
        assert!(
            has(&format!("<%@ Control language='{lang}' %>{body}")),
            "{lang}"
        );
    }
    // The script's own attribute wins over the directive.
    let vb = body.replace("runat=", "language=\"VB\" runat=");
    assert!(!has(&format!("<%@ Page Language=\"C#\" %>{vb}")));
    let cs = body.replace("runat=", "language=\"C#\" runat=");
    assert!(has(&format!("<%@ Page Language=\"VB\" %>{cs}")));
    assert!(has(&cs));
}

#[test]
fn well_nested_detects_partial_overlap() {
    use graph_core::Span;
    let sp = |a: u32, b: u32| Span {
        start: a,
        end: b,
        start_line: 1,
        start_col: a + 1,
        end_line: 1,
        end_col: b + 1,
    };
    let sym = |a, b| SymbolDecl {
        name: "s".into(),
        kind: SymbolKind::Other,
        lang_kind: None,
        span: sp(a, b),
        owner: None,
    };
    let tok = |a, b| TokenDecl {
        text: "t".into(),
        class: TokenClass::Identifier,
        span: sp(a, b),
    };
    let ex = |symbols, tokens| Extraction {
        symbols,
        tokens,
        has_errors: false,
    };
    assert!(well_nested(&ex(
        vec![sym(0, 10), sym(2, 5)],
        vec![tok(0, 2), tok(8, 10)]
    )));
    assert!(!well_nested(&ex(vec![sym(0, 5), sym(3, 8)], vec![])));
    assert!(!well_nested(&ex(vec![sym(0, 5)], vec![tok(4, 6)])));
    assert!(!well_nested(&ex(
        vec![sym(0, 5), sym(1, 3)],
        vec![tok(2, 4)]
    )));
}

#[test]
fn version_is_pinned() {
    assert!(AspxExtractor
        .version()
        .starts_with("aspx-scan-2+kw1+cb1+tok"));
}

#[test]
fn computed_attribute_values_are_not_names() {
    let s = syms("<div id=<%= X %> runat=\"server\"></div><td class=<%# Eval(\"a\") %> id=t></td>");
    let names: Vec<_> = s.iter().map(|x| (x.0.as_str(), x.1.as_str())).collect();
    assert_eq!(
        names,
        [
            ("div", "control"),
            ("X", "expression"),
            ("t", "element"),
            ("Eval", "binding"),
        ]
    );
}

#[test]
fn claims_web_forms_extensions() {
    for e in ["aspx", "ascx", "master"] {
        assert!(AspxExtractor.extensions().contains(&e));
    }
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "<%",
        "<%@ Page",
        "<asp:Button id=",
        "<div <% x %>",
        "<% // x %><asp:A id=a>",
        "%>",
        "<a b='<% x",
        "",
    ] {
        let ex = AspxExtractor.extract(src);
        assert!(!ex.has_errors, "{src}");
        assert_nested(&ex);
    }
}

proptest! {
    #[test]
    fn markup_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("<asp:Label id=a runat=server>"), Just("</asp:Label>"), Just("<div id=d>"),
                Just("</div>"), Just("<%"), Just("<%="), Just("<%#"), Just("<%@ Page"), Just("%>"),
                Just("<%--"), Just("--%>"), Just("'"), Just("\""), Just("<"), Just(">"), Just("x"),
                Just("<script runat=server>"), Just("</script>"), Just("//"), Just("\n"),
                Just("<script runat=server language=cs>"), Just("<%@ Page Language=\"C#\" %>"),
                Just("class A { void M() { } }"), Just("{"), Just("}"), Just("int P {get;set;}"),
                Just("var x = 1;"), Just("h\u{e9}\u{1F600}"), Just("foreach (var p in q)"),
            ],
            0..40,
        )
    ) {
        let src = parts.concat();
        let ex = AspxExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
        }
        assert_nested(&ex);
        for t in &ex.tokens {
            prop_assert_eq!(&src[t.span.start as usize..t.span.end as usize], t.text.as_str());
        }
    }
}

fn classes_of(toks: &[TokenDecl], text: &str) -> Vec<TokenClass> {
    toks.iter()
        .filter(|t| t.text == text)
        .map(|t| t.class)
        .collect()
}

/// #143: every C# reserved word, bare in a C# server script body, is a
/// keyword.
#[test]
fn every_listed_keyword_is_classed_keyword() {
    for kw in graph_lang_csharp::KEYWORDS {
        let src =
            format!("<%@ Page Language=\"C#\" %>\n<script runat=\"server\">\n{kw}\n</script>\n");
        let toks = AspxExtractor.extract(&src).tokens;
        let body = src.find("server\">").unwrap() as u32;
        let in_body: Vec<_> = toks
            .iter()
            .filter(|t| t.text == *kw && t.span.start > body)
            .map(|t| t.class)
            .collect();
        assert_eq!(in_body, [TokenClass::Keyword], "{kw}");
    }
}

/// Only C# server script bodies get keywords: markup, attribute values,
/// `<% %>` blocks (possibly VB) and VB scripts do not; symbols are
/// unchanged.
#[test]
fn keywords_only_in_csharp_server_scripts() {
    let src = "<%@ Page Language=\"C#\" %>\n<div class=\"if\">for while</div>\n<% if (x) { } %>\n<script runat=\"server\">\nprotected void Page_Load(object s, EventArgs e) { if (@class) return; }\n</script>\n";
    let ex = AspxExtractor.extract(src);
    let toks = &ex.tokens;
    assert_eq!(
        classes_of(toks, "if"),
        [TokenClass::Keyword, TokenClass::Keyword],
        "a C# page's `<% if %>` and the script's `if` are keywords"
    );
    assert_eq!(classes_of(toks, "for"), [TokenClass::Identifier]);
    assert_eq!(classes_of(toks, "while"), [TokenClass::Identifier]);
    for kw in ["protected", "void", "object", "return"] {
        assert_eq!(classes_of(toks, kw), [TokenClass::Keyword], "{kw}");
    }
    assert_eq!(
        classes_of(toks, "class"),
        [TokenClass::Identifier, TokenClass::Identifier],
        "the `class` attribute name, then the verbatim `@class`"
    );
    assert!(ex.symbols.iter().any(|s| s.name == "Page_Load"));
    let vb = "<script runat=\"server\" language=\"VB\">\nPublic Sub X()\nIf y Then Return\nEnd Sub\n</script>\n";
    let toks = AspxExtractor.extract(vb).tokens;
    assert!(
        toks.iter().all(|t| t.class != TokenClass::Keyword),
        "{toks:?}"
    );
    assert!(AspxExtractor
        .version()
        .starts_with("aspx-scan-2+kw1+cb1+tok"));
    // No directive language (VB, the default): `<% %>` stays unclassed.
    let toks = AspxExtractor.extract("<% if (x) { int n = 0; } %>").tokens;
    assert!(toks.iter().all(|t| t.class != TokenClass::Keyword));
}

/// Asserts every token's text and line/column span against `src`.
fn assert_exact_tokens(src: &str, ex: &Extraction) {
    for t in &ex.tokens {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        assert_eq!(&src[s..e], t.text);
        let line = 1 + src[..s].matches('\n').count() as u32;
        let col = 1 + src[..s].rsplit('\n').next().unwrap().chars().count() as u32;
        assert_eq!((t.span.start_line, t.span.start_col), (line, col), "{t:?}");
        let line = 1 + src[..e].matches('\n').count() as u32;
        let col = 1 + src[..e].rsplit('\n').next().unwrap().chars().count() as u32;
        assert_eq!((t.span.end_line, t.span.end_col), (line, col), "{t:?}");
    }
}

/// Multibyte text (2-, 3- and 4-byte UTF-8) right before the first block.
const CS_BLOCKS: &str = "<%@ Page Language=\"C#\" %>\n<p>h\u{e9}llo \u{1F600} \u{65e5}\u{672c}</p> <% var total = 0;\n  const int Max = 5;\n  List<Dictionary<string, int[]>> rows = Load();\n  foreach (var p in People) { %>\n  <div id=\"row\"><%= p.Name %></div>\n<% }\n  for (int i = 0; i < Max; i++) total += i;\n  try { } catch (Exception ex) { }\n  using (var conn = Open()) { }\n  return x; await y; a = b; x.y = z; Foo(); if (a) b(); else c(); %>\n";

/// #72: locals in `<% %>` blocks of a C# page, nested in the block, with
/// exact spans after multibyte text.
#[test]
fn code_block_locals_on_csharp_page() {
    let ex = AspxExtractor.extract(CS_BLOCKS);
    assert_exact_tokens(CS_BLOCKS, &ex);
    let s = syms(CS_BLOCKS);
    let locals: Vec<_> = s
        .iter()
        .filter(|x| x.1.starts_with("local"))
        .map(|x| (x.0.as_str(), x.1.as_str(), x.2.as_str()))
        .collect();
    assert_eq!(
        locals,
        [
            ("total", "local", "var total = 0;"),
            ("Max", "local_const", "const int Max = 5;"),
            (
                "rows",
                "local",
                "List<Dictionary<string, int[]>> rows = Load();"
            ),
            ("p", "local", "var p"),
            ("i", "local", "int i"),
            ("ex", "local", "Exception ex"),
            ("conn", "local", "var conn"),
        ]
    );
    // Each local nests in a code block, so it is scoped under the page or
    // the enclosing element.
    for l in &ex.symbols {
        if l.lang_kind
            .as_deref()
            .is_some_and(|k| k.starts_with("local"))
        {
            assert!(ex
                .symbols
                .iter()
                .any(|b| b.lang_kind.as_deref() == Some("code_block")
                    && b.span.start <= l.span.start
                    && l.span.end <= b.span.end));
        }
    }
    let kind = |n: &str| ex.symbols.iter().find(|x| x.name == n).unwrap().kind;
    assert_eq!(kind("Max"), SymbolKind::Constant);
    assert_eq!(kind("total"), SymbolKind::Variable);
    // Keywords in C# code blocks; `<%= %>` expressions stay markup.
    assert_eq!(classes_of(&ex.tokens, "foreach"), [TokenClass::Keyword]);
    assert_eq!(classes_of(&ex.tokens, "Name"), [TokenClass::Identifier]);
    // Markup symbols are still found.
    assert!(s.iter().any(|x| x.0 == "row" && x.1 == "element"));
    assert!(s.iter().any(|x| x.0 == "p" && x.1 == "expression"));
    assert!(s.iter().any(|x| x.0 == "var" && x.1 == "code_block"));
}

/// VB pages (explicit, or by default with no language) are unaffected: no
/// locals, no keywords, the same tokens as plain markup.
#[test]
fn code_blocks_of_vb_pages_unchanged() {
    for directive in ["<%@ Page Language=\"VB\" %>", "<%@ Page %>", ""] {
        let src = CS_BLOCKS.replacen("<%@ Page Language=\"C#\" %>", directive, 1);
        let ex = AspxExtractor.extract(&src);
        assert!(ex.symbols.iter().all(|x| !x
            .lang_kind
            .as_deref()
            .unwrap_or("")
            .starts_with("local")));
        assert!(ex.tokens.iter().all(|t| t.class != TokenClass::Keyword));
        assert_eq!(
            ex.tokens,
            tokenize_with(&src, ASPX_TOKENIZER),
            "{directive}"
        );
    }
    // On a C# page: a `<%` whose next server tag is not its `%>` stays
    // markup; a statement after `}` is a declaration.
    let cs = "<%@ Page Language=\"C#\" %>\n";
    let ex = AspxExtractor.extract(&format!("{cs}<% int x = 1; <%= y %>"));
    assert!(ex.tokens.iter().all(|t| t.class != TokenClass::Keyword));
    assert!(ex.symbols.iter().all(|s| s.name != "x"));
    assert!(syms(&format!("{cs}<% if (a) {{ %>t<% }} int x = 1; %>"))
        .iter()
        .any(|s| s.0 == "x" && s.1 == "local"));
    let vb = "<%@ Page Language=\"VB\" %>\n<% Dim n As Integer = 0 %>";
    assert!(syms(vb).iter().all(|x| x.0 != "n"));
    // A C# script's own `language` does not make the page's blocks C#.
    let src = "<script runat=\"server\" language=\"C#\"> int F; </script><% int n = 0; %>";
    assert!(syms(src).iter().all(|x| x.0 != "n"));
}
