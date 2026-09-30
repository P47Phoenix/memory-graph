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
//! Server-side C#: the body of a closed `<script runat="server">` element is
//! tokenized as C# (its tokens replace the markup tokens, with spans in file
//! coordinates) and becomes a `SymbolKind::Module` (`lang_kind` and name
//! `server_script`) spanning its first through last token, holding the
//! symbols the C# extractor (`graph-lang-csharp`) finds in it as members of
//! the page class (nested classes, fields, properties, methods, ...). A
//! server script that is never closed, or whose body holds `<%`/`%>`, stays
//! markup. Only C# scripts are scanned: the language is the script's
//! `language` attribute, else the `<%@ Page/Control/Master Language=... %>`
//! directive's, and must be `C#`, `cs` or `csharp` (any case). With neither,
//! Web Forms compiles the script as VB, so it stays markup, as do VB (and
//! other) scripts. As in ASP.NET, the first `</script>` ends the body even
//! inside a C# string (`"</script>"`). `<% ... %>` blocks are not scanned for C# symbols: they hold the
//! statements of the page's render method, where a scanner would read
//! `if (x) {` or `int n = 0;` as declarations.
//!
//! Element spans follow the HTML scanner's rule;
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
    csharp_strings: true,
    markup: true,
    aspx: true,
    ..TokenizerOptions::DEFAULT
};

impl Extractor for AspxExtractor {
    fn language(&self) -> &str {
        "aspx"
    }

    fn extensions(&self) -> &[&str] {
        &["aspx", "ascx", "master"]
    }

    fn version(&self) -> String {
        format!("aspx-scan-2+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let markup = tokenize_with(source, ASPX_TOKENIZER);
        let (tokens, scripts) = server_scripts(source, markup.clone());
        let with_scripts = extraction(tokens, scripts);
        if well_nested(&with_scripts) {
            return with_scripts;
        }
        // Server script bodies re-tokenized as C# confused the markup
        // scanner (never seen on real pages): keep them as markup.
        extraction(markup, Vec::new())
    }
}

fn extraction(tokens: Vec<TokenDecl>, mut symbols: Vec<SymbolDecl>) -> Extraction {
    symbols.extend(server_blocks(&tokens));
    symbols.extend(scan_elements(&tokens, &[("<%", "%>")], aspx_symbol));
    symbols.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
    Extraction {
        symbols,
        tokens,
        has_errors: false,
    }
}

/// Every symbol and token span nests in or is disjoint from each enclosing
/// symbol (what the store requires). `symbols` are sorted by start, longest
/// first.
fn well_nested(ex: &Extraction) -> bool {
    let mut open: Vec<u32> = Vec::new();
    let mut t = 0;
    for s in &ex.symbols {
        while t < ex.tokens.len() && ex.tokens[t].span.start < s.span.start {
            let sp = ex.tokens[t].span;
            while open.last().is_some_and(|&e| e <= sp.start) {
                open.pop();
            }
            if open.last().is_some_and(|&e| sp.end > e) {
                return false;
            }
            t += 1;
        }
        while open.last().is_some_and(|&e| e <= s.span.start) {
            open.pop();
        }
        if open.last().is_some_and(|&e| s.span.end > e) {
            return false;
        }
        open.push(s.span.end);
    }
    for tok in &ex.tokens[t..] {
        while open.last().is_some_and(|&e| e <= tok.span.start) {
            open.pop();
        }
        if open.last().is_some_and(|&e| tok.span.end > e) {
            return false;
        }
    }
    true
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
    // `next_close[i]`: the first `%>` after `i`, found in one backward
    // pass (a forward search per `<%` is quadratic on a long unclosed run).
    let mut next_close = vec![None; tokens.len()];
    for j in (1..tokens.len()).rev() {
        next_close[j - 1] = if tokens[j].text == "%>" {
            Some(j)
        } else {
            next_close[j]
        };
    }
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
        let close = next_close[i];
        let end = close.unwrap_or(i);
        let name = (i + 1..end)
            .find(|&j| tokens[j].class == TokenClass::Identifier)
            .map_or_else(|| lang.to_string(), |j| tokens[j].text.clone());
        out.push(SymbolDecl {
            owner: None,
            name,
            kind: SymbolKind::Other,
            lang_kind: Some(lang.into()),
            span: span_between(&tokens[i].span, &tokens[end].span),
        });
        i = end + 1;
    }
    out
}

/// Re-tokenizes the body of every closed `<script runat="server">` element
/// as C#: returns the tokens (markup tokens outside the bodies, C# tokens
/// inside, all in file coordinates) and a `server_script` module plus the C#
/// symbols for each body.
fn is_csharp(lang: &str) -> bool {
    ["c#", "cs", "csharp"]
        .iter()
        .any(|l| lang.trim().eq_ignore_ascii_case(l))
}

