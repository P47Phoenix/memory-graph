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

/// [`matching_close`] for every token at once: `close_table(tokens)[i] ==
/// matching_close(tokens, i)` for every index `i`, computed in one linear
/// pass instead of one scan per opener. Use it (or [`code_close_table`])
/// whenever an extractor looks up closers in a loop: calling
/// [`matching_close`] per opener is quadratic on long unbalanced runs such
/// as 100k `(`.
///
/// Semantics match [`matching_close`] exactly: comments and literals are
/// skipped, and a mismatched closer (e.g. the `]` in `(]`) leaves every
/// delimiter still open at that point unclosed.
pub fn close_table(tokens: &[TokenDecl]) -> Vec<Option<usize>> {
    let mut out = vec![None; tokens.len()];
    let mut stack: Vec<(usize, &'static str)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if is_trivia(t) {
            continue;
        }
        if let Some(c) = closer_for(&t.text) {
            stack.push((i, c));
        } else if matches!(t.text.as_str(), ")" | "]" | "}") {
            match stack.pop() {
                Some((open, want)) if want == t.text => out[open] = Some(i),
                _ => stack.clear(),
            }
        }
    }
    out
}

/// [`close_table`] re-indexed by code position, for extractors that walk a
/// `code` index (sorted indices into `tokens`, e.g. from [`code_index`]):
/// entry `c` is the code position of the token closing `tokens[code[c]]`,
/// or `None` if it does not close or its closer is not in `code`. This is
/// exactly `matching_close(tokens, code[c])` followed by
/// `code.binary_search(&close).ok()`, in linear time.
pub fn code_close_table(tokens: &[TokenDecl], code: &[usize]) -> Vec<Option<usize>> {
    let table = close_table(tokens);
    let mut pos = vec![None; tokens.len()];
    for (c, &i) in code.iter().enumerate() {
        pos[i] = Some(c);
    }
    code.iter()
        .map(|&i| table[i].and_then(|close| pos[close]))
        .collect()
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

/// Relabels every `Identifier` token whose text is one of `keywords` as
/// `Keyword`, unless `is_escaped(tokens, i)` says that occurrence is used as
/// a plain name (an escaped identifier, a property name, ...). The list and
/// the escape rule are the extractor's; this only applies them. Comparison
/// is exact (case-sensitive). Spans and texts are untouched.
pub fn mark_keywords(
    tokens: &mut [TokenDecl],
    keywords: &[&str],
    is_escaped: impl Fn(&[TokenDecl], usize) -> bool,
) {
    for i in 0..tokens.len() {
        if tokens[i].class == TokenClass::Identifier
            && keywords.contains(&tokens[i].text.as_str())
            && !is_escaped(tokens, i)
        {
            tokens[i].class = TokenClass::Keyword;
        }
    }
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
///
/// Each call scans forward from `open`, so it is O(n) per call: an
/// extractor that looks up many blocks should build a
/// [`keyword_close_table`] once instead.
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

/// [`keyword_block`] for every token at once: `keyword_close_table(tokens,
/// pairs, ignore_case)[i] == keyword_block(tokens, i, pairs, ignore_case)`
/// for every index `i`, in one linear pass (times the number of `pairs`).
/// Use it whenever an extractor looks up keyword blocks in a loop: calling
/// [`keyword_block`] per opener is quadratic on long unclosed runs such as
/// 100k `do`.
pub fn keyword_close_table(
    tokens: &[TokenDecl],
    pairs: &[(&str, &str)],
    ignore_case: bool,
) -> Vec<Option<usize>> {
    let eq = |a: &str, b: &str| {
        if ignore_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let mut out = vec![None; tokens.len()];
    // (opener index, index into `pairs` of its closer).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if is_trivia(t) {
            continue;
        }
        if let Some(k) = pairs.iter().position(|(o, _)| eq(&t.text, o)) {
            stack.push((i, k));
        } else if pairs.iter().any(|(_, c)| eq(&t.text, c)) {
            // A mismatched (or unopened) closer ends every block still open:
            // `keyword_block` from any of them returns `None` here.
            match stack.pop() {
                Some((open, k)) if eq(&t.text, pairs[k].1) => out[open] = Some(i),
                _ => stack.clear(),
            }
        }
    }
    out
}

/// What a [`NestedEnds`] scan does at one position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Opens a nested list (e.g. `<`).
    Open,
    /// Closes the innermost open list (e.g. `>`).
    Close,
    /// Ends the scan: every list still open fails (e.g. `;`).
    Stop,
    /// A group to jump over: `Some(close)` is the group's closer and the
    /// scan resumes at `close + 1`; `None` fails the scan (e.g. an
    /// unmatched `(`).
    Skip(Option<usize>),
    /// Anything else.
    Other,
}

