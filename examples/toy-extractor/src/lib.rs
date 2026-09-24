//! A worked example of a third-party language extractor, kept small enough to
//! read in one sitting. See `docs/adding-a-language.md`.
//!
//! It indexes INI files: every `[section]` becomes a `Module` symbol spanning
//! the section header through its last key, and every `key = value` line
//! becomes a `Variable` inside it. It depends only on `graph-core`.
use graph_core::scan::span_between;
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenDecl};

pub struct IniExtractor;

impl Extractor for IniExtractor {
    fn language(&self) -> &str {
        "ini"
    }

    /// Claimed extensions win over the built-in table, so `.cfg` files are
    /// indexed as INI too.
    fn extensions(&self) -> &[&str] {
        &["ini", "cfg"]
    }

    /// Bump the leading part whenever `extract` output changes; carry the
    /// tokenizer version so a tokenizer change re-indexes files as well.
    fn version(&self) -> String {
        format!("ini-scan-1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize_with(source, TokenizerOptions::default());
        let symbols = symbols(&tokens);
        Extraction {
            symbols,
            tokens,
            // Odd input never "fails": we just find fewer symbols.
            has_errors: false,
        }
    }
}

fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let mut out = Vec::new();
    // Tokens grouped by source line.
    let mut lines: Vec<&[TokenDecl]> = Vec::new();
    let mut start = 0;
    for i in 1..=tokens.len() {
        if i == tokens.len() || tokens[i].span.start_line != tokens[start].span.start_line {
            lines.push(&tokens[start..i]);
            start = i;
        }
    }
    // (header symbol index in `out`, last token of the section so far)
    let mut section: Option<(usize, &TokenDecl)> = None;
    for line in lines.into_iter().filter(|l| !l.is_empty()) {
        let (first, last) = (&line[0], &line[line.len() - 1]);
        if first.text == "[" && last.text == "]" && line.len() > 2 {
            close_section(&mut out, section.take());
            let name: String = line[1..line.len() - 1].iter().map(|t| &*t.text).collect();
            out.push(SymbolDecl {
                name,
                kind: SymbolKind::Module,
                lang_kind: Some("section".into()),
                span: span_between(&first.span, &last.span),
            });
            section = Some((out.len() - 1, last));
        } else if line.len() >= 2 && line[1].text == "=" {
            out.push(SymbolDecl {
                name: first.text.clone(),
                kind: SymbolKind::Variable,
                lang_kind: Some("key".into()),
                span: span_between(&first.span, &last.span),
            });
            if let Some((_, end)) = &mut section {
                *end = last;
            }
        }
    }
    close_section(&mut out, section);
    out
}

/// Stretch a section's span to its last key, so its keys nest inside it.
fn close_section(out: &mut [SymbolDecl], section: Option<(usize, &TokenDecl)>) {
    if let Some((i, end)) = section {
        out[i].span = span_between(&out[i].span, &end.span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_contain_their_keys() {
        let src = "top = 1\n[server]\nhost = example.org\nport = 80\n\n[client]\nretry = 3\n";
        let ex = IniExtractor.extract(src);
        let got: Vec<(&str, SymbolKind, &str)> = ex
            .symbols
            .iter()
            .map(|s| {
                let text = &src[s.span.start as usize..s.span.end as usize];
                (s.name.as_str(), s.kind, text)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("top", SymbolKind::Variable, "top = 1"),
                (
                    "server",
                    SymbolKind::Module,
                    "[server]\nhost = example.org\nport = 80"
                ),
                ("host", SymbolKind::Variable, "host = example.org"),
                ("port", SymbolKind::Variable, "port = 80"),
                ("client", SymbolKind::Module, "[client]\nretry = 3"),
                ("retry", SymbolKind::Variable, "retry = 3"),
            ]
        );
    }

    #[test]
    fn odd_input_degrades() {
        for src in ["", "[", "[]", "=", "[a\nb = ", "x ="] {
            let ex = IniExtractor.extract(src);
            assert!(!ex.has_errors);
        }
    }
}
