//! Generic fallback tokenizer: language-agnostic, never fails.
use crate::schema::{Span, TokenClass, TokenDecl};

/// Version of the tokenizer's output. Bump it whenever a change alters the
/// tokens `tokenize` returns for any input; it is folded into extractor
/// versions (and so file fingerprints) so stored files re-index. The golden
/// test `tokenizer_output_is_pinned` fails when output changes without a bump.
pub const TOKENIZER_VERSION: u32 = 1;

const OPERATOR_CHARS: &str = "+-*/%=<>!&|^~?@";

/// Tokenize everything except whitespace (a BOM counts as whitespace; offsets
/// stay relative to the original text). Callers must ensure `src.len() <= u32::MAX`
/// (spans are `u32`); `Store::index_bytes` enforces this.
/// Unterminated strings and comments
/// run to end of input (or line, for `//`).
pub fn tokenize(src: &str) -> Vec<TokenDecl> {
    tokenize_with(src, TokenizerOptions::default())
}

/// Dialect switches for `tokenize_with`. The default is the language-agnostic
/// fallback used for every language without an extractor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenizerOptions {
    /// Lex Rust literal forms as single Literal tokens: raw strings `r"..."`,
    /// `r#"..."#` (any number of hashes, no escapes), `br#"..."#`, and byte
    /// literals `b"..."`, `b'x'`. Off by default because other languages
    /// disagree (Python's `r"a\"b"` is one string via backslash escapes, shell
    /// `b"x"` is an identifier followed by a string). The Rust extractor turns
    /// it on; a Rust file with no extractor registered gets the plain fallback
    /// tokens (no symbols), deliberately, so its tokens may split raw strings.
    pub rust_literals: bool,
    /// `'...'` is always a string Literal (backslash escapes), as in
    /// JavaScript or HTML attribute values, instead of a char literal or a
    /// lone `'`. A single-quoted string never spans lines: an unterminated one
    /// stops at the end of its line, so an apostrophe in prose cannot swallow
    /// the rest of a file.
    pub single_quote_strings: bool,
    /// C# string prefixes: verbatim `@"..."` (no backslash escapes, `""` is an
    /// escaped quote), interpolated `$"..."`, and `$@"..."` / `@$"..."`, each
    /// one Literal. Interpolation holes are not lexed separately, so a `"`
    /// inside `{...}` ends the literal early (spans stay exact). Not handled:
    /// raw strings (`"""..."""`, `$$"""..."""`) and the `u8` suffix.
    pub csharp_strings: bool,
    /// Markup (HTML/XML-like), lexed with a little context:
    /// - `<!-- ... -->` is a Comment (`<!-->` and `<!--->` are empty ones).
    /// - Inside a tag (from a `<` directly followed by a letter, `/`, `!` or
    ///   `?`, to the next `>`): `"..."` and `'...'` are attribute-value
    ///   Literals (no escapes), and `-`/`:` join an identifier when a letter,
    ///   digit or `_` follows (`data-id`, `asp:Button`).
    /// - Outside tags (text, including `<script>`/`<style>` bodies): quotes
    ///   are single Punctuation tokens, so a stray `"` in prose cannot swallow
    ///   the file, and `//` and `/*` are not comments.
    pub markup: bool,
    /// ASP.NET server tags: `<%-- ... --%>` is a Comment; `<%@`, `<%=`,
    /// `<%#`, `<%:`, `<%$`, `<%` and `%>` are single Punctuation tokens (`%>`
    /// is one even outside a server tag). Meant to be combined with `markup`:
    /// between `<%` and `%>` the markup rules are off (C#-like code: `//`
    /// comments, normal strings, no name joining), and a quoted attribute
    /// value stops before a `<%` and resumes after the matching `%>`, so
    /// `Text='<%# Eval("x") %>'` exposes its server tag. A `//` comment in
    /// server code ends before `%>`; a string literal in server code does not
    /// (`<%= "a%>b" %>` keeps `"a%>b"` whole). A server tag inside an HTML
    /// `<!-- -->` comment stays part of the comment. Heuristic limit: in a
    /// `<script>` body, `a<b` opens a "tag" until the next `>`.
    pub aspx: bool,
    /// JavaScript regex literals: a `/` that cannot end an operand (at the
    /// start of input, or after `(` `,` `=` `:` `[` `!` `&` `|` `?` `{` `}`
    /// `;` `+` `-` `*` `%` `~` `^` or `return`/`typeof`/`case`/...; never
    /// after `<` or `>`, so JSX `</div>` is not a regex)
    /// starts one Literal running to the next unescaped `/` outside a `[...]`
    /// class, plus trailing flag letters. A regex never spans lines: without
    /// a closing `/` on its line the `/` stays an operator. This keeps quotes
    /// and braces inside regexes (`/'/g`, `/[{]/`) from derailing scanners.
    pub regex_literals: bool,
}