/// Ends of nested lists that no bracket table covers, such as generic
/// `<...>` lists, found by a forward scan whose next move depends only on
/// the position (a [`Step`]) and the range end `hi`. The scan from any
/// opener it passes would follow the same path, so one scan answers every
/// opener on it and later lookups reuse the answers: looking up every
/// opener of a long unclosed run (100k `<`) is linear, where a plain
/// forward scan per lookup is quadratic.
///
/// Answers are cached by `(open, hi)` only, so use one instance per step
/// function (and token sequence): sharing an instance between different
/// rules returns stale answers.
#[derive(Debug, Default)]
pub struct NestedEnds {
    memo: std::cell::RefCell<std::collections::HashMap<(usize, usize), Option<usize>>>,
}

impl NestedEnds {
    /// An empty cache, for one step function over one token sequence.
    pub fn new() -> Self {
        Self::default()
    }

    /// The position closing the list opened at `open` (whose step must be
    /// [`Step::Open`]), scanning `[open, hi)`; `None` if a [`Step::Stop`],
    /// a failed [`Step::Skip`] or `hi` comes first. `step` must give the
    /// same answer for a position every time it is called with this `hi`.
    /// If `open` is not an opener the answer is meaningless (it may be the
    /// end of a later list, and is cached as such): callers check first.
    pub fn find(&self, open: usize, hi: usize, step: impl Fn(usize) -> Step) -> Option<usize> {
        if let Some(&r) = self.memo.borrow().get(&(open, hi)) {
            return r;
        }
        // Openers on the path not yet closed, and the answers found.
        let mut stack: Vec<usize> = Vec::new();
        let mut found = Vec::new();
        let mut c = open;
        let result = loop {
            if c >= hi {
                break None;
            }
            match step(c) {
                Step::Open => {
                    // An opener answered before: skip its list, or fail
                    // with it (the path from here on is the same).
                    let known = if c == open {
                        None
                    } else {
                        self.memo.borrow().get(&(c, hi)).copied()
                    };
                    match known {
                        Some(Some(close)) => {
                            c = close + 1;
                            continue;
                        }
                        Some(None) => break None,
                        None => stack.push(c),
                    }
                }
                Step::Close => match stack.pop() {
                    Some(o) => {
                        found.push((o, Some(c)));
                        if stack.is_empty() {
                            break Some(c);
                        }
                    }
                    // `open` was not an opener.
                    None => break None,
                },
                Step::Stop | Step::Skip(None) => break None,
                Step::Skip(Some(to)) => c = to,
                Step::Other => {}
            }
            c += 1;
        };
        let mut memo = self.memo.borrow_mut();
        for (o, r) in found {
            memo.insert((o, hi), r);
        }
        for o in stack {
            memo.insert((o, hi), None);
        }
        result
    }
}

