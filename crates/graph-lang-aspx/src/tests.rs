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
