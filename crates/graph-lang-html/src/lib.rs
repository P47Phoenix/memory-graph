//! HTML extractor, plus the reusable markup element scanner ([`scan_elements`])
//! that the ASP.NET extractor builds on.
//!
//! Symbols (all `SymbolKind::Other`): elements with an `id` attribute
//! (`lang_kind` `element`, name = the id), and every `<script>`, `<style>`,
//! `<form>` and `<template>` (`lang_kind` = the tag, name = id or tag).
//! Attributes are not symbols.
//!
//! Span rule, which always yields validly nested spans: a stack of open tags;
//! a close tag pops to the nearest open tag with the same name. A matched
//! element spans its start tag through its close tag; anything popped without
//! a match, void and self-closing elements, and elements never closed span
//! only their own start tag.
use graph_core::scan::span_between;
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct HtmlExtractor;

/// Tokenizer dialect used for HTML.
pub const HTML_TOKENIZER: TokenizerOptions = TokenizerOptions {
    rust_literals: false,
    single_quote_strings: false,
    csharp_strings: false,
    markup: true,
    aspx: false,
    regex_literals: false,
};

impl Extractor for HtmlExtractor {
    fn language(&self) -> &str {
        "html"
    }

    fn extensions(&self) -> &[&str] {
        &["html", "htm", "xhtml"]
    }