/// The `Language` attribute of the first `<%@ Page %>` / `<%@ Control %>`
/// (or `Master`) directive.
fn directive_language(source: &str, tokens: &[TokenDecl]) -> Option<String> {
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i].text == "<%@"
            && ["page", "control", "master"]
                .iter()
                .any(|d| tokens[i + 1].text.eq_ignore_ascii_case(d))
        {
            let mut j = i + 2;
            while j + 2 < tokens.len() && tokens[j].text != "%>" {
                if tokens[j].text.eq_ignore_ascii_case("language") && tokens[j + 1].text == "=" {
                    let v = &tokens[j + 2];
                    if v.text == "'" {
                        // Server-tag code lexes `'C#'` as `'` `C` `#` `'`.
                        let close = (j + 3..tokens.len()).find(|&k| tokens[k].text == "'")?;
                        let (a, b) = (v.span.end as usize, tokens[close].span.start as usize);
                        return source.get(a..b).map(str::to_string);
                    }
                    return Some(v.text.trim_matches(|c| c == '"' || c == '\'').to_string());
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    None
}

fn server_scripts(source: &str, tokens: Vec<TokenDecl>) -> (Vec<TokenDecl>, Vec<SymbolDecl>) {
    // (index of the start tag's `>`, index of the end tag's `<`)
    let bodies = std::cell::RefCell::new(Vec::new());
    let page_lang = directive_language(source, &tokens);
    scan_elements(&tokens, &[("<%", "%>")], |tag| {
        let lang = tag.attr("language").or(page_lang.as_deref());
        let server = tag.name.eq_ignore_ascii_case("script")
            && !tag.self_closing
            && lang.is_some_and(is_csharp)
            && tag
                .attr("runat")
                .is_some_and(|v| v.eq_ignore_ascii_case("server"));
        if server {
            if let Some(close) = script_close(&tokens, tag.end + 1) {
                bodies.borrow_mut().push((tag.end, close));
            }
        }
        None
    });
    let bodies = bodies.into_inner();
    if bodies.is_empty() {
        return (tokens, Vec::new());
    }
    let mut out = Vec::with_capacity(tokens.len());
    let mut symbols = Vec::new();
    let mut next = 0;
    for (gt, lt) in bodies {
        if gt < next {
            continue; // cannot happen (bodies are disjoint); be safe
        }
        out.extend_from_slice(&tokens[next..=gt]);
        let (from, to) = (tokens[gt].span.end, tokens[lt].span.start);
        let base = (tokens[gt].span.end_line, tokens[gt].span.end_col);
        let mut code = tokenize_with(
            &source[from as usize..to as usize],
            graph_lang_csharp::CSHARP_TOKENIZER,
        );
        for t in &mut code {
            rebase(&mut t.span, from, base);
        }
        if let (Some(first), Some(last)) = (code.first(), code.last()) {
            symbols.push(SymbolDecl {
                name: "server_script".into(),
                kind: SymbolKind::Module,
                lang_kind: Some("server_script".into()),
                span: span_between(&first.span, &last.span),
                owner: None,
            });
            symbols.extend(graph_lang_csharp::member_symbols(&code));
        }
        out.extend(code);
        next = lt;
    }
    out.extend_from_slice(&tokens[next..]);
    (out, symbols)
}

/// Index of the `<` of the first `</script ... >` after `from` (the HTML
/// rule for a raw-text element's end), or `None` when the element is never
/// closed or its body holds a server tag (`<%`, `%>`): the body is then left
/// as markup, so the element scanner sees exactly the tokens it saw before.
fn script_close(tokens: &[TokenDecl], from: usize) -> Option<usize> {
    let adjacent = |a: usize, b: usize| tokens[a].span.end == tokens[b].span.start;
    let mut i = from;
    while i < tokens.len() {
        let t = &tokens[i].text;
        if t.starts_with("<%") || t == "%>" {
            return None;
        }
        if t == "<"
            && i + 2 < tokens.len()
            && tokens[i + 1].text == "/"
            && tokens[i + 2].text.eq_ignore_ascii_case("script")
            && adjacent(i, i + 1)
            && adjacent(i + 1, i + 2)
        {
            // The end tag needs its `>` (before any other `<`), as in HTML.
            let rest = &tokens[i + 3..];
            return match rest.iter().position(|t| t.text == ">" || t.text == "<") {
                Some(p) if rest[p].text == ">" => Some(i),
                _ => None,
            };
        }
        i += 1;
    }
    None
}

/// Moves a span tokenized from a substring starting at byte `from`, line and
/// column `base`, to file coordinates.
fn rebase(span: &mut graph_core::Span, from: u32, base: (u32, u32)) {
    span.start += from;
    span.end += from;
    if span.start_line == 1 {
        span.start_col += base.1 - 1;
    }
    if span.end_line == 1 {
        span.end_col += base.1 - 1;
    }
    span.start_line += base.0 - 1;
    span.end_line += base.0 - 1;
}

#[cfg(test)]
mod tests;