/// Lexing context for the `markup`/`aspx` dialects; unused otherwise.
#[derive(Default)]
struct MarkupState {
    /// Inside a start/end tag, between `<name` and `>`.
    tag: bool,
    /// Inside an ASP.NET server tag, between `<%` and `%>`.
    code: bool,
    /// An attribute value interrupted by a server tag, awaiting this quote.
    pending: Option<char>,
}

/// `tokenize` with an explicit dialect.
pub fn tokenize_with(src: &str, opts: TokenizerOptions) -> Vec<TokenDecl> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1u32, 1u32);
    let mut m = MarkupState::default();
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
        // Markup rules apply outside server-tag code.
        let markup = opts.markup && !m.code;
        let (len, class) = if let Some(n) = opts.aspx.then(|| aspx_len(rest)).flatten() {
            if n.1 == TokenClass::Punctuation {
                m.code = rest.starts_with("<%");
            }
            n
        } else if let Some(q) = m.pending.filter(|_| markup) {
            let (n, closed) = attr_value_len(rest, q, 0, opts.aspx);
            if closed {
                m.pending = None;
            }
            (n, TokenClass::Literal)
        } else if markup && rest.starts_with("<!--") {
            (
                rest[2..].find("-->").map_or(rest.len(), |p| p + 5),
                TokenClass::Comment,
            )
        } else if !markup && rest.starts_with("//") {
            let mut n = rest.find('\n').unwrap_or(rest.len());
            // ASP.NET ends a server block at `%>` even inside a `//` comment.
            if m.code {
                n = rest[..n].find("%>").unwrap_or(n);
            }
            (n, TokenClass::Comment)
        } else if let Some(after) = rest.strip_prefix("/*").filter(|_| !markup) {
            (
                after.find("*/").map_or(rest.len(), |p| p + 4),
                TokenClass::Comment,
            )
        } else if let Some(n) = opts
            .rust_literals
            .then(|| prefixed_literal_len(rest))
            .flatten()
        {
            (n, TokenClass::Literal)
        } else if let Some(n) = (opts.csharp_strings && !markup)
            .then(|| csharp_literal_len(rest))
            .flatten()
        {
            (n, TokenClass::Literal)
        } else if c.is_alphabetic() || c == '_' {
            (ident_len(rest, markup && m.tag), TokenClass::Identifier)
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
        } else if markup && matches!(c, '"' | '\'' | '`') {
            if m.tag && c != '`' {
                let (n, closed) = attr_value_len(rest, c, 1, opts.aspx);
                if !closed {
                    m.pending = Some(c);
                }
                (n, TokenClass::Literal)
            } else {
                (1, TokenClass::Punctuation)
            }
        } else if c == '\'' && opts.single_quote_strings {
            (single_quoted_len(rest), TokenClass::Literal)
        } else if c == '"' || c == '`' || (c == '\'' && is_char_literal(rest)) {
            (quoted_len(rest, c), TokenClass::Literal)
        } else if let Some(n) = (opts.regex_literals && c == '/' && regex_allowed(out.last()))
            .then(|| regex_len(rest))
            .flatten()
        {
            (n, TokenClass::Literal)
        } else if OPERATOR_CHARS.contains(c) {
            (c.len_utf8(), TokenClass::Operator)
        } else {
            (c.len_utf8(), TokenClass::Punctuation)
        };
        if markup && len == 1 {
            match c {
                '<' => {
                    m.tag = rest[1..].starts_with(|n: char| n.is_alphabetic() || "/!?".contains(n))
                }
                '>' => m.tag = false,
                _ => {}
            }
        }
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

