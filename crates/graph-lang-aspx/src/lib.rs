//! ASP.NET Web Forms markup extractor (`.aspx`, `.ascx`, `.master`). `.asax`,
//! `.ashx` and `.asmx` are not claimed: they are usually one directive then
//! plain C#, which markup rules would misread.
//!
//! Symbols (all `SymbolKind::Other`):
//!
//! | Markup | `lang_kind` | name |
//! |---|---|---|
//! | `<%@ Page ... %>` | `directive` | `Page`, `Control`, `Register`, ... |
//! | `<% ... %>` | `code_block` | first identifier inside, else `code_block` |
//! | `<%= ... %>`, `<%: ... %>` | `expression` | first identifier inside |
//! | `<%# ... %>` | `binding` | first identifier inside |
//! | `<%$ ... %>` | `expression_builder` | first identifier inside |
//! | `<asp:Button id="b">`, any `runat="server"` tag | `control` | id, else the tag |
//! | HTML elements (see `graph-lang-html`) | `element`, `script`, ... | id / tag |
//!
//! Server-side `<script runat="server">` code is tokenized but not scanned
//! for C# symbols (follow-up #72). Element spans follow the HTML scanner's rule;
//! server blocks are opaque to it (they may sit inside a start tag), so their
//! spans never cross a tag. An unclosed `<%` hides the rest of the file from
//! the element scanner; its own symbol spans just the `<%`.
use graph_core::scan::span_between;
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use graph_lang_html::{html_symbol, scan_elements, Tag};

pub struct AspxExtractor;

/// Tokenizer dialect used for ASP.NET markup.
pub const ASPX_TOKENIZER: TokenizerOptions = TokenizerOptions {
    rust_literals: false,
    single_quote_strings: false,
    csharp_strings: true,
    markup: true,
    aspx: true,
    regex_literals: false,
};

impl Extractor for AspxExtractor {
    fn language(&self) -> &str {
        "aspx"
    }

    fn extensions(&self) -> &[&str] {
        &["aspx", "ascx", "master"]
    }

    fn version(&self) -> String {
        format!("aspx-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, ASPX_TOKENIZER);
        let mut symbols = server_blocks(&tokens);
        symbols.extend(scan_elements(&tokens, &[("<%", "%>")], aspx_symbol));
        symbols.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Server controls (prefixed tags or `runat="server"`), else the HTML rule.
fn aspx_symbol(tag: &Tag) -> Option<(String, String)> {
    let server = tag.name.contains(':')
        || tag
            .attr("runat")
            .is_some_and(|v| v.eq_ignore_ascii_case("server"));
    if server {
        let name = tag.attr("id").unwrap_or(&tag.name).to_string();
        return Some((name, "control".into()));
    }
    html_symbol(tag)
}

/// `<%@ %>`, `<% %>`, `<%= %>`, `<%: %>`, `<%# %>`, `<%$ %>` blocks.
fn server_blocks(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let lang = match tokens[i].text.as_str() {
            "<%@" => "directive",
            "<%" => "code_block",
            "<%=" | "<%:" => "expression",
            "<%#" => "binding",
            "<%$" => "expression_builder",
            _ => {
                i += 1;
                continue;
            }
        };
        let close = (i + 1..tokens.len()).find(|&j| tokens[j].text == "%>");
        let end = close.unwrap_or(i);
        let name = (i + 1..end)
            .find(|&j| tokens[j].class == TokenClass::Identifier)
            .map_or_else(|| lang.to_string(), |j| tokens[j].text.clone());
        out.push(SymbolDecl {
            name,
            kind: SymbolKind::Other,
            lang_kind: Some(lang.into()),
            span: span_between(&tokens[i].span, &tokens[end].span),
        });
        i = end + 1;
    }
    out
}

#[cfg(test)]
mod tests;
