//! Generic fallback tokenizer: language-agnostic, never fails.
use crate::schema::{Span, TokenClass, TokenDecl};

/// Version of the tokenizer's output. Bump it whenever a change alters the
/// tokens `tokenize` returns for any input; it is folded into extractor
/// versions (and so file fingerprints) so stored files re-index. The golden
/// test `tokenizer_output_is_pinned` fails when output changes without a bump.
pub const TOKENIZER_VERSION: u32 = 2;

const OPERATOR_CHARS: &str = "+-*/%=<>!&|^~?@";

/// Tokenize everything except whitespace (a BOM counts as whitespace; offsets
/// stay relative to the original text). Callers must ensure `src.len() <= u32::MAX`
/// (spans are `u32`); `Store::index_bytes` enforces this.
/// Unterminated strings and comments
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
            } else if c != '\u{feff}' {
                *col += 1;
            }
        }
    };
    while i < b.len() {
        let c = src[i..].chars().next().unwrap();
        if c.is_whitespace() || c == '\u{feff}' {
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
        } else if let Some(n) = prefixed_literal_len(rest) {
            (n, TokenClass::Literal)
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

/// Length of a Rust-style prefixed literal at the start of `rest`: raw strings
/// `r"..."`, `r#"..."#` (any number of hashes; the closing quote needs the same
/// number), `br#"..."#`, and byte literals `b"..."`, `b'x'`. The prefix only
/// counts when the quote/hash sequence follows immediately, so identifiers such
/// as `r`, `br`, `bar` and raw identifiers (`r#type`) are unaffected. An
/// unterminated raw string runs to end of input.
fn prefixed_literal_len(rest: &str) -> Option<usize> {
    let b = rest.as_bytes();
    let mut p = usize::from(b[0] == b'b');
    if b.get(p) == Some(&b'r') {
        p += 1;
        let hashes = b[p..].iter().take_while(|&&c| c == b'#').count();
        p += hashes;
        if b.get(p) != Some(&b'"') {
            return None;
        }
        let body = p + 1;
        let mut close = vec![b'"'];
        close.extend(std::iter::repeat_n(b'#', hashes));
        return Some(
            rest.as_bytes()[body..]
                .windows(close.len())
                .position(|w| w == close.as_slice())
                .map_or(rest.len(), |q| body + q + close.len()),
        );
    }
    if b[0] != b'b' {
        return None;
    }
    match b.get(1) {
        Some(b'"') => Some(1 + quoted_len(&rest[1..], '"')),
        Some(b'\'') if is_char_literal(&rest[1..]) => Some(1 + quoted_len(&rest[1..], '\'')),
        _ => None,
    }
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

    fn is_gap(c: char) -> bool {
        c.is_whitespace() || c == '\u{feff}'
    }

    fn texts(s: &str) -> Vec<String> {
        tokenize(s).into_iter().map(|t| t.text).collect()
    }

    const GOLDEN: u64 = 0x7f377ab72ee35a84;
    const GOLDEN_VERSION: u32 = 2;

    /// Golden test: pins tokenizer output for a fixed source set.
    #[test]
    fn tokenizer_output_is_pinned() {
        const SOURCES: &[&str] = &[
            "fn main() { let x = 1 + 2; // hi\n}\n",
            "/* block */ \"str\" 'c' 0xFF 1.5e3 a::b -> c",
            "\u{feff}def f(a, b):\n    return a >= b  # cmp\n",
            "unterminated \"string",
            "/* unterminated",
            "héllo wörld = ünï",
            "const A: &str = r#\"a\"b\"#; let b = br##\"x\"#y\"##; b\"by\\\"te\" b'x' r\"raw\\\"",
            "let r = br + bar; r#type unterminated r#\"never closed",
        ];
        // FNV-1a 64 over the Debug rendering of every token.
        let mut h: u64 = 0xcbf29ce484222325;
        for src in SOURCES {
            for b in format!("{:?}", tokenize(src)).bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
            }
        }
        assert_eq!(
            h, GOLDEN,
            "tokenizer output changed (hash {h:#x}): bump TOKENIZER_VERSION in \
             graph-core/src/tokenizer.rs and update GOLDEN in this test"
        );
        assert_eq!(
            TOKENIZER_VERSION, GOLDEN_VERSION,
            "update GOLDEN_VERSION with the bump"
        );
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
    fn bom_is_not_a_token() {
        let t = tokenize("\u{feff}foo bar");
        assert_eq!(t[0].text, "foo");
        assert_eq!((t[0].span.start, t[0].span.start_col), (3, 1)); // BOM takes no column
    }

    #[test]
    fn positions() {
        let t = tokenize("ab\n  é cd");
        assert_eq!((t[1].span.start_line, t[1].span.start_col), (2, 3));
        assert_eq!((t[2].span.start_line, t[2].span.start_col), (2, 5));
        assert_eq!(t[2].span.start, 8);
    }

    fn lit(s: &str) -> Vec<(String, TokenClass)> {
        tokenize(s).into_iter().map(|t| (t.text, t.class)).collect()
    }

    #[test]
    fn raw_and_byte_strings() {
        use TokenClass::Literal;
        let one = |s: &str| assert_eq!(lit(s), [(s.to_string(), Literal)], "{s}");
        one("r\"a\"");
        one("r#\"a\"b\"#");
        one("r###\"a\"## \"#b\"###");
        one("br#\"a\"b\"#");
        one("b\"by\\\"te\"");
        one("b'x'");
        one("b'\\n'");
        // Backslash is not an escape in raw strings.
        assert_eq!(texts("r\"a\\\" x"), ["r\"a\\\"", "x"]);
        // Closing needs the same number of hashes; a longer run still closes.
        assert_eq!(texts("r##\"a\"# b\"## c"), ["r##\"a\"# b\"##", "c"]);
        // Unterminated raw strings run to end of input.
        assert_eq!(texts("x r#\"abc\" \"#"), ["x", "r#\"abc\" \"#"]);
        assert_eq!(texts("r\"abc"), ["r\"abc"]);
    }

    #[test]
    fn prefixes_need_the_quote_immediately() {
        use TokenClass::Identifier;
        for id in ["r", "br", "bar", "b", "rb", "raw", "r_", "brr"] {
            assert_eq!(lit(id), [(id.to_string(), Identifier)], "{id}");
        }
        assert_eq!(texts("r #\"a\"#")[0], "r");
        assert_eq!(texts("r#type"), ["r", "#", "type"]);
        assert_eq!(texts("b 'x'"), ["b", "'x'"]);
        assert_eq!(texts("br#x"), ["br", "#", "x"]);
        // Identifier ending in r/b does not start a raw string.
        assert_eq!(texts("bar\"x\""), ["bar", "\"x\""]);
        assert_eq!(texts("b'a"), ["b", "'", "a"]);
    }

    proptest! {
        #[test]
        fn spans_match_source_raw_heavy(
            src in "([ \\n]|r|b|br|#{0,3}|\"|'|x|\\\\|é|/\\*|//){0,40}"
        ) {
            check_spans(&src)?;
        }

        #[test]
        fn raw_string_shapes(n in 0usize..4, m in 0usize..5, body in "[a-z\"# ]{0,12}", pre in "(r|br|b|)") {
            let src = format!("{pre}{h}\"{body}\"{t} tail", h = "#".repeat(n), t = "#".repeat(m));
            check_spans(&src)?;
        }


        #[test]
        fn spans_match_source(src in "\\PC{0,200}") {
            check_spans(&src)?;
        }
    }

    fn check_spans(src: &str) -> Result<(), TestCaseError> {
        let src = src.to_string();
        let toks = tokenize(&src);
        let mut prev_end = 0usize;
        for t in &toks {
            let (s, e) = (t.span.start as usize, t.span.end as usize);
            prop_assert_eq!(&src[s..e], t.text.as_str());
            prop_assert!(s >= prev_end);
            // Gap between tokens is whitespace only.
            prop_assert!(src[prev_end..s].chars().all(is_gap));
            // Independent line/col computation.
            let before = &src[..s];
            let line = 1 + before.matches('\n').count() as u32;
            let col = 1 + before
                .rsplit('\n')
                .next()
                .unwrap()
                .chars()
                .filter(|&c| c != '\u{feff}')
                .count() as u32;
            prop_assert_eq!((t.span.start_line, t.span.start_col), (line, col));
            let upto = &src[..e];
            let el = 1 + upto.matches('\n').count() as u32;
            let ec = 1 + upto
                .rsplit('\n')
                .next()
                .unwrap()
                .chars()
                .filter(|&c| c != '\u{feff}')
                .count() as u32;
            prop_assert_eq!((t.span.end_line, t.span.end_col), (el, ec));
            prev_end = e;
        }
        prop_assert!(src[prev_end..].chars().all(is_gap));
        Ok(())
    }
}
