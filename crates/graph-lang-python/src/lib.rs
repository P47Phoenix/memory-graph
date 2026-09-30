//! Python extractor: a token-stream scanner over logical lines, not a parser.
//!
//! | Python | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `class Name:` | Type | `class` |
//! | `def` / `async def` (not directly in a class) | Function | `function` |
//! | `def` / `async def` directly in a class body | Method | `method` |
//! | `name = lambda ...` (any level) | Function | `lambda` |
//! | module-level `ALL_CAPS = ...` / `ALL_CAPS: T = ...` | Constant | `constant` |
//!
//! A `class`/`def` span runs from its first decorator (or the keyword)
//! through the last token of its indented block; an assignment's span is its
//! logical line. Blocks are measured on logical lines (the same rule as
//! `graph_core::scan::indent_block`, but a line continuing a bracket or a `\`
//! continuation never ends a block, whatever its column). A `def` nested in
//! a function is a Function. A file with unbalanced brackets or broken
//! indentation (an unexpected indent, a dedent to no enclosing level, a `:`
//! header without a block) is flagged `has_errors` and yields tokens only.
//! Columns count characters, so a tab is one column: indentation mixing tabs
//! and spaces inconsistently between lines usually sets `has_errors`, much as
//! Python raises `TabError`.
//! Not symbols: instance attributes, non-constant module variables, class
//! attributes other than lambdas, conditional imports.
use graph_core::scan::{code_index, mark_keywords, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct PythonExtractor;

/// Tokenizer dialect used for Python.
pub const PYTHON_TOKENIZER: TokenizerOptions = TokenizerOptions::PYTHON;

impl Extractor for PythonExtractor {
    fn language(&self) -> &str {
        "python"
    }

    fn extensions(&self) -> &[&str] {
        &["py", "pyw", "pyi"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        format!("python-scan-2+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, PYTHON_TOKENIZER);
        let code = code_index(&tokens, &[TokenClass::Comment]);
        let lines = logical_lines(&tokens, &code);
        let has_errors = has_syntax_errors(&tokens, &code, &lines);
        let symbols = if has_errors {
            vec![]
        } else {
            scan(&tokens, &code, &lines)
        };
        // After the symbol scan, which reads identifiers as it always has.
        // Python has no escaped identifiers.
        mark_keywords(&mut tokens, KEYWORDS, |_, _| false);
        Extraction {
            symbols,
            tokens,
            has_errors,
        }
    }
}

/// Python's hard keywords. Soft keywords (`match`, `case`, `type`, `_`) are
/// ordinary names outside their statements and stay identifiers.
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Symbols in Python tokens (as produced with [`PYTHON_TOKENIZER`]), best
/// effort even when the source has syntax errors.
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let code = code_index(tokens, &[TokenClass::Comment]);
    let lines = logical_lines(tokens, &code);
    scan(tokens, &code, &lines)
}

/// Logical lines as ranges of code positions: a line starts at the first
/// code token on a physical line, unless a bracket is open or the previous
/// line ended in a `\` continuation.
fn logical_lines(tokens: &[TokenDecl], code: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut starts = Vec::new();
    let mut depth = 0usize;
    for (c, &i) in code.iter().enumerate() {
        let t = &tokens[i];
        let new_line = match c.checked_sub(1) {
            None => true,
            Some(p) => {
                let prev = &tokens[code[p]];
                t.span.start_line > prev.span.end_line && prev.text != "\\"
            }
        };
        if new_line && depth == 0 {
            starts.push(c);
        }
        if t.class != TokenClass::Literal {
            match t.text.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(code.len());
        out.push(s..e);
    }
    out
}

/// Unbalanced brackets or broken indentation.
fn has_syntax_errors(
    tokens: &[TokenDecl],
    code: &[usize],
    lines: &[std::ops::Range<usize>],
) -> bool {
    let mut stack = Vec::new();
    for &i in code {
        let t = &tokens[i];
        if t.class == TokenClass::Literal {
            continue;
        }
        match t.text.as_str() {
            "(" => stack.push(")"),
            "[" => stack.push("]"),
            "{" => stack.push("}"),
            ")" | "]" | "}" if stack.pop() != Some(t.text.as_str()) => return true,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return true;
    }
    // The indentation stack of the Python tokenizer: after a line ending in
    // `:` the next line must indent; otherwise it must return to a level on
    // the stack.
    let mut levels = vec![1u32];
    let mut block_expected = false;
    for line in lines {
        let col = tokens[code[line.start]].span.start_col;
        let top = *levels.last().unwrap_or(&1);
        if block_expected {
            if col <= top {
                return true;
            }
            levels.push(col);
        } else if col > top {
            return true;
        } else {
            while levels.last().is_some_and(|&l| l > col) {
                levels.pop();
            }
            if levels.last() != Some(&col) {
                return true;
            }
        }
        block_expected = tokens[code[line.end - 1]].text == ":";
    }
    block_expected
}

fn scan(tokens: &[TokenDecl], code: &[usize], lines: &[std::ops::Range<usize>]) -> Vec<SymbolDecl> {
    let text = |c: usize| tokens[code[c]].text.as_str();
    let is_ident = |c: usize| tokens[code[c]].class == TokenClass::Identifier;
    let span = |a: usize, b: usize| span_between(&tokens[code[a]].span, &tokens[b].span);
    let mut out = Vec::new();
    // Open class/def blocks: (last token index, is a class).
    let mut open: Vec<(usize, bool)> = Vec::new();
    // First code position of pending decorator lines.
    let mut decorators: Option<usize> = None;
    for (k, line) in lines.iter().enumerate() {
        let s = line.start;
        while open.last().is_some_and(|&(end, _)| end < code[s]) {
            open.pop();
        }
        if text(s) == "@" {
            decorators.get_or_insert(s);
            continue;
        }
        let start = decorators.take().unwrap_or(s);
        let mut c = s;
        if text(c) == "async" && c + 1 < line.end {
            c += 1;
        }
        let kw = text(c);
        if matches!(kw, "def" | "class") && c + 1 < line.end && is_ident(c + 1) {
            // The block: every later logical line indented past the header.
            let indent = tokens[code[s]].span.start_col;
            let block_last = lines[k + 1..]
                .iter()
                .take_while(|l| tokens[code[l.start]].span.start_col > indent)
                .last()
                .unwrap_or(line);
            let end = code[block_last.end - 1];
            let class = kw == "class";
            let (kind, lang) = if class {
                (SymbolKind::Type, "class")
            } else if open.last().is_some_and(|&(_, is_class)| is_class) {
                (SymbolKind::Method, "method")
            } else {
                (SymbolKind::Function, "function")
            };
            out.push(SymbolDecl {
                owner: None,
                name: text(c + 1).to_string(),
                kind,
                lang_kind: Some(lang.into()),
                span: span(start, end),
            });
            open.push((end, class));
            continue;
        }
        // Assignments: `name = ...` or `name: T = ...`.
        if !is_ident(s) || s + 1 >= line.end {
            continue;
        }
        let eq = (s + 1..line.end).find(|&e| {
            text(e) == "="
                && !(e + 1 < line.end && text(e + 1) == "=")
                && !matches!(text(e - 1), "=" | "!" | "<" | ">" | ":")
        });
        let simple = text(s + 1) == "=" || text(s + 1) == ":";
        let Some(eq) = eq.filter(|_| simple) else {
            continue;
        };
        let name = text(s);
        let last = code[line.end - 1];
        if eq + 1 < line.end && text(eq + 1) == "lambda" && (text(s + 1) == "=" || eq > s + 2) {
            out.push(SymbolDecl {
                owner: None,
                name: name.to_string(),
                kind: SymbolKind::Function,
                lang_kind: Some("lambda".into()),
                span: span(s, last),
            });
        } else if open.is_empty() && is_constant_name(name) {
            out.push(SymbolDecl {
                owner: None,
                name: name.to_string(),
                kind: SymbolKind::Constant,
                lang_kind: Some("constant".into()),
                span: span(s, last),
            });
        }
    }
    out
}

/// `MAX_SIZE`, `_PRIVATE`, `V2`: upper case, digits and `_`, with a letter.
fn is_constant_name(name: &str) -> bool {
    name.bytes().any(|b| b.is_ascii_uppercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests;
