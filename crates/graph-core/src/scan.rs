//! Generic helpers for token-stream extractors: a cursor over tokens, a
//! balanced-delimiter matcher and span joining. No language knowledge lives
//! here; extractors decide what the tokens mean.
use crate::schema::{Span, TokenClass, TokenDecl};

/// Span running from the start of `first` to the end of `last`. `last` must
/// not end before `first` starts.
pub fn span_between(first: &Span, last: &Span) -> Span {
    debug_assert!(
        first.start <= last.end,
        "span_between: {first:?} > {last:?}"
    );
    Span {
        start: first.start,
        end: last.end,
        start_line: first.start_line,
        start_col: first.start_col,
        end_line: last.end_line,
        end_col: last.end_col,
    }
}

/// Index of the token closing the delimiter at `open`, counting only tokens
/// whose text is exactly one of the delimiter pairs `(`/`)`, `[`/`]`, `{`/`}`.
/// Literals and comments are skipped even if their text is a delimiter.
/// Returns `None` if `open` is not an opening delimiter or it never closes;
/// a mismatched closer (e.g. `(]`) also gives `None`.
pub fn matching_close(tokens: &[TokenDecl], open: usize) -> Option<usize> {
    let first = tokens.get(open)?;
    if closer_for(&first.text).is_none() || is_trivia(first) {
        return None;
    }
    let mut stack: Vec<&'static str> = Vec::new();
    for (i, t) in tokens.iter().enumerate().skip(open) {
        if is_trivia(t) {
            continue;
        }
        if let Some(c) = closer_for(&t.text) {
            stack.push(c);
        } else if matches!(t.text.as_str(), ")" | "]" | "}") {
            if stack.pop()? != t.text {
                return None;
            }
            if stack.is_empty() {
                return Some(i);
            }
        }
    }
    None
}

fn closer_for(text: &str) -> Option<&'static str> {
    match text {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        _ => None,
    }
}

fn is_trivia(t: &TokenDecl) -> bool {
    matches!(t.class, TokenClass::Comment | TokenClass::Literal)
}

/// A forward cursor over tokens that can skip comments. The `*_code`
/// methods and `eat` skip only Comment tokens; string literals are code here
/// (unlike in [`matching_close`], which ignores delimiters inside literals).
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    tokens: &'a [TokenDecl],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(tokens: &'a [TokenDecl]) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Index of the next token `peek` would return.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.tokens.len());
    }

    pub fn tokens(&self) -> &'a [TokenDecl] {
        self.tokens
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The next token, comments included.
    pub fn peek(&self) -> Option<&'a TokenDecl> {
        self.tokens.get(self.pos)
    }

    /// The `n`th upcoming non-comment token (0 = next), without moving.
    pub fn peek_code(&self, n: usize) -> Option<(usize, &'a TokenDecl)> {
        self.tokens
            .iter()
            .enumerate()
            .skip(self.pos)
            .filter(|(_, t)| t.class != TokenClass::Comment)
            .nth(n)
    }

    /// Advance past comments.
    pub fn skip_comments(&mut self) {
        while self.peek().is_some_and(|t| t.class == TokenClass::Comment) {
            self.pos += 1;
        }
    }

    /// Return the next non-comment token and its index, advancing past it.
    pub fn next_code(&mut self) -> Option<(usize, &'a TokenDecl)> {
        self.skip_comments();
        let i = self.pos;
        let t = self.tokens.get(i)?;
        self.pos += 1;
        Some((i, t))
    }

    /// If the next non-comment token's text is `text`, consume it.
    pub fn eat(&mut self, text: &str) -> Option<usize> {
        let (i, t) = self.peek_code(0)?;
        (t.text == text).then(|| {
            self.pos = i + 1;
            i
        })
    }

    /// If the next non-comment token opens a delimiter, jump past its match
    /// and return `(open, close)` indices. On an unmatched delimiter, the
    /// cursor does not move.
    pub fn skip_balanced(&mut self) -> Option<(usize, usize)> {
        let (open, _) = self.peek_code(0)?;
        let close = matching_close(self.tokens, open)?;
        self.pos = close + 1;
        Some((open, close))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::tokenize;

    #[test]
    fn matching_close_balances_and_skips_literals() {
        let t = tokenize(r#"f(a, "(", [b { c }]) x"#);
        let open = t.iter().position(|t| t.text == "(").unwrap();
        let close = matching_close(&t, open).unwrap();
        assert_eq!(t[close].text, ")");
        assert_eq!(t[close + 1].text, "x");
    }

    #[test]
    fn matching_close_rejects_unbalanced() {
        let t = tokenize("( [ ) ]");
        assert_eq!(matching_close(&t, 0), None);
        let t = tokenize("{ {");
        assert_eq!(matching_close(&t, 0), None);
        let t = tokenize("x");
        assert_eq!(matching_close(&t, 0), None);
        assert_eq!(matching_close(&t, 9), None);
    }

    #[test]
    fn span_between_joins() {
        let t = tokenize("a\n  b");
        let s = span_between(&t[0].span, &t[1].span);
        assert_eq!((s.start, s.end), (0, 5));
        assert_eq!((s.start_line, s.end_line), (t[0].span.start_line, 2));
    }

    #[test]
    fn cursor_skips_comments_and_balanced_groups() {
        let t = tokenize("// c\nfn f(x) { y } z");
        let mut c = Cursor::new(&t);
        assert_eq!(c.next_code().unwrap().1.text, "fn");
        assert!(c.eat("g").is_none());
        assert!(c.eat("f").is_some());
        assert!(c.skip_balanced().is_some());
        assert!(c.skip_balanced().is_some());
        assert_eq!(c.next_code().unwrap().1.text, "z");
        assert!(c.next_code().is_none());
        assert!(c.at_end());
    }

    #[test]
    fn matching_close_ignores_delimiters_in_literals_and_comments() {
        // Starting on a literal/comment whose text is a delimiter.
        let lit = |s: &str| TokenDecl {
            text: s.into(),
            class: TokenClass::Literal,
            span: tokenize("(")[0].span,
        };
        assert_eq!(matching_close(&[lit("(")], 0), None);
        // A closer inside a literal or comment does not close.
        let t = tokenize("( \")\" /* ) */ )");
        assert_eq!(matching_close(&t, 0), Some(t.len() - 1));
    }

    #[test]
    fn cursor_peek_set_pos_and_unmatched() {
        let t = tokenize("a /* c */ b ( c");
        let mut c = Cursor::new(&t);
        assert_eq!(c.peek_code(1).unwrap().1.text, "b");
        assert_eq!(c.peek_code(2).unwrap().1.text, "(");
        assert!(c.peek_code(9).is_none());
        c.set_pos(99);
        assert_eq!(c.pos(), t.len());
        assert!(c.at_end() && c.peek().is_none());
        // An unmatched opener leaves the cursor where it was.
        let open = t.iter().position(|t| t.text == "(").unwrap();
        c.set_pos(open);
        assert!(c.skip_balanced().is_none());
        assert_eq!(c.pos(), open);
        assert_eq!(c.tokens().len(), t.len());
    }
}
