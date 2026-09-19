//! Generic fallback tokenizer: language-agnostic, never fails.
use crate::schema::{Span, TokenClass, TokenDecl};

const OPERATOR_CHARS: &str = "+-*/%=<>!&|^~?@";

/// Tokenize everything except whitespace. Unterminated strings and comments
/// run to end of input (or line, for `//`).
pub fn tokenize(src: &str) -> Vec<TokenDecl> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1u32, 1u32);
    // Advance position tracking over src[from..to].
    let adv = |from: usize, to: usize, line: &mut u32, col: &mut u32| {
        for c in src[from..to].chars() {
            if c == '\n' {
                *line += 1;
                *col = 1;
            } else {
                *col += 1;
            }
        }
    };
    while i < b.len() {
        let c = src[i..].chars().next().unwrap();
        if c.is_whitespace() {
            adv(i, i + c.len_utf8(), &mut line, &mut col);
            i += c.len_utf8();
            continue;
        }
        let rest = &src[i..];
        let (len, class) = if rest.starts_with("//") {
            (rest.find('\n').unwrap_or(rest.len()), TokenClass::Comment)
        } else if let Some(after) = rest.strip_prefix("/*") {
            (
                after.find("*/").map_or(rest.len(), |p| p + 4),
                TokenClass::Comment,
            )
        } else if c.is_alphabetic() || c == '_' {
            let n = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            (n, TokenClass::Identifier)
        } else if c.is_ascii_digit() {
            let n = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.'))
                .unwrap_or(rest.len());
            // Don't swallow a trailing range/method dot, e.g. `1..2` or `1.`.
            let mut n = n;
            while n > 1 && rest[..n].ends_with('.') {
                n -= 1;
            }
            (n, TokenClass::Literal)
        } else if c == '"' || c == '`' || (c == '\'' && is_char_literal(rest)) {
            (quoted_len(rest, c), TokenClass::Literal)
        } else if OPERATOR_CHARS.contains(c) {
            (c.len_utf8(), TokenClass::Operator)
        } else {
            (c.len_utf8(), TokenClass::Punctuation)
        };
        let end = i + len;
        let (sl, sc) = (line, col);
        adv(i, end, &mut line, &mut col);
        out.push(TokenDecl {
            text: src[i..end].to_string(),
            class,
            span: Span {
                start: i as u32,
                end: end as u32,
                start_line: sl,
                start_col: sc,
                end_line: line,
                end_col: col,
            },
        });
        i = end;
    }
    out
}

/// Byte length of a quoted literal starting at `rest[0] == q`, honoring
/// backslash escapes; unterminated runs to end of input.
fn quoted_len(rest: &str, q: char) -> usize {
    let mut esc = false;
    for (p, ch) in rest.char_indices().skip(1) {
        if esc {
            esc = false;
        } else if ch == '\\' {
            esc = true;
        } else if ch == q {
            return p + ch.len_utf8();
        }
    }
    rest.len()
}

/// `'x'` or `'\..'` is a char literal; otherwise a lone `'` (lifetime,
/// apostrophe) is punctuation.
fn is_char_literal(rest: &str) -> bool {
    let mut it = rest.chars().skip(1);
    match it.next() {
        Some('\\') => rest.chars().skip(2).take(10).any(|c| c == '\''),
        Some(c) if c != '\'' && c != '\n' => it.next() == Some('\''),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn texts(s: &str) -> Vec<String> {
        tokenize(s).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn zig_like() {
        let t = texts("pub fn add(a: i32) i32 { return a + 1; } // done");
        assert_eq!(t[..3], ["pub", "fn", "add"]);
        assert_eq!(t.last().unwrap(), "// done");
    }

    #[test]
    fn unterminated() {
        assert_eq!(texts("x \"abc"), ["x", "\"abc"]);
        assert_eq!(texts("x /* abc"), ["x", "/* abc"]);
        assert_eq!(texts("fn f<'a>()")[3..5], ["'", "a"]);
    }

    #[test]
    fn positions() {
        let t = tokenize("ab\n  é cd");
        assert_eq!((t[1].span.start_line, t[1].span.start_col), (2, 3));
        assert_eq!((t[2].span.start_line, t[2].span.start_col), (2, 5));
        assert_eq!(t[2].span.start, 8);
    }

    proptest! {
        #[test]
        fn spans_match_source(src in "\\PC{0,200}") {
            let toks = tokenize(&src);
            let mut prev_end = 0usize;
            for t in &toks {
                let (s, e) = (t.span.start as usize, t.span.end as usize);
                prop_assert_eq!(&src[s..e], t.text.as_str());
                prop_assert!(s >= prev_end);
                // Gap between tokens is whitespace only.
                prop_assert!(src[prev_end..s].chars().all(char::is_whitespace));
                // Independent line/col computation.
                let before = &src[..s];
                let line = 1 + before.matches('\n').count() as u32;
                let col = 1 + before.rsplit('\n').next().unwrap().chars().count() as u32;
                prop_assert_eq!((t.span.start_line, t.span.start_col), (line, col));
                let upto = &src[..e];
                let el = 1 + upto.matches('\n').count() as u32;
                let ec = 1 + upto.rsplit('\n').next().unwrap().chars().count() as u32;
                prop_assert_eq!((t.span.end_line, t.span.end_col), (el, ec));
                prev_end = e;
            }
            prop_assert!(src[prev_end..].chars().all(char::is_whitespace));
        }
    }
}
