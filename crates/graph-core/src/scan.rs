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

/// Indices of the tokens whose class is not in `skip`, in order. With
/// `skip = &[TokenClass::Comment]` this is the code tokens an extractor walks.
pub fn code_index(tokens: &[TokenDecl], skip: &[TokenClass]) -> Vec<usize> {
    tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| !skip.contains(&t.class))
        .map(|(i, _)| i)
        .collect()
}

/// Index of the keyword closing the block opened at `open`, for keyword
/// blocks such as `do`/`end`, `if`/`fi`, `case`/`esac`, `do`/`done`,
/// `BEGIN`/`END`, `PROC`/`ENDP`, `MACRO`/`ENDM` or `dcl-proc`/`end-proc`.
/// `pairs` lists `(opener, closer)`; several openers may share a closer
/// (`("do", "end"), ("fn", "end")`), and the first pair listing an opener
/// wins. Blocks nest: every opener pushes its closer, and a closer must match
/// the innermost open block, otherwise (or if the block never closes, or
/// `tokens[open]` is not an opener) the result is `None`. Comments and
/// literals are ignored; `ignore_case` compares ASCII case-insensitively.
/// Openers used as plain words (Ruby-style `x if y`) are the caller's
/// business: filter them out of `tokens` first or pass a pre-checked slice.
pub fn keyword_block(
    tokens: &[TokenDecl],
    open: usize,
    pairs: &[(&str, &str)],
    ignore_case: bool,
) -> Option<usize> {
    let eq = |a: &str, b: &str| {
        if ignore_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let closer_of = |t: &TokenDecl| pairs.iter().find(|(o, _)| eq(&t.text, o)).map(|&(_, c)| c);
    let first = tokens.get(open)?;
    if is_trivia(first) || closer_of(first).is_none() {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for (i, t) in tokens.iter().enumerate().skip(open) {
        if is_trivia(t) {
            continue;
        }
        if let Some(c) = closer_of(t) {
            stack.push(c);
        } else if pairs.iter().any(|(_, c)| eq(&t.text, c)) {
            if !eq(&t.text, stack.pop()?) {
                return None;
            }
            if stack.is_empty() {
                return Some(i);
            }
        }
    }
    None
}

/// Index of the last token of the indented block headed by `tokens[header]`
/// (layout languages: Python, GDScript, Haskell, F#). The header's indent is
/// the start column of the first token on its line; the block runs through
/// every later token until the first line whose first non-`skip` token
/// starts at or left of that column. Tokens in `skip` (usually comments) never
/// end a block, but trailing ones are not included. A one-line block (`def
/// f(): pass`) ends on its header line. Columns count characters, so a tab
/// is one column: mixed tab/space indentation is compared as written.
pub fn indent_block(tokens: &[TokenDecl], header: usize, skip: &[TokenClass]) -> usize {
    let Some(h) = tokens.get(header) else {
        return header;
    };
    let line_first = tokens[..header]
        .iter()
        .rposition(|t| t.span.end_line < h.span.start_line)
        .map_or(0, |p| p + 1);
    let indent = tokens[line_first].span.start_col;
    let mut last = header;
    let mut prev_end_line = h.span.end_line;
    // Whether a non-skip token was already seen on the current line (the
    // header's line counts as seen).
    let mut line_open = true;
    for (j, t) in tokens.iter().enumerate().skip(header + 1) {
        if t.span.start_line > prev_end_line {
            line_open = false;
        }
        prev_end_line = prev_end_line.max(t.span.end_line);
        if skip.contains(&t.class) {
            continue;
        }
        if !line_open && t.span.start_col <= indent {
            break;
        }
        line_open = true;
        last = j;
    }
    last
}

/// Groups token indices by start line: yields `(line, range)` for each run
/// of consecutive tokens starting on the same line. A multi-line token
/// belongs to the line it starts on.
pub fn line_iter(tokens: &[TokenDecl]) -> impl Iterator<Item = (u32, std::ops::Range<usize>)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let first = tokens.get(i)?;
        let line = first.span.start_line;
        let start = i;
        while tokens.get(i).is_some_and(|t| t.span.start_line == line) {
            i += 1;
        }
        Some((line, start..i))
    })
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

    fn tok_with(s: &str, o: crate::tokenizer::TokenizerOptions) -> Vec<TokenDecl> {
        crate::tokenizer::tokenize_with(s, o)
    }

    #[test]
    fn code_index_skips_classes() {
        let t = tokenize("a /* c */ \"s\" b");
        assert_eq!(code_index(&t, &[TokenClass::Comment]), [0, 2, 3]);
        assert_eq!(
            code_index(&t, &[TokenClass::Comment, TokenClass::Literal]),
            [0, 3]
        );
        assert_eq!(code_index(&t, &[]), [0, 1, 2, 3]);
    }

    #[test]
    fn keyword_block_nests_and_matches() {
        use crate::tokenizer::TokenizerOptions;
        let sh = |s: &str| tok_with(s, TokenizerOptions::SHELL);
        let pairs = [("if", "fi"), ("case", "esac"), ("do", "done")];
        let t = sh("if a; then case x in y) if b; then c; fi;; esac; fi # fi\nz");
        let close = keyword_block(&t, 0, &pairs, false).unwrap();
        assert_eq!(t[close].text, "fi");
        assert_eq!(t[close + 1].class, TokenClass::Comment);
        // Inner block.
        let inner = t.iter().position(|t| t.text == "case").unwrap();
        assert_eq!(
            t[keyword_block(&t, inner, &pairs, false).unwrap()].text,
            "esac"
        );
        // Shared closer, case-insensitive.
        let t = tokenize("Foo PROC x MACRO y ENDM z endp w");
        let p = [("proc", "endp"), ("macro", "endm")];
        assert_eq!(keyword_block(&t, 1, &p, true), Some(7));
        assert_eq!(keyword_block(&t, 1, &p, false), None);
        let t = tokenize("fn do x end end");
        let p = [("do", "end"), ("fn", "end")];
        assert_eq!(keyword_block(&t, 0, &p, false), Some(4));
        // Hyphenated RPG keywords.
        let t = tok_with(
            "**FREE\nDCL-PROC a; dcl-pi *n; end-pi; end-proc;",
            TokenizerOptions::RPG,
        );
        let p = [("dcl-proc", "end-proc"), ("dcl-pi", "end-pi")];
        let open = t.iter().position(|t| t.text == "DCL-PROC").unwrap();
        assert_eq!(
            t[keyword_block(&t, open, &p, true).unwrap()].text,
            "end-proc"
        );
        // Mismatch, unclosed, not an opener, literal opener.
        let p = [("if", "fi"), ("do", "done")];
        assert_eq!(keyword_block(&sh("if do fi done"), 0, &p, false), None);
        assert_eq!(keyword_block(&sh("if x"), 0, &p, false), None);
        assert_eq!(keyword_block(&sh("x fi"), 0, &p, false), None);
        assert_eq!(keyword_block(&sh("if x"), 9, &p, false), None);
        assert_eq!(keyword_block(&sh("'if' fi"), 0, &p, false), None);
    }

    #[test]
    fn indent_block_follows_layout() {
        use crate::tokenizer::TokenizerOptions;
        let py = |s: &str| tok_with(s, TokenizerOptions::PYTHON);
        let skip = [TokenClass::Comment];
        let src = "class A:\n    def f(self):\n        x = 1\n# low comment\n        y = 2\n    def g(): pass\n# trailing\nz = 3\n";
        let t = py(src);
        let text_at = |i: usize| t[i].text.as_str();
        let class_end = indent_block(&t, 0, &skip);
        assert_eq!(text_at(class_end), "pass");
        let f = t.iter().position(|t| t.text == "def").unwrap();
        let f_end = indent_block(&t, f, &skip);
        assert_eq!(text_at(f_end), "2");
        let g = t.iter().rposition(|t| t.text == "def").unwrap();
        assert_eq!(text_at(indent_block(&t, g, &skip)), "pass");
        // Header in the middle of a line uses the line's indent.
        let t = py("if a: b = 1\n  c\nd");
        assert_eq!(t[indent_block(&t, 3, &skip)].text, "c");
        // Out of range.
        assert_eq!(indent_block(&t, 99, &skip), 99);
    }

    #[test]
    fn line_iter_groups_by_start_line() {
        let t = tokenize("a b\n\"x\ny\" c\n\nd");
        let lines: Vec<_> = line_iter(&t).collect();
        assert_eq!(lines, [(1, 0..2), (2, 2..3), (3, 3..4), (5, 4..5)]);
        assert_eq!(line_iter(&[]).count(), 0);
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