/// Length of an identifier at the start of `rest`. In markup, `-` and `:`
/// continue it when followed by a letter, digit or `_` (`data-id`, `asp:Button`).
fn ident_len(rest: &str, markup: bool) -> usize {
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut it = rest.char_indices().peekable();
    while let Some((p, ch)) = it.next() {
        if word(ch) {
            continue;
        }
        let joins =
            markup && (ch == '-' || ch == ':') && it.peek().is_some_and(|&(_, next)| word(next));
        if !joins {
            return p;
        }
    }
    rest.len()
}

/// `'...'` with backslash escapes, ending at the closing quote or before the
/// end of the line (`\n` or `\r\n`).
fn single_quoted_len(rest: &str) -> usize {
    let mut line = rest.find('\n').unwrap_or(rest.len());
    if rest[..line].ends_with('\r') && line > 1 {
        line -= 1;
    }
    quoted_len(&rest[..line], '\'')
}

/// A markup attribute value from `rest[from..]` (1 skips the opening quote):
/// up to and including the closing `q` (returns `closed`), or, with `aspx`,
/// up to a `<%` (not closed, the value resumes after `%>`), or to end of
/// input. No escapes. Never returns 0 for `from == 1`; for `from == 0` the
/// caller guarantees `rest` does not start with `<%`.
fn attr_value_len(rest: &str, q: char, from: usize, aspx: bool) -> (usize, bool) {
    for (p, ch) in rest.char_indices().skip(from) {
        if ch == q {
            return (p + 1, true);
        }
        if aspx && rest[p..].starts_with("<%") {
            return (p, false);
        }
    }
    (rest.len(), false)
}

/// C# `@"..."`, `$"..."`, `$@"..."` and `@$"..."` at the start of `rest`.
fn csharp_literal_len(rest: &str) -> Option<usize> {
    let prefix = ["$@\"", "@$\"", "@\"", "$\""]
        .into_iter()
        .find(|p| rest.starts_with(p))?;
    let open = prefix.len() - 1;
    if !prefix.contains('@') {
        return Some(open + quoted_len(&rest[open..], '"'));
    }
    // Verbatim: no escapes except `""`.
    let b = rest.as_bytes();
    let mut p = open + 1;
    while p < b.len() {
        if b[p] == b'"' {
            if b.get(p + 1) == Some(&b'"') {
                p += 2;
                continue;
            }
            return Some(p + 1);
        }
        p += 1;
    }
    Some(rest.len())
}

/// Whether a `/` after `prev` (the last token) starts a regex rather than
/// dividing: true when `prev` cannot end an operand.
fn regex_allowed(prev: Option<&TokenDecl>) -> bool {
    let Some(p) = prev else {
        return true;
    };
    match p.class {
        TokenClass::Identifier => matches!(
            p.text.as_str(),
            "return"
                | "typeof"
                | "case"
                | "do"
                | "else"
                | "in"
                | "of"
                | "new"
                | "delete"
                | "void"
                | "throw"
                | "instanceof"
                | "yield"
                | "await"
        ),
        TokenClass::Literal => false,
        // Not after `<` or `>`: JSX closing tags (`</div>`) and self-closing
        // runs (`<br/>`) are not regexes.
        _ => !matches!(p.text.as_str(), ")" | "]" | "}" | "<" | ">"),
    }
}

/// Length of a regex literal `/.../flags` at the start of `rest`, if it closes
/// on the same line.
fn regex_len(rest: &str) -> Option<usize> {
    if rest.starts_with("//") || rest.starts_with("/*") {
        return None;
    }
    let (mut esc, mut class) = (false, false);
    for (p, ch) in rest.char_indices().skip(1) {
        match ch {
            '\n' | '\r' => return None,
            _ if esc => esc = false,
            '\\' => esc = true,
            '[' => class = true,
            ']' => class = false,
            '/' if !class => {
                let end = p + 1;
                let flags = rest[end..]
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(rest.len() - end);
                return Some(end + flags);
            }
            _ => {}
        }
    }
    None
}