/// Index of the last token of the indented block headed by `tokens[header]`
/// (layout languages: Python, GDScript, Haskell, F#). The header's indent is
/// the start column of the first token that starts on the header's line
/// (so a multi-line token ending there does not count); the block runs
/// through every later token until the first line whose first non-`skip`
/// token starts at or left of that column. Line starts inside an open
/// `(`, `[` or `{` (counted after the header, ignoring literals and
/// comments) never end the block, so a header split over lines
/// (`def f(\n  a,\n):`) and multi-line calls in the body stay inside. Tokens
/// in `skip` (usually comments) never end a block, but trailing ones are not
/// included. A one-line block (`def f(): pass`) ends on its header line.
/// Columns count characters, so a tab is one column: mixed tab/space
/// indentation is compared as written.
pub fn indent_block(tokens: &[TokenDecl], header: usize, skip: &[TokenClass]) -> usize {
    let Some(h) = tokens.get(header) else {
        return header;
    };
    let line = h.span.start_line;
    let mut line_first = header;
    while line_first > 0 && tokens[line_first - 1].span.start_line == line {
        line_first -= 1;
    }
    let indent = tokens[line_first].span.start_col;
    let mut last = header;
    let mut prev_end_line = h.span.end_line;
    let mut depth = 0usize;
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
        if !line_open && depth == 0 && t.span.start_col <= indent {
            break;
        }
        if !is_trivia(t) {
            match t.text.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
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
    ///
    /// This scans forward with [`matching_close`], O(n) per call, so it is
    /// quadratic if called once per opener over a long unbalanced run; an
    /// extractor calling it in a loop should build a [`close_table`] once
    /// and use [`Cursor::skip_balanced_with`].
    pub fn skip_balanced(&mut self) -> Option<(usize, usize)> {
        let (open, _) = self.peek_code(0)?;
        let close = matching_close(self.tokens, open)?;
        self.pos = close + 1;
        Some((open, close))
    }

    /// [`Cursor::skip_balanced`] with closers looked up in `table`, which
    /// must be [`close_table`] of this cursor's tokens: O(1) per call apart
    /// from skipping comments.
    pub fn skip_balanced_with(&mut self, table: &[Option<usize>]) -> Option<(usize, usize)> {
        let (open, _) = self.peek_code(0)?;
        let close = (*table.get(open)?)?;
        self.pos = close + 1;
        Some((open, close))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::tokenize;

    #[test]
    fn mark_keywords_relabels_listed_identifiers_unless_escaped() {
        let mut t = tokenize(r#"if x . if "if" If"#);
        mark_keywords(&mut t, &["if"], |t, i| i > 0 && t[i - 1].text == ".");
        let classes: Vec<_> = t.iter().map(|t| t.class).collect();
        assert_eq!(classes[0], TokenClass::Keyword);
        assert_eq!(classes[1], TokenClass::Identifier);
        assert_eq!(classes[3], TokenClass::Identifier, "escaped");
        assert_eq!(classes[4], TokenClass::Literal);
        assert_eq!(classes[5], TokenClass::Identifier, "case-sensitive");
    }

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
        // Brackets: a split header and a multi-line call stay in the block.
        let t = py("x = 0\ndef f(\n a,\n):\n body(1,\n2)\n tail\nnext");
        let def = t.iter().position(|t| t.text == "def").unwrap();
        assert_eq!(t[indent_block(&t, def, &skip)].text, "tail");
        // A header after other tokens on its line (line_first > 0), where
        // the previous line ends in a multi-line token.
        let t = py("s = '''a\nb''' ; if c:\n  d\ne");
        let hdr = t.iter().position(|t| t.text == "if").unwrap();
        // Indent is `;`'s column 6 (the first token starting on that line,
        // not the literal's column 5), so `d` at column 3 is outside.
        assert_eq!(t[indent_block(&t, hdr, &skip)].text, ":");
        let t = py("s = '''a\nb''' ; if c:\n       d\ne");
        assert_eq!(t[indent_block(&t, hdr, &skip)].text, "d");
        let t = py("a\n  if c:\n    d\n  e");
        let hdr = t.iter().position(|t| t.text == "if").unwrap();
        assert_eq!(t[indent_block(&t, hdr, &skip)].text, "d");
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

#[cfg(test)]
mod close_table_tests {
    use super::*;
    use crate::tokenizer::tokenize;
    use proptest::prelude::*;

    proptest! {
        /// `close_table` equals `matching_close` at every index, and
        /// `code_close_table` equals the per-opener lookup-then-binary-search
        /// every extractor used, for any subset of tokens as `code`.
        #[test]
        fn close_tables_match_matching_close(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("("), Just(")"), Just("["), Just("]"), Just("{"), Just("}"),
                    Just("a"), Just("\"(\""), Just("'}'"), Just("/*{*/"), Just("//)\n"),
                    Just("\n#if (\n"), Just("'('"), Just("-- )\n"), Just("# {\n"),
                    Just("$$ ( $$"), Just("r#\"[\"#"),
                ],
                0..48,
            ),
            dialect in 0usize..6,
            keep in proptest::collection::vec(any::<bool>(), 0..200),
        ) {
            let src = parts.join(" ");
            use crate::tokenizer::{tokenize_with, TokenizerOptions as O};
            // Dialects change what is a comment or literal, so the trivia
            // skip is exercised on more than the default tokenizer.
            let opts = [O::DEFAULT, O::SQL, O::SHELL, O::CSHARP, O::RUST, O::PYTHON][dialect];
            let tokens = tokenize_with(&src, opts);
            let table = close_table(&tokens);
            prop_assert_eq!(table.len(), tokens.len());
            for (i, got) in table.iter().enumerate() {
                prop_assert_eq!(*got, matching_close(&tokens, i), "{} at {}", src, i);
            }
            let code: Vec<usize> = (0..tokens.len())
                .filter(|&i| keep.get(i).copied().unwrap_or(true))
                .collect();
            let by_code = code_close_table(&tokens, &code);
            for (c, &i) in code.iter().enumerate() {
                let expected = matching_close(&tokens, i)
                    .and_then(|close| code.binary_search(&close).ok());
                prop_assert_eq!(by_code[c], expected, "{} at {}", src, c);
            }
        }
    }

    proptest! {
        /// `keyword_close_table` equals `keyword_block` at every index, with
        /// shared closers, nesting, mismatches, literals and comments.
        #[test]
        fn keyword_close_table_matches_keyword_block(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("if"), Just("fi"), Just("do"), Just("done"), Just("end"),
                    Just("fn"), Just("IF"), Just("End"), Just("x"), Just("'if'"),
                    Just("# fi\n"), Just("\"done\""),
                ],
                0..48,
            ),
            ignore_case in any::<bool>(),
            which in 0usize..3,
        ) {
            use crate::tokenizer::{tokenize_with, TokenizerOptions};
            let src = parts.join(" ");
            let tokens = tokenize_with(&src, TokenizerOptions::SHELL);
            let pair_sets: [&[(&str, &str)]; 3] = [
                &[("if", "fi"), ("do", "done")],
                &[("do", "end"), ("fn", "end")],
                &[("if", "end"), ("do", "end"), ("do", "done")],
            ];
            let pairs = pair_sets[which];
            let table = keyword_close_table(&tokens, pairs, ignore_case);
            prop_assert_eq!(table.len(), tokens.len());
            for (i, got) in table.iter().enumerate() {
                prop_assert_eq!(*got, keyword_block(&tokens, i, pairs, ignore_case), "{} at {}", src, i);
            }
            // `skip_balanced_with` agrees with `skip_balanced`.
            let ct = close_table(&tokens);
            for i in 0..tokens.len() {
                let mut a = Cursor::new(&tokens);
                a.set_pos(i);
                let mut b = a.clone();
                prop_assert_eq!(a.skip_balanced(), b.skip_balanced_with(&ct));
                prop_assert_eq!(a.pos(), b.pos());
            }
        }
    }

    /// Plain forward scan: the reference for [`NestedEnds::find`].
    fn nested_reference(s: &[u8], skip: &[Option<usize>], open: usize, hi: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut c = open;
        while c < hi {
            match s[c] {
                b'<' => depth += 1,
                b'>' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(c);
                    }
                }
                b'(' => c = skip[c].filter(|&x| x < hi)?,
                b';' => return None,
                _ => {}
            }
            c += 1;
        }
        None
    }

    proptest! {
        /// `NestedEnds::find` equals a plain forward scan for every opener,
        /// with one memo reused across all calls and several `hi` values,
        /// queried in random order.
        #[test]
        fn nested_ends_match_forward_scan(
            s in proptest::collection::vec(
                prop_oneof![Just(b'<'), Just(b'>'), Just(b'('), Just(b')'), Just(b';'), Just(b'x')],
                0..60,
            ),
            order in proptest::collection::vec(any::<prop::sample::Index>(), 0..120),
            his in proptest::collection::vec(any::<prop::sample::Index>(), 1..4),
        ) {
            // `(` skips to its `)` (the next one), like a close table.
            let skip: Vec<Option<usize>> = (0..s.len())
                .map(|i| (s[i] == b'(').then(|| (i + 1..s.len()).find(|&j| s[j] == b')')).flatten())
                .collect();
            let step = |c: usize| match s[c] {
                b'<' => Step::Open,
                b'>' => Step::Close,
                b'(' => Step::Skip(skip[c]),
                b';' => Step::Stop,
                _ => Step::Other,
            };
            let his: Vec<usize> = his.iter().map(|h| h.index(s.len() + 1)).collect();
            // One memo for every `hi`: answers are keyed by `(open, hi)`.
            let memo = NestedEnds::new();
            let opens: Vec<usize> = (0..s.len()).filter(|&i| s[i] == b'<').collect();
            if !opens.is_empty() {
                for q in order.iter().chain(order.iter()) {
                    let open = opens[q.index(opens.len())];
                    for &hi in &his {
                        if open >= hi {
                            continue;
                        }
                        let step_hi = |c: usize| match step(c) {
                            Step::Skip(t) => Step::Skip(t.filter(|&x| x < hi)),
                            other => other,
                        };
                        prop_assert_eq!(
                            memo.find(open, hi, step_hi),
                            nested_reference(&s, &skip, open, hi),
                            "{:?} open {} hi {}", String::from_utf8_lossy(&s), open, hi
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn keyword_close_table_is_linear_on_unclosed_runs() {
        let tokens = tokenize(&"do ".repeat(100_000));
        let start = std::time::Instant::now();
        let t = keyword_close_table(&tokens, &[("do", "end")], false);
        assert!(t.iter().all(Option::is_none));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn close_table_is_linear_on_unbalanced_runs() {
        let tokens = tokenize(&"(".repeat(100_000));
        let start = std::time::Instant::now();
        assert!(close_table(&tokens).iter().all(Option::is_none));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
