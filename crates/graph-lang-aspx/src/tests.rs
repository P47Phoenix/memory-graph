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
    // Code blocks are statements, not scanned for declarations.
    assert!(s.iter().all(|x| x.0 != "n"));
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
    assert!(AspxExtractor.version().starts_with("aspx-scan-2+tok"));
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
    }
}