/// ASP.NET server-tag delimiters and `<%-- --%>` comments.
fn aspx_len(rest: &str) -> Option<(usize, TokenClass)> {
    if let Some(after) = rest.strip_prefix("<%--") {
        let n = after.find("--%>").map_or(rest.len(), |p| p + 8);
        return Some((n, TokenClass::Comment));
    }
    if rest.starts_with("%>") {
        return Some((2, TokenClass::Punctuation));
    }
    let after = rest.strip_prefix("<%")?;
    let n = if after.starts_with(['@', '=', '#', ':', '$']) {
        3
    } else {
        2
    };
    Some((n, TokenClass::Punctuation))
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

    const GOLDEN: u64 = 0x4ee808b12b2ae44e;
    const RUST_GOLDEN: u64 = 0x9ec89e415b7c7bac;
    const GOLDEN_VERSION: u32 = 1;

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
        ];
        const RUST_SOURCES: &[&str] = &[
            "const A: &str = r#\"a\"b\"#; let b = br##\"x\"#y\"##; b\"by\\\"te\" b'x' r\"raw\\\"",
            "let r = br + bar; r#type unterminated r#\"never closed",
            "fn main() { let x = 1 + 2; // hi\n}\n",
        ];
        // FNV-1a 64 over the Debug rendering of every token.
        let fnv = |srcs: &[&str], opts: TokenizerOptions| {
            let mut h: u64 = 0xcbf29ce484222325;
            for src in srcs {
                for b in format!("{:?}", tokenize_with(src, opts)).bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
            }
            h
        };
        // The default (fallback) dialect is unchanged since version 1.
        let h = fnv(SOURCES, TokenizerOptions::default());
        let rust = TokenizerOptions {
            rust_literals: true,
            ..Default::default()
        };
        let rh = fnv(RUST_SOURCES, rust);
        assert_eq!(
            rh, RUST_GOLDEN,
            "rust-dialect output changed (hash {rh:#x}): bump TOKENIZER_VERSION and update RUST_GOLDEN"
        );
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

    /// One golden per non-default dialect over `DIALECT_SOURCES`.
    const DIALECT_GOLDENS: &[(&str, u64)] = &[
        ("single_quote_strings", 0x4e1b76e132514c29),
        ("csharp_strings", 0x40f802a4cf0ec6de),
        ("markup", 0x7e72042f258818fc),
        ("aspx", 0xced497dbf0743215),
        ("aspx_only", 0xbc7b0d641a1bf8a5),
        ("regex_literals", 0x8ffed24f2f0ed634),
    ];

    const DIALECT_SOURCES: &[&str] = &[
        "const s = 'it\\'s'; // c\nlet t = 'open\r\nx",
        "s.replace(/'/g, '').split(/[{\"]/); x = a / b / c; return /\\//i.test(q); y = /open\nz",
        r#"var a = @"C:\x ""q"""; var b = $"{n}\""; var c = $@"{d}"""; @$"z" @x"#,
        "<div data-id=\"a\" class='b'><!-- note --> http://x/*y*/ a: b- 27\" <!--> <!-- open",
        r#"<asp:Label Text='<%# Eval("x") %> of' /><% s = @"a""b"; // c
%><style>p{color:red}</style><% // t %><b id="z">"#,
        r#"<%@ Page Language="C#" %><%-- hidden --%><asp:Button id="b1" runat="server" /><%= x %><%# Eval("y") %><%: z %><%$ r %><% if (a) { %> <%-- open"#,
    ];

    fn dialect(name: &str) -> TokenizerOptions {
        let mut o = TokenizerOptions::default();
        match name {
            "single_quote_strings" => o.single_quote_strings = true,
            "csharp_strings" => o.csharp_strings = true,
            "markup" => o.markup = true,
            "aspx" => {
                o.markup = true;
                o.aspx = true;
                o.csharp_strings = true;
            }
            "aspx_only" => o.aspx = true,
            "regex_literals" => {
                o.regex_literals = true;
                o.single_quote_strings = true;
            }
            _ => unreachable!("{name}"),
        }
        o
    }

    fn dtoks(name: &str, s: &str) -> Vec<(String, TokenClass)> {
        tokenize_with(s, dialect(name))
            .into_iter()
            .map(|t| (t.text, t.class))
            .collect()
    }

    fn dtexts(name: &str, s: &str) -> Vec<String> {
        dtoks(name, s).into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn dialects_are_pinned() {
        let fnv = |opts: TokenizerOptions| {
            let mut h: u64 = 0xcbf29ce484222325;
            for src in DIALECT_SOURCES {
                for b in format!("{:?}", tokenize_with(src, opts)).bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
            }
            h
        };
        let changed: Vec<String> = DIALECT_GOLDENS
            .iter()
            .filter_map(|&(name, golden)| {
                let got = fnv(dialect(name));
                (got != golden).then(|| format!("(\"{name}\", {got:#x})"))
            })
            .collect();
        assert!(
            changed.is_empty(),
            "dialect output changed: bump TOKENIZER_VERSION and update DIALECT_GOLDENS to {}",
            changed.join(", ")
        );
    }

    #[test]
    fn single_quote_strings_dialect() {
        use TokenClass::*;
        let t = dtoks("single_quote_strings", r"x = 'it\'s' + 'a'");
        assert_eq!(t[2], (r"'it\'s'".to_string(), Literal));
        assert_eq!(t[4], ("'a'".to_string(), Literal));
        // Unterminated stops at the end of its line.
        assert_eq!(
            dtexts("single_quote_strings", "don't stop\nnext"),
            ["don", "'t stop", "next"]
        );
        // Default dialect is unchanged: a lone `'`.
        assert_eq!(texts("'ab'"), ["'", "ab", "'"]);
    }

    #[test]
    fn csharp_strings_dialect() {
        let one = |s: &str| {
            assert_eq!(
                dtoks("csharp_strings", s),
                [(s.to_string(), TokenClass::Literal)],
                "{s}"
            )
        };
        one(r#"@"C:\dir\""#);
        one(r#"@"say ""hi"" now""#);
        one(r#"$"a {b} \" c""#);
        one(r#"$@"{x}\ ""q""""#);
        one(r#"@$"{x}\""#);
        one(r#"@"unterminated"#);
        assert_eq!(
            dtexts("csharp_strings", "@class $ x"),
            ["@", "class", "$", "x"]
        );
        // Default dialect splits the prefix off.
        assert_eq!(texts(r#"@"a\" b""#), ["@", r#""a\" b""#]);
    }

    #[test]
    fn markup_dialect() {
        let t = dtexts("markup", "<a data-id-2=x asp:Button> http://h/*x*/ a- b:");
        assert_eq!(
            t,
            [
                "<",
                "a",
                "data-id-2",
                "=",
                "x",
                "asp:Button",
                ">",
                "http",
                ":",
                "/",
                "/",
                "h",
                "/",
                "*",
                "x",
                "*",
                "/",
                "a",
                "-",
                "b",
                ":"
            ]
        );
        // Names join only inside a tag.
        assert_eq!(
            dtexts("markup", "data-id <a data-id>")[..4],
            ["data", "-", "id", "<"]
        );
        assert_eq!(dtoks("markup", "<a data-id>")[2].1, TokenClass::Identifier);
        let t = dtoks("markup", "a <!-- x -- y --> b <!-- open");
        assert_eq!(t[1], ("<!-- x -- y -->".to_string(), TokenClass::Comment));
        assert_eq!(t[3], ("<!-- open".to_string(), TokenClass::Comment));
        assert_eq!(
            dtexts("markup", "<!--> a <!---> b <!----> c"),
            ["<!-->", "a", "<!--->", "b", "<!---->", "c"]
        );
    }

    #[test]
    fn markup_quotes_are_strings_only_in_tags() {
        use TokenClass::*;
        // A stray quote in text does not swallow the following tag.
        let t = dtoks("markup", "<p>27\" monitor, don't</p><form id=\"f\" v='x'>");
        assert!(t.contains(&("\"".to_string(), Punctuation)));
        assert!(t.contains(&("'".to_string(), Punctuation)));
        assert!(t.contains(&("\"f\"".to_string(), Literal)));
        assert!(t.contains(&("'x'".to_string(), Literal)));
        assert!(t.contains(&("form".to_string(), Identifier)));
        // Style and script bodies are text: no joining, quotes are punctuation.
        assert_eq!(
            dtexts("markup", "<style>a{color:red}</style>")[5..8],
            ["color", ":", "red"]
        );
        // `<` not followed by a name does not open a tag.
        assert_eq!(
            dtexts("markup", "a < b-c \"d\"")[2..],
            ["b", "-", "c", "\"", "d", "\""]
        );
    }

    #[test]
    fn aspx_code_and_attribute_values() {
        use TokenClass::*;
        // Server code uses code rules: no joining, `//` comments, strings.
        assert_eq!(
            dtexts("aspx", "<%= ok?a:b %><%= n-1 %>"),
            ["<%=", "ok", "?", "a", ":", "b", "%>", "<%=", "n", "-", "1", "%>"]
        );
        // A `//` comment ends at `%>`; markup rules resume after it.
        let t = dtexts("aspx", "<% // TODO %>\n<asp:Label id=\"a\">");
        assert_eq!(
            t,
            [
                "<%",
                "// TODO ",
                "%>",
                "<",
                "asp:Label",
                "id",
                "=",
                "\"a\"",
                ">"
            ]
        );
        let t = dtoks("aspx", "<% // c\n s = \"<p>\"; %>");
        assert_eq!(t[1], ("// c".to_string(), Comment));
        assert_eq!(t[4], ("\"<p>\"".to_string(), Literal));
        // A quoted attribute value pauses around a server tag.
        let t = dtoks(
            "aspx",
            "<asp:Label Text='<%# Eval(\"x\") %> of y' runat=\"server\" />",
        );
        let lit = |s: &str| (s.to_string(), Literal);
        assert_eq!(t[4], lit("'"));
        assert_eq!(t[5], ("<%#".to_string(), Punctuation));
        assert_eq!(t[8], lit("\"x\""));
        assert_eq!(t[10], ("%>".to_string(), Punctuation));
        assert_eq!(t[11], lit("of y'"));
        assert_eq!(t[12], ("runat".to_string(), Identifier));
        assert_eq!(t[14], lit("\"server\""));
        // A server tag inside an HTML comment stays in the comment.
        assert_eq!(dtoks("aspx", "<!-- <%= s %> -->")[0].1, Comment);
    }

    #[test]
    fn regex_literals_dialect() {
        let d = |s: &str| dtoks("regex_literals", s);
        let lit = |s: &str| (s.to_string(), TokenClass::Literal);
        // Quotes and braces inside a regex stay inside it.
        let t = d("s.replace(/'/g, ''); f()");
        assert_eq!(t[4], lit("/'/g"));
        assert_eq!(d("x = /[{/]\\//i;")[2], lit("/[{/]\\//i"));
        assert_eq!(d("/a/")[0], lit("/a/"));
        assert_eq!(d("return /a/.test(s)")[1], lit("/a/"));
        // Division after an operand.
        let texts = |s: &str| -> Vec<String> { d(s).into_iter().map(|t| t.0).collect() };
        assert_eq!(texts("a / b / c"), ["a", "/", "b", "/", "c"]);
        assert_eq!(
            texts("f(x) / 2 / y"),
            ["f", "(", "x", ")", "/", "2", "/", "y"]
        );
        // No closing `/` on the line: an operator.
        assert_eq!(texts("= /a\nb/"), ["=", "/", "a", "b", "/"]);
        // Comments still win.
        assert_eq!(d("x = // c")[2].1, TokenClass::Comment);
        // Off by default.
        assert_eq!(texts_default("= /a/"), ["=", "/", "a", "/"]);
    }

    fn texts_default(s: &str) -> Vec<String> {
        texts(s)
    }

    #[test]
    fn aspx_without_markup() {
        let o = TokenizerOptions {
            aspx: true,
            ..Default::default()
        };
        let t: Vec<String> = tokenize_with("<% // c\n x %> a-b 'q'", o)
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert_eq!(t, ["<%", "// c", "x", "%>", "a", "-", "b", "'q'"]);
    }

    #[test]
    fn single_quote_crlf() {
        assert_eq!(
            dtexts("single_quote_strings", "x\r\n'y\r\nz '\r\n"),
            ["x", "'y", "z", "'"]
        );
        // Default dialect: `-` splits names.
        assert_eq!(texts("data-id"), ["data", "-", "id"]);
    }

    #[test]
    fn aspx_dialect() {
        use TokenClass::*;
        let t = dtoks(
            "aspx",
            "<%@ Page %><%= a %><%# b %><%: c %><%$ d %><% e %><%-- x %> --%>",
        );
        let p = |s: &str| (s.to_string(), Punctuation);
        let id = |s: &str| (s.to_string(), Identifier);
        assert_eq!(
            t,
            [
                p("<%@"),
                id("Page"),
                p("%>"),
                p("<%="),
                id("a"),
                p("%>"),
                p("<%#"),
                id("b"),
                p("%>"),
                p("<%:"),
                id("c"),
                p("%>"),
                p("<%$"),
                id("d"),
                p("%>"),
                p("<%"),
                id("e"),
                p("%>"),
                ("<%-- x %> --%>".to_string(), Comment),
            ]
        );
        assert_eq!(dtoks("aspx", "<%-- open")[0].1, Comment);
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

    const RUST: TokenizerOptions = TokenizerOptions {
        rust_literals: true,
        single_quote_strings: false,
        csharp_strings: false,
        markup: false,
        aspx: false,
        regex_literals: false,
    };

    fn rtexts(s: &str) -> Vec<String> {
        tokenize_with(s, RUST).into_iter().map(|t| t.text).collect()
    }

    fn lit(s: &str) -> Vec<(String, TokenClass)> {
        tokenize_with(s, RUST)
            .into_iter()
            .map(|t| (t.text, t.class))
            .collect()
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
        assert_eq!(rtexts("r\"a\\\" x"), ["r\"a\\\"", "x"]);
        // Closing needs the same number of hashes; a longer run still closes.
        assert_eq!(rtexts("r##\"a\"# b\"## c"), ["r##\"a\"# b\"##", "c"]);
        // Unterminated raw strings run to end of input.
        assert_eq!(rtexts("x r#\"abc\" \"#"), ["x", "r#\"abc\" \"#"]);
        assert_eq!(rtexts("r\"abc"), ["r\"abc"]);
    }

    #[test]
    fn prefixes_need_the_quote_immediately() {
        use TokenClass::Identifier;
        for id in ["r", "br", "bar", "b", "rb", "raw", "r_", "brr"] {
            assert_eq!(lit(id), [(id.to_string(), Identifier)], "{id}");
        }
        assert_eq!(rtexts("r #\"a\"#")[0], "r");
        assert_eq!(rtexts("r#type"), ["r", "#", "type"]);
        assert_eq!(rtexts("b 'x'"), ["b", "'x'"]);
        assert_eq!(rtexts("br#x"), ["br", "#", "x"]);
        // Identifier ending in r/b does not start a raw string.
        assert_eq!(rtexts("bar\"x\""), ["bar", "\"x\""]);
        assert_eq!(rtexts("b'a"), ["b", "'", "a"]);
    }

    #[test]
    fn default_dialect_ignores_rust_literals() {
        // Same tokens as version 1 / main: Python raw strings, shell `b"y"`.
        assert_eq!(texts("r\"a\\\"b\""), ["r", "\"a\\\"b\""]);
        assert_eq!(texts("r\"\\\"\""), ["r", "\"\\\"\""]);
        assert_eq!(texts("echo b\"y\""), ["echo", "b", "\"y\""]);
        assert_eq!(texts("r#\"a\"b\"#"), ["r", "#", "\"a\"", "b", "\"#"]);
        assert_eq!(texts("b'x'"), ["b", "'x'"]);
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
        fn spans_match_source_dialect_heavy(
            src in "([ \\n]|<|%|>|-|:|@|\\$|!|\"|'|x|\\\\|é|/|\\*|#|=|\\{|\\}){0,50}"
        ) {
            check_spans(&src)?;
        }

        #[test]
        fn spans_match_source(src in "\\PC{0,200}") {
            check_spans(&src)?;
        }
    }

    fn check_spans(src: &str) -> Result<(), TestCaseError> {
        check_spans_with(src, TokenizerOptions::default())?;
        check_spans_with(src, RUST)?;
        for (name, _) in DIALECT_GOLDENS {
            check_spans_with(src, dialect(name))?;
        }
        // Every flag at once.
        check_spans_with(
            src,
            TokenizerOptions {
                rust_literals: true,
                single_quote_strings: true,
                csharp_strings: true,
                markup: true,
                aspx: true,
                regex_literals: true,
            },
        )
    }

    fn check_spans_with(src: &str, opts: TokenizerOptions) -> Result<(), TestCaseError> {
        let src = src.to_string();
        let toks = tokenize_with(&src, opts);
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