    fn version(&self) -> String {
        format!("html-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, HTML_TOKENIZER);
        let symbols = scan_elements(&tokens, &[], html_symbol);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// The HTML rule: `(name, lang_kind)` for a start tag, or `None`.
pub fn html_symbol(tag: &Tag) -> Option<(String, String)> {
    let lower = tag.name.to_ascii_lowercase();
    let id = tag.attr("id");
    if matches!(lower.as_str(), "script" | "style" | "form" | "template") {
        return Some((id.unwrap_or(&tag.name).to_string(), lower));
    }
    id.map(|id| (id.to_string(), "element".into()))
}

/// A start tag as seen by the scanner.
#[derive(Debug)]
pub struct Tag {
    /// Tag name as written (`div`, `asp:Button`).
    pub name: String,
    /// Attributes in order: (name, unquoted value; empty if none).
    pub attrs: Vec<(String, String)>,
    /// Token indices of the `<` and the closing `>`.
    pub start: usize,
    pub end: usize,
    pub self_closing: bool,
}

impl Tag {
    /// Value of an attribute (name compared case-insensitively).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr", "param", "keygen",
];

/// Elements whose content is raw text: no tags are recognized inside.
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title"];

/// Find elements and return a symbol for each start tag `classify` accepts.
///
/// `opaque` lists (open, close) token texts of regions the scanner must skip
/// as a whole, e.g. `("<%", "%>")` for server blocks; a region may appear
/// inside a start tag. Token indices in `Tag` refer to `tokens`.
pub fn scan_elements(
    tokens: &[TokenDecl],
    opaque: &[(&str, &str)],
    classify: impl Fn(&Tag) -> Option<(String, String)>,
) -> Vec<SymbolDecl> {
    // (lowercased name, start-tag token index, start-tag end index, symbol slot)
    let mut stack: Vec<(String, usize, usize, Option<usize>)> = Vec::new();
    let mut out: Vec<SymbolDecl> = Vec::new();
    let span = |a: usize, b: usize| span_between(&tokens[a].span, &tokens[b].span);
    let mut i = 0;
    while i < tokens.len() {
        if let Some(j) = skip_opaque(tokens, i, opaque) {
            i = j;
            continue;
        }
        let t = &tokens[i];
        if t.text != "<" || t.class == TokenClass::Comment {
            i += 1;
            continue;
        }
        // End tag: `<` `/` name ... `>`.
        if let Some(close) = end_tag(tokens, i, opaque) {
            let (name, end) = close;
            if let Some(pos) = stack.iter().rposition(|e| e.0 == name) {
                // Unmatched inner tags keep their start-tag spans.
                stack.truncate(pos + 1);
                let (_, start, _, slot) = stack.pop().unwrap();
                if let Some(s) = slot {
                    out[s].span = span(start, end);
                }
            }
            i = end + 1;
            continue;
        }
        let Some(tag) = start_tag(tokens, i, opaque) else {
            i += 1;
            continue;
        };
        let slot = classify(&tag).map(|(name, lang)| {
            out.push(SymbolDecl {
                name,
                kind: SymbolKind::Other,
                lang_kind: Some(lang),
                span: span(tag.start, tag.end),
            });
            out.len() - 1
        });
        let lower = tag.name.to_ascii_lowercase();
        i = tag.end + 1;
        if tag.self_closing || VOID.contains(&lower.as_str()) {
            continue;
        }
        if RAW_TEXT.contains(&lower.as_str()) {
            // Jump to the matching close tag, if any; else treat as unclosed.
            let mut j = i;
            while j < tokens.len() {
                if let Some(k) = skip_opaque(tokens, j, opaque) {
                    j = k;
                    continue;
                }
                if tokens[j].text == "<" {
                    if let Some((name, end)) = end_tag(tokens, j, opaque) {
                        if name == lower {
                            if let Some(s) = slot {
                                out[s].span = span(tag.start, end);
                            }
                            i = end + 1;
                            break;
                        }
                    }
                }
                j += 1;
            }
            if j >= tokens.len() {
                i = tag.end + 1;
                // Unclosed raw-text element: keep scanning after its start tag.
            }
            continue;
        }
        stack.push((lower, tag.start, tag.end, slot));
    }
    out
}

/// If an opaque region starts at `i`, the index after its end (or the end of
/// input when it never closes).
fn skip_opaque(tokens: &[TokenDecl], i: usize, opaque: &[(&str, &str)]) -> Option<usize> {
    let t = &tokens[i].text;
    let (_, close) = opaque
        .iter()
        .find(|(o, _)| t.starts_with(o) && t.len() <= o.len() + 1)?;
    let end = (i + 1..tokens.len()).find(|&j| tokens[j].text == *close);
    Some(end.map_or(tokens.len(), |j| j + 1))
}

fn adjacent(tokens: &[TokenDecl], a: usize, b: usize) -> bool {
    b < tokens.len() && tokens[a].span.end == tokens[b].span.start
}

/// `</name ... >` at `i`: (lowercased name, index of `>`).
fn end_tag(tokens: &[TokenDecl], i: usize, opaque: &[(&str, &str)]) -> Option<(String, usize)> {
    if !(adjacent(tokens, i, i + 1) && tokens[i + 1].text == "/") {
        return None;
    }
    let name = i + 2;
    if !adjacent(tokens, i + 1, name) || tokens[name].class != TokenClass::Identifier {
        return None;
    }
    let end = tag_close(tokens, name + 1, opaque)?;
    Some((tokens[name].text.to_ascii_lowercase(), end))
}

/// `<name attrs... [/]>` at `i`.
fn start_tag(tokens: &[TokenDecl], i: usize, opaque: &[(&str, &str)]) -> Option<Tag> {
    let n = i + 1;
    if !adjacent(tokens, i, n) || tokens[n].class != TokenClass::Identifier {
        return None;
    }
    let end = tag_close(tokens, n + 1, opaque)?;
    let mut attrs = Vec::new();
    let mut j = n + 1;
    while j < end {
        if let Some(k) = skip_opaque(tokens, j, opaque) {
            j = k;
            continue;
        }
        let t = &tokens[j];
        if t.class == TokenClass::Identifier {
            let mut value = String::new();
            if j + 2 < end && tokens[j + 1].text == "=" {
                let v = &tokens[j + 2];
                value = v.text.trim_matches(|c| c == '"' || c == '\'').to_string();
                j += 2;
            }
            attrs.push((t.text.clone(), value));
        }
        j += 1;
    }
    Some(Tag {
        name: tokens[n].text.clone(),
        attrs,
        start: i,
        end,
        self_closing: tokens[end - 1].text == "/" && adjacent(tokens, end - 1, end),
    })
}

/// Index of the `>` closing a tag, scanning from `from`; `None` if a new `<`
/// tag starts first or input ends.
fn tag_close(tokens: &[TokenDecl], from: usize, opaque: &[(&str, &str)]) -> Option<usize> {
    let mut j = from;
    while j < tokens.len() {
        if let Some(k) = skip_opaque(tokens, j, opaque) {
            j = k;
            continue;
        }
        match tokens[j].text.as_str() {
            ">" => return Some(j),
            "<" => return None,
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests;
