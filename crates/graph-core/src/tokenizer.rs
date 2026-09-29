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

/// Fixed-column source layouts for [`TokenizerOptions::fixed_columns`].
/// Columns count characters (a tab is one column, a BOM none), 1-based.
/// Every area still yields exact-span tokens; only how it is lexed changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedLayout {
    /// COBOL reference format: columns 1-6 (sequence area) are one Comment
    /// token (trimmed) when not blank; a `*` or `/` in column 7 makes the rest
    /// of the line one Comment; any other non-blank column-7 indicator (`-`
    /// continuation, `D` debug) is its own token; columns 8-72 are code, where
    /// `*>` starts a comment to the end of the area; columns 73+ are one
    /// Comment token (trimmed) when not blank. Nothing crosses a line: a
    /// literal continued on the next line is two tokens.
    Cobol,
    /// Fixed-form RPG IV: columns 1-5 (sequence) are one Comment when not
    /// blank; column 6 (the form type, `H` `F` `D` `C` ...) is its own token;
    /// a `*` in column 7 makes the rest of the line a Comment; columns 7-80
    /// are code (free-form lines with blank columns 6-7 work unchanged) and
    /// columns 81+ one Comment. A file whose first line is `**FREE`
    /// (case-insensitive) is fully free-form: no column handling at all.
    Rpg,
}

/// Dialect switches for `tokenize_with`. The default is the language-agnostic
/// fallback used for every language without an extractor. Every switch is off
/// by default; named dialects (`TokenizerOptions::PYTHON`, ...) are ready-made
/// combinations, written as `TokenizerOptions { x: true, ..TokenizerOptions::DEFAULT }`.
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
    /// `//` is not a line comment (Python floor division, Haskell operators,
    /// shell paths): the slashes are Operator tokens.
    pub no_line_slash_comments: bool,
    /// `/* ... */` is not a block comment (shell globs such as `/tmp/*`).
    pub no_block_slash_comments: bool,
    /// `#` starts a Comment running to the end of the line (Python, shell,
    /// R, Elixir, GDScript, GNU as). With `shell_words`, only a `#` that
    /// starts a word (at line start or after whitespace or `;` `&` `|` `(`
    /// `)`) is a comment, so `a#b` and `${#x}` are not.
    pub hash_comments: bool,
    /// Restricts `hash_comments` to a `#` that is the first token on its
    /// line, so ARM immediates (`mov r0, #1`) are not comments.
    pub hash_comments_line_start_only: bool,
    /// `--` starts a Comment to the end of the line (SQL, Haskell). With
    /// `haskell_block_comments` the Haskell rule applies: a run of two or
    /// more dashes is a comment only when neither preceded nor followed by
    /// another symbol character, so `-->`, `--|` and `|--` are operators.
    pub dash_comments: bool,
    /// `{- ... -}` is a Comment, nesting (Haskell; pragmas `{-# ... #-}`
    /// included). Unterminated runs to end of input.
    pub haskell_block_comments: bool,
    /// `(* ... *)` is a Comment, nesting (F#, OCaml). `(*)` (the
    /// multiplication operator in parentheses) is not a comment.
    pub ml_block_comments: bool,
    /// `;` starts a Comment to the end of the line (NASM, MASM, Lisp).
    pub semicolon_comments: bool,
    /// `"""..."""` and `'''...'''` are one Literal each, spanning lines,
    /// with backslash escapes (Python, Elixir, Java text blocks, GDScript).
    pub triple_quote_strings: bool,
    /// Makes `triple_quote_strings` raw: no backslash escapes, the first
    /// closing triple quote ends the literal (Scala, F#).
    pub triple_quote_raw: bool,
    /// `'...'` is a Literal where `''` is an escaped quote, with no
    /// backslash escapes, on one line (an unterminated one stops at the line
    /// end): COBOL, RPG, NASM/MASM.
    pub doubled_single_quotes: bool,
    /// An identifier may contain `'` after its first character (Haskell and
    /// F# primes: `foldl'`, `x'`), so a `'` right after an identifier is
    /// never a char literal.
    pub prime_idents: bool,
    /// `` `...` `` is one Literal with no escapes, spanning lines (Go raw
    /// strings, R quoted names). Unterminated runs to end of input.
    pub raw_backtick_strings: bool,
    /// SQL quoting: `'...'` is a Literal where `''` is an escaped quote (no
    /// backslash escapes, may span lines); `"..."` (with `""` escapes) and
    /// `[...]` (single line, starting with a letter, `_`, `@`, `#` or space,
    /// no nested `[`) are quoted-identifier Identifier tokens.
    pub sql_strings: bool,
    /// Shell words: `$name`, `$1`, `$@`/`$*`/`$#`/`$?`/`$$`/`$!`/`$-` and
    /// `${...}` (braces nest, one line) are single Identifier tokens (`$(`
    /// stays `$` then `(`, so command substitutions balance as parentheses);
    /// `'...'` is a Literal with no escapes that may span lines (an
    /// unterminated one stops at its line end); ANSI-C `$'...'` is one
    /// Literal with backslash escapes; a heredoc marker `<<WORD`,
    /// `<<-WORD`, `<<'WORD'` or `<<"WORD"` is one Operator token, and its
    /// body, from the next line through the terminating `WORD` line (leading
    /// tabs ignored for `<<-`), is one Literal token (leading whitespace
    /// excluded). `<<<` is a here-string, not a heredoc, and `<<` inside
    /// arithmetic `((...))` / `$((...))` is a shift; an unquoted delimiter
    /// (optionally `\`-escaped) must start with a letter or `_`. A `"..."`
    /// string holding a command substitution is one Literal through its
    /// real closing quote, however the substitution nests quotes
    /// (`"$(basename "$f")"`); when the string or substitution never
    /// closes, the string ends at its next `"` as without this rule, and so
    /// do all later strings in the file (keeping lexing linear).
    pub shell_words: bool,
    /// `-` joins an identifier when directly followed by a letter, digit or
    /// `_` (COBOL `WORKING-STORAGE`, RPG `dcl-proc`). Note `a-b` is then one
    /// identifier; spaced subtraction is unaffected.
    pub hyphen_idents: bool,
    /// Fixed-column source layout (COBOL, fixed-form RPG); `None` is free form.
    pub fixed_columns: Option<FixedLayout>,
    /// Python string prefixes: `r`, `u`, `b`, `f`, `t` and the two-letter
    /// combinations (`rb`, `br`, `fr`, `rf`, `tr`, `rt`), any case, directly
    /// followed by a quote make one Literal with their string (`rb'''x'''`,
    /// `f"{a}"`). Escapes are lexed as for an unprefixed string (so raw
    /// `r"a\"b"` stays one literal, as in Python). A `"` inside an f-string
    /// replacement field ends the literal early (spans stay exact).
    pub python_string_prefixes: bool,
    /// R raw strings: `r"(...)"`, `R'[...]'`, `r"{...}"` with optional
    /// dashes between the quote and the bracket (`r"--(...)--"`), one
    /// Literal. Without its closing sequence the `r` stays an identifier,
    /// and the rest of the file lexes without this rule (so an unterminated
    /// raw string is scanned for once, keeping lexing linear).
    pub r_raw_strings: bool,
    /// MySQL `#` comments, SQL-safe subset: a `#` that is the first token on
    /// its line and is followed by whitespace or the line end is a Comment
    /// to the end of the line. `#temp` / `##temp` (T-SQL temporary tables),
    /// `#` after code (PostgreSQL XOR) and `#comment` without a space stay
    /// code. Known misfire: a PostgreSQL XOR operator that starts a
    /// continuation line (`SELECT a\n# b`) is read as a comment.
    pub sql_hash_comments: bool,
    /// RPG compile-time data: from the first line starting with `**CTDATA`,
    /// `**FTRANS` or `**ALTSEQ` (any case) or, in fixed form only, `**`
    /// followed by a blank or the line end (so a free-form `** 2;`
    /// continuation is code; never the `**FREE` first line), each section (its marker
    /// line and data lines up to the next marker) is one Literal, trimmed.
    pub rpg_compile_time_data: bool,
}

impl TokenizerOptions {
    /// Every switch off: the language-agnostic fallback, equal to
    /// `TokenizerOptions::default()` but usable in `const` items.
    pub const DEFAULT: Self = Self {
        rust_literals: false,
        single_quote_strings: false,
        csharp_strings: false,
        markup: false,
        aspx: false,
        regex_literals: false,
        no_line_slash_comments: false,
        no_block_slash_comments: false,
        hash_comments: false,
        hash_comments_line_start_only: false,
        dash_comments: false,
        haskell_block_comments: false,
        ml_block_comments: false,
        semicolon_comments: false,
        triple_quote_strings: false,
        triple_quote_raw: false,
        doubled_single_quotes: false,
        prime_idents: false,
        raw_backtick_strings: false,
        sql_strings: false,
        shell_words: false,
        hyphen_idents: false,
        fixed_columns: None,
        python_string_prefixes: false,
        r_raw_strings: false,
        sql_hash_comments: false,
        rpg_compile_time_data: false,
    };
    /// Rust (with the extractor): raw and byte literals.
    pub const RUST: Self = Self {
        rust_literals: true,
        ..Self::DEFAULT
    };
    /// C#: verbatim and interpolated strings.
    pub const CSHARP: Self = Self {
        csharp_strings: true,
        ..Self::DEFAULT
    };
    /// JavaScript (and TypeScript): `'...'` strings and regex literals.
    pub const JAVASCRIPT: Self = Self {
        single_quote_strings: true,
        regex_literals: true,
        ..Self::DEFAULT
    };
    /// TypeScript: the JavaScript dialect.
    pub const TYPESCRIPT: Self = Self::JAVASCRIPT;
    /// HTML and XML-like markup.
    pub const HTML: Self = Self {
        markup: true,
        ..Self::DEFAULT
    };
    /// ASP.NET markup with C# server tags.
    pub const ASPX: Self = Self {
        csharp_strings: true,
        markup: true,
        aspx: true,
        ..Self::DEFAULT
    };
    /// C: the default (`//`, `/* */`, `"..."`, char literals).
    pub const C: Self = Self::DEFAULT;
    /// C++: the default (raw strings `R"(...)"` are not special).
    pub const CPP: Self = Self::DEFAULT;
    /// Java: text blocks `"""..."""`.
    pub const JAVA: Self = Self {
        triple_quote_strings: true,
        ..Self::DEFAULT
    };
    /// Go: raw strings `` `...` ``.
    pub const GO: Self = Self {
        raw_backtick_strings: true,
        ..Self::DEFAULT
    };
    /// Scala: raw `"""..."""` strings.
    pub const SCALA: Self = Self {
        triple_quote_strings: true,
        triple_quote_raw: true,
        ..Self::DEFAULT
    };
    /// Python: `#` comments, `'...'` and triple-quoted strings, `//` is an
    /// operator.
    pub const PYTHON: Self = Self {
        single_quote_strings: true,
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        hash_comments: true,
        triple_quote_strings: true,
        python_string_prefixes: true,
        ..Self::DEFAULT
    };
    /// GDScript: the Python dialect (its `r"..."` raw strings are prefixed
    /// strings too).
    pub const GDSCRIPT: Self = Self::PYTHON;
    /// POSIX shell / bash.
    pub const SHELL: Self = Self {
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        hash_comments: true,
        shell_words: true,
        ..Self::DEFAULT
    };
    /// R: `#` comments, `'...'` strings, backtick names, raw strings.
    pub const R: Self = Self {
        single_quote_strings: true,
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        hash_comments: true,
        raw_backtick_strings: true,
        r_raw_strings: true,
        ..Self::DEFAULT
    };
    /// SQL (ANSI plus T-SQL `[names]`): `--` and `/* */` comments, and MySQL
    /// `# ` comments at line start (see `sql_hash_comments`).
    pub const SQL: Self = Self {
        no_line_slash_comments: true,
        dash_comments: true,
        sql_strings: true,
        sql_hash_comments: true,
        ..Self::DEFAULT
    };
    /// Haskell: `--` (Haskell rule) and nested `{- -}` comments, primes.
    pub const HASKELL: Self = Self {
        prime_idents: true,
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        dash_comments: true,
        haskell_block_comments: true,
        ..Self::DEFAULT
    };
    /// F#: `//` and nested `(* *)` comments, raw triple-quoted strings,
    /// primes.
    pub const FSHARP: Self = Self {
        no_block_slash_comments: true,
        ml_block_comments: true,
        triple_quote_strings: true,
        triple_quote_raw: true,
        prime_idents: true,
        ..Self::DEFAULT
    };
    /// Elixir: `#` comments, `'...'` charlists, `"""` heredocs.
    pub const ELIXIR: Self = Self {
        single_quote_strings: true,
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        hash_comments: true,
        triple_quote_strings: true,
        ..Self::DEFAULT
    };
    /// Assembly (NASM, MASM, GNU as; x86 and ARM): `;` comments, `#`
    /// comments only at line start (ARM `#1` immediates stay code), `//`
    /// and `/* */` comments, `'...'` strings with `''` escapes. Known
    /// limits: `;` always starts a comment (GNU as uses it as a statement
    /// separator on some targets); a trailing AT&T `# comment` after code is
    /// not a comment; ARM32 `@` comments are not recognised (`@` stays an
    /// operator, since MASM uses `@@:` labels at line start). Both are kept
    /// out on purpose (#127): a trailing `#` cannot be told from an ARM
    /// immediate (`mov r0, # 1`), and `@` is code in MASM (`@@:`, `jmp @F`)
    /// and GNU as x86 (`.type f, @function`).
    pub const ASM: Self = Self {
        doubled_single_quotes: true,
        hash_comments: true,
        hash_comments_line_start_only: true,
        semicolon_comments: true,
        ..Self::DEFAULT
    };
    /// COBOL reference format (fixed columns, hyphenated names).
    pub const COBOL: Self = Self {
        doubled_single_quotes: true,
        no_line_slash_comments: true,
        no_block_slash_comments: true,
        hyphen_idents: true,
        fixed_columns: Some(FixedLayout::Cobol),
        ..Self::DEFAULT
    };
    /// RPG IV: fixed form unless the file starts with `**FREE`; `//`
    /// comments, hyphenated names (`dcl-proc`), compile-time data.
    pub const RPG: Self = Self {
        doubled_single_quotes: true,
        no_block_slash_comments: true,
        hyphen_idents: true,
        fixed_columns: Some(FixedLayout::Rpg),
        rpg_compile_time_data: true,
        ..Self::DEFAULT
    };
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
    let mut lx = Lexer {
        src,
        opts,
        out: Vec::new(),
        i: 0,
        line: 1,
        col: 1,
        m: MarkupState::default(),
        heredocs: Vec::new(),
        parens: Vec::new(),
        shell_nest_off: false,
        r_raw_off: false,
    };
    let end = if opts.rpg_compile_time_data {
        rpg_data_start(src)
    } else {
        src.len()
    };
    match opts.fixed_columns {
        Some(FixedLayout::Rpg) if is_free_rpg(src) => lx.run(end),
        Some(layout) => lx.fixed(layout, end),
        None => lx.run(end),
    }
    if end < src.len() {
        lx.data_sections(end);
    }
    lx.out
}

/// RPG: a compile-time data marker line: `**CTDATA`, `**FTRANS`,
/// `**ALTSEQ` (any case), or, in a fixed-form file (`free` false), `**`
/// followed by a blank or the line end. A `**` exponent operator continuing
/// a free-form expression (`**2;`, `** 2;`) is not one.
pub fn is_rpg_data_marker(line: &str, free: bool) -> bool {
    let Some(rest) = line.strip_prefix("**") else {
        return false;
    };
    let rest = rest.trim_end_matches(['\r', '\n']);
    let upper = rest.to_ascii_uppercase();
    (!free && (rest.is_empty() || rest.starts_with([' ', '\t'])))
        || ["CTDATA", "FTRANS", "ALTSEQ"]
            .iter()
            .any(|k| upper.starts_with(k))
}

/// RPG: byte offset where compile-time data starts (the first data marker
/// line, never the `**FREE` first line), or `src.len()` when there is none.
pub fn rpg_data_start(src: &str) -> usize {
    let free = is_free_rpg(src);
    let mut off = 0;
    for (n, line) in src.split_inclusive('\n').enumerate() {
        let l = if n == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if is_rpg_data_marker(l, free) && !(n == 0 && free) {
            return off;
        }
        off += line.len();
    }
    src.len()
}

/// `**FREE` (any case) as the whole first line, after an optional BOM.
fn is_free_rpg(src: &str) -> bool {
    let s = src.strip_prefix('\u{feff}').unwrap_or(src);
    let first = s.split('\n').next().unwrap_or("");
    let first = first.strip_suffix('\r').unwrap_or(first).trim_end();
    first.eq_ignore_ascii_case("**free")
}

fn is_gap(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

struct Lexer<'a> {
    src: &'a str,
    opts: TokenizerOptions,
    out: Vec<TokenDecl>,
    i: usize,
    line: u32,
    col: u32,
    m: MarkupState,
    /// Heredoc bodies awaiting the next newline: (delimiter, strip tabs).
    heredocs: Vec<(String, bool)>,
    /// `shell_words`: open parentheses, true when inside arithmetic `((`.
    parens: Vec<bool>,
    /// A nested `"$(...)"` scan read to the end of input without closing:
    /// later strings use the plain rule (keeps lexing linear).
    shell_nest_off: bool,
    /// Same for an unterminated R raw string.
    r_raw_off: bool,
}

impl Lexer<'_> {
    /// Advance position tracking over `src[i..to]` (no token).
    fn adv_to(&mut self, to: usize) {
        for c in self.src[self.i..to].chars() {
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else if c != '\u{feff}' {
                self.col += 1;
            }
        }
        self.i = to;
    }

    /// Emit `src[i..end]` as one token.
    fn push(&mut self, end: usize, class: TokenClass) {
        let (start, sl, sc) = (self.i, self.line, self.col);
        self.adv_to(end);
        self.out.push(TokenDecl {
            text: self.src[start..end].to_string(),
            class,
            span: Span {
                start: start as u32,
                end: end as u32,
                start_line: sl,
                start_col: sc,
                end_line: self.line,
                end_col: self.col,
            },
        });
    }

    /// Emit `src[from..to]`, trimmed of whitespace, as one Comment if any is left.
    fn trimmed_comment(&mut self, from: usize, to: usize) {
        self.trimmed(from, to, TokenClass::Comment);
    }

    /// Emit `src[from..to]`, trimmed of whitespace, as one `class` token if
    /// any is left.
    fn trimmed(&mut self, from: usize, to: usize, class: TokenClass) {
        let s = &self.src[from..to];
        let t = s.trim_start_matches(is_gap);
        let start = to - t.len();
        let end = start + t.trim_end_matches(is_gap).len();
        if end > start {
            self.adv_to(start);
            self.push(end, class);
        }
    }

    /// Nothing emitted yet on the current line.
    fn at_line_start(&self) -> bool {
        self.out.last().is_none_or(|t| t.span.end_line < self.line)
    }

    /// RPG compile-time data from `from` (a line start) to the end: one
    /// trimmed Literal per section, a section starting at each marker line.
    fn data_sections(&mut self, from: usize) {
        let src = self.src;
        let free = is_free_rpg(src);
        let mut start = from;
        let mut off = from;
        for line in src[from..].split_inclusive('\n') {
            if off > start && is_rpg_data_marker(line, free) {
                self.trimmed(start, off, TokenClass::Literal);
                start = off;
            }
            off += line.len();
        }
        self.trimmed(start, src.len(), TokenClass::Literal);
    }

    /// Lex a fixed-column source line by line, up to `limit` (a line start
    /// or the end of input).
    fn fixed(&mut self, layout: FixedLayout, limit: usize) {
        let src = self.src;
        let (seq, lead, code_end) = match layout {
            FixedLayout::Cobol => (6, false, 72),
            FixedLayout::Rpg => (5, true, 80),
        };
        while self.i < limit {
            let ls = self.i;
            let le = src[ls..].find('\n').map_or(src.len(), |p| ls + p);
            let ce = if src[ls..le].ends_with('\r') {
                le - 1
            } else {
                le
            };
            // Byte offset where column `n` (1-based) starts, clamped to `ce`.
            let col_at = |n: usize| {
                src[ls..ce]
                    .char_indices()
                    .filter(|&(_, c)| c != '\u{feff}')
                    .nth(n - 1)
                    .map_or(ce, |(p, _)| ls + p)
            };
            let seq_end = col_at(seq + 1);
            self.trimmed_comment(ls, seq_end);
            let mut code_start = seq_end;
            if lead {
                // RPG column 6: the form type, its own token.
                code_start = col_at(seq + 2);
                self.adv_to(seq_end);
                self.run(code_start);
            }
            let ind_end = col_at(8);
            let ind = &src[code_start..ind_end];
            if ind == "*" || (layout == FixedLayout::Cobol && ind == "/") {
                self.trimmed_comment(code_start, ce);
            } else {
                let area_end = col_at(code_end + 1);
                if layout == FixedLayout::Cobol {
                    // Column 7 indicator on its own.
                    self.adv_to(code_start);
                    self.run(ind_end);
                    code_start = ind_end;
                }
                self.adv_to(code_start);
                self.run(area_end);
                self.trimmed_comment(area_end, ce);
            }
            self.adv_to((le + 1).min(src.len()));
        }
    }

    /// Lex `src[i..limit]` with the free-form rules.
    fn run(&mut self, limit: usize) {
        let src = self.src;
        let opts = self.opts;
        while self.i < limit {
            let i = self.i;
            let c = src[i..].chars().next().unwrap();
            if is_gap(c) {
                self.adv_to(i + c.len_utf8());
                if c == '\n' && !self.heredocs.is_empty() {
                    self.heredoc_bodies(limit);
                }
                continue;
            }
            let rest = &src[i..limit];
            let hash = c == '#'
                && ((opts.hash_comments && self.hash_ok())
                    || (opts.sql_hash_comments
                        && self.at_line_start()
                        && rest[1..].chars().next().is_none_or(char::is_whitespace)));
            let mut m = std::mem::take(&mut self.m);
            // Markup rules apply outside server-tag code.
            let markup = opts.markup && !m.code;
            let to_eol = || rest.find('\n').unwrap_or(rest.len());
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
            } else if hash
                || (opts.fixed_columns == Some(FixedLayout::Cobol) && rest.starts_with("*>"))
                || (opts.dash_comments && self.dash_comment_at(rest))
                || (opts.semicolon_comments && c == ';')
            {
                // A comment to the end of the line.
                (to_eol(), TokenClass::Comment)
            } else if opts.haskell_block_comments && rest.starts_with("{-") {
                (nested_len(rest, "{-", "-}"), TokenClass::Comment)
            } else if opts.ml_block_comments && rest.starts_with("(*") && !rest.starts_with("(*)") {
                (nested_len(rest, "(*", "*)"), TokenClass::Comment)
            } else if !markup && !opts.no_line_slash_comments && rest.starts_with("//") {
                let mut n = to_eol();
                // ASP.NET ends a server block at `%>` even inside a `//` comment.
                if m.code {
                    n = rest[..n].find("%>").unwrap_or(n);
                }
                (n, TokenClass::Comment)
            } else if let Some(after) = rest
                .strip_prefix("/*")
                .filter(|_| !markup && !opts.no_block_slash_comments)
            {
                (
                    after.find("*/").map_or(rest.len(), |p| p + 4),
                    TokenClass::Comment,
                )
            } else if let Some(n) = (opts.shell_words && c == '$')
                .then(|| shell_var_len(rest))
                .flatten()
            {
                (n, TokenClass::Identifier)
            } else if let Some((n, delim, strip)) = (opts.shell_words
                && c == '<'
                && !src[..i].ends_with('<')
                && !self.parens.last().copied().unwrap_or(false))
            .then(|| heredoc_marker(rest))
            .flatten()
            {
                self.heredocs.push((delim, strip));
                (n, TokenClass::Operator)
            } else if opts.shell_words && rest.starts_with("$'") {
                (1 + quoted_len(&rest[1..], '\''), TokenClass::Literal)
            } else if opts.shell_words && c == '\'' && !markup {
                let n = rest[1..]
                    .find('\'')
                    .map_or_else(|| line_len(rest), |p| p + 2);
                (n, TokenClass::Literal)
            } else if let Some(n) = opts
                .triple_quote_strings
                .then(|| triple_quoted_len(rest, opts.triple_quote_raw))
                .flatten()
            {
                (n, TokenClass::Literal)
            } else if opts.raw_backtick_strings && c == '`' && !markup {
                (
                    rest[1..].find('`').map_or(rest.len(), |p| p + 2),
                    TokenClass::Literal,
                )
            } else if let Some(n) = (opts.sql_strings && c == '\'').then(|| doubled_len(rest)) {
                (n, TokenClass::Literal)
            } else if let Some(n) = (opts.sql_strings && c == '"').then(|| doubled_len(rest)) {
                (n, TokenClass::Identifier)
            } else if let Some(n) = (opts.sql_strings && c == '[')
                .then(|| sql_bracket_len(rest))
                .flatten()
            {
                (n, TokenClass::Identifier)
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
            } else if let Some(n) = (opts.python_string_prefixes && !markup)
                .then(|| python_prefixed_len(rest, opts.single_quote_strings))
                .flatten()
            {
                (n, TokenClass::Literal)
            } else if let Some(n) = (opts.r_raw_strings && !markup && !self.r_raw_off)
                .then(|| {
                    r_raw_len(rest).unwrap_or_else(|()| {
                        self.r_raw_off = true;
                        None
                    })
                })
                .flatten()
            {
                (n, TokenClass::Literal)
            } else if c.is_alphabetic() || c == '_' {
                (
                    ident_len(rest, markup && m.tag, opts.hyphen_idents, opts.prime_idents),
                    TokenClass::Identifier,
                )
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
            } else if c == '\'' && opts.doubled_single_quotes {
                (doubled_len(&rest[..line_len(rest)]), TokenClass::Literal)
            } else if c == '\'' && opts.single_quote_strings {
                (single_quoted_len(rest), TokenClass::Literal)
            } else if let Some(n) =
                (opts.shell_words && c == '"' && !markup && !self.shell_nest_off)
                    .then(|| {
                        shell_dq_len(rest).unwrap_or_else(|()| {
                            self.shell_nest_off = true;
                            None
                        })
                    })
                    .flatten()
            {
                (n, TokenClass::Literal)
            } else if c == '"' || c == '`' || (c == '\'' && is_char_literal(rest)) {
                (quoted_len(rest, c), TokenClass::Literal)
            } else if let Some(n) =
                (opts.regex_literals && c == '/' && regex_allowed(self.out.last()))
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
                        m.tag =
                            rest[1..].starts_with(|n: char| n.is_alphabetic() || "/!?".contains(n))
                    }
                    '>' => m.tag = false,
                    _ => {}
                }
            }
            self.m = m;
            if opts.shell_words && len == 1 {
                match c {
                    '(' => {
                        let arith = rest.starts_with("((") || self.parens.last() == Some(&true);
                        self.parens.push(arith);
                    }
                    ')' => {
                        self.parens.pop();
                    }
                    _ => {}
                }
            }
            self.push(i + len, class);
        }
    }

    /// Whether `rest` (at `i`) starts a `--` comment under `dash_comments`.
    fn dash_comment_at(&self, rest: &str) -> bool {
        if !rest.starts_with("--") {
            return false;
        }
        if !self.opts.haskell_block_comments {
            return true;
        }
        const SYMBOLS: &str = "!#$%&*+./<=>?@\\^|-~:";
        let dashes = rest.len() - rest.trim_start_matches('-').len();
        let after = rest[dashes..].chars().next();
        let before = self.src[..self.i].chars().next_back();
        !after.is_some_and(|c| SYMBOLS.contains(c)) && !before.is_some_and(|c| SYMBOLS.contains(c))
    }

    /// Whether a `#` at `i` starts a comment under `hash_comments`.
    fn hash_ok(&self) -> bool {
        if self.opts.hash_comments_line_start_only && !self.at_line_start() {
            return false;
        }
        if self.opts.shell_words && self.i > 0 {
            let prev = self.src[..self.i].chars().next_back().unwrap();
            return is_gap(prev) || ";&|()".contains(prev);
        }
        true
    }

    /// Emit pending heredoc bodies starting at `i` (just after a newline).
    fn heredoc_bodies(&mut self, limit: usize) {
        let src = self.src;
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            let start = self.i;
            if start >= limit {
                break;
            }
            let mut p = start;
            let mut end = limit;
            while p < limit {
                let le = src[p..limit].find('\n').map_or(limit, |q| p + q);
                let content = src[p..le].strip_suffix('\r').unwrap_or(&src[p..le]);
                let body = if strip {
                    content.trim_start_matches('\t')
                } else {
                    content
                };
                if body == delim {
                    end = p + content.len();
                    break;
                }
                p = le + 1;
            }
            let first = src[start..end]
                .find(|c: char| !is_gap(c))
                .map_or(end, |q| start + q);
            if first < end {
                self.adv_to(first);
                self.push(end, TokenClass::Literal);
            } else {
                self.adv_to(end);
            }
            // Step over the terminator's line break before the next body.
            if let Some(nl) = src[self.i..limit].find('\n') {
                if src[self.i..self.i + nl].chars().all(is_gap) {
                    self.adv_to(self.i + nl + 1);
                    continue;
                }
            }
            break;
        }
    }
}

/// Length of `rest` up to (not including) its line break (`\n` or `\r\n`).
fn line_len(rest: &str) -> usize {
    let mut line = rest.find('\n').unwrap_or(rest.len());
    if rest[..line].ends_with('\r') && line > 1 {
        line -= 1;
    }
    line
}

/// A nesting comment from `open` to the matching `close`; unterminated runs
/// to end of input.
fn nested_len(rest: &str, open: &str, close: &str) -> usize {
    let (mut depth, mut p) = (0usize, 0usize);
    while p < rest.len() {
        if rest[p..].starts_with(open) {
            depth += 1;
            p += open.len();
        } else if rest[p..].starts_with(close) {
            depth -= 1;
            p += close.len();
            if depth == 0 {
                return p;
            }
        } else {
            p += rest[p..].chars().next().unwrap().len_utf8();
        }
    }
    rest.len()
}

/// `"""..."""` or `'''...'''` with backslash escapes, if `rest` starts with one.
fn triple_quoted_len(rest: &str, raw: bool) -> Option<usize> {
    let q = ["\"\"\"", "'''"]
        .into_iter()
        .find(|q| rest.starts_with(q))?;
    let mut esc = false;
    for (p, ch) in rest.char_indices().skip(3) {
        if esc {
            esc = false;
        } else if ch == '\\' && !raw {
            esc = true;
        } else if rest[p..].starts_with(q) {
            return Some(p + 3);
        }
    }
    Some(rest.len())
}

/// A quoted run where a doubled quote is an escaped one (SQL `'it''s'`,
/// `"a""b"`); no backslash escapes; unterminated runs to end of input.
fn doubled_len(rest: &str) -> usize {
    let b = rest.as_bytes();
    let q = b[0];
    let mut p = 1;
    while p < b.len() {
        if b[p] == q {
            if b.get(p + 1) == Some(&q) {
                p += 2;
                continue;
            }
            return p + 1;
        }
        p += 1;
    }
    rest.len()
}

/// T-SQL `[quoted name]` on one line.
fn sql_bracket_len(rest: &str) -> Option<usize> {
    let first = rest[1..].chars().next()?;
    if !(first.is_alphabetic() || "_@# ".contains(first)) {
        return None;
    }
    let body = &rest[1..line_len(rest)];
    let close = body.find(']')?;
    (!body[..close].contains('[')).then_some(close + 2)
}

/// A shell parameter expansion at `rest[0] == '$'`.
fn shell_var_len(rest: &str) -> Option<usize> {
    let mut it = rest[1..].chars();
    match it.next()? {
        '{' => {
            let mut depth = 0usize;
            for (p, ch) in rest.char_indices().skip(1) {
                match ch {
                    '\n' => return None,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(p + 1);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        c if c.is_ascii_alphabetic() || c == '_' => Some(
            1 + rest[1..]
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len() - 1),
        ),
        c if c.is_ascii_digit() || "@*#?$!-".contains(c) => Some(2),
        _ => None,
    }
}

/// A heredoc marker `<<WORD` / `<<-WORD` / `<<'WORD'` / `<<"WORD"`:
/// (marker length, delimiter, strip leading tabs).
fn heredoc_marker(rest: &str) -> Option<(usize, String, bool)> {
    let after = rest.strip_prefix("<<")?;
    if after.starts_with('<') {
        return None;
    }
    let strip = after.starts_with('-');
    let mut p = 2 + usize::from(strip);
    p += rest[p..]
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(rest.len() - p);
    let word = &rest[p..];
    let q = word.chars().next()?;
    if q == '\'' || q == '"' {
        let close = word[1..line_len(word)].find(q)?;
        let delim = &word[1..1 + close];
        return (!delim.is_empty()).then(|| (p + close + 2, delim.to_string(), strip));
    }
    let (skip, word) = match word.strip_prefix('\\') {
        Some(w) => (1, w),
        None => (0, word),
    };
    if !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    let n = word
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(word.len());
    Some((p + skip + n, word[..n].to_string(), strip))
}

/// Length of an identifier at the start of `rest`. In markup, `-` and `:`
/// continue it when followed by a letter, digit or `_` (`data-id`, `asp:Button`);
/// with `hyphen`, `-` does.
fn ident_len(rest: &str, markup: bool, hyphen: bool, prime: bool) -> usize {
    let word = |ch: char| ch.is_alphanumeric() || ch == '_' || (prime && ch == '\'');
    let mut it = rest.char_indices().peekable();
    while let Some((p, ch)) = it.next() {
        if word(ch) {
            continue;
        }
        let joins = ((markup && (ch == '-' || ch == ':')) || (hyphen && ch == '-'))
            && it.peek().is_some_and(|&(_, next)| word(next));
        if !joins {
            return p;
        }
    }
    rest.len()
}

/// `'...'` with backslash escapes, ending at the closing quote or before the
/// end of the line (`\n` or `\r\n`).
fn single_quoted_len(rest: &str) -> usize {
    quoted_len(&rest[..line_len(rest)], '\'')
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

/// A Python prefixed string (`r"..."`, `rb'''...'''`, `f"..."`) at the start
/// of `rest`. `sq`: `'...'` is a string in this dialect.
fn python_prefixed_len(rest: &str, sq: bool) -> Option<usize> {
    let p = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let prefix = rest[..p].to_ascii_lowercase();
    const PREFIXES: &[&str] = &["r", "u", "b", "f", "t", "br", "rb", "fr", "rf", "tr", "rt"];
    if !PREFIXES.contains(&prefix.as_str()) {
        return None;
    }
    let q = &rest[p..];
    let n = if let Some(n) = triple_quoted_len(q, false) {
        n
    } else if q.starts_with('"') {
        quoted_len(q, '"')
    } else if q.starts_with('\'') && sq {
        single_quoted_len(q)
    } else {
        return None;
    };
    Some(p + n)
}

/// An R raw string (`r"(...)"`, `R'--[...]--'`) at the start of `rest`:
/// `Ok(None)` when `rest` does not open one, `Err` when it opens one that
/// never closes (the scan read to the end of input; the caller then stops
/// trying, which keeps lexing linear).
fn r_raw_len(rest: &str) -> Result<Option<usize>, ()> {
    let b = rest.as_bytes();
    if !matches!(b[0], b'r' | b'R') {
        return Ok(None);
    }
    let Some(&q) = b.get(1) else {
        return Ok(None);
    };
    if q != b'"' && q != b'\'' {
        return Ok(None);
    }
    let dashes = b[2..].iter().take_while(|&&c| c == b'-').count();
    let close = match b.get(2 + dashes) {
        Some(b'(') => b')',
        Some(b'[') => b']',
        Some(b'{') => b'}',
        _ => return Ok(None),
    };
    let mut term = vec![close];
    term.extend(std::iter::repeat_n(b'-', dashes));
    term.push(q);
    let body = 3 + dashes;
    b[body..]
        .windows(term.len())
        .position(|w| w == term.as_slice())
        .map(|p| Some(body + p + term.len()))
        .ok_or(())
}

/// Deepest nesting of quotes and command substitutions followed by
/// [`shell_dq_len`]; deeper input falls back to the plain rule.
const SHELL_NEST_MAX: usize = 64;

/// A shell `"..."` string at the start of `rest` that holds a `$(...)`
/// command substitution, through its real closing quote (quotes inside the
/// substitution nest). `None` (use the plain rule) when it holds none, or
/// when anything in it is unterminated.
/// `Err` when the string or a substitution in it never closes (the scan
/// read to the end of input; the caller then stops trying, which keeps
/// lexing linear).
fn shell_dq_len(rest: &str) -> Result<Option<usize>, ()> {
    let (end, subst) = shell_dq_end(rest.as_bytes(), 1, 0).ok_or(())?;
    Ok(subst.then_some(end))
}

/// End (after the closing `"`) of a double-quoted string whose body starts
/// at `p`, and whether it held a command substitution.
fn shell_dq_end(b: &[u8], mut p: usize, depth: usize) -> Option<(usize, bool)> {
    if depth > SHELL_NEST_MAX {
        return None;
    }
    let mut subst = false;
    while p < b.len() {
        match b[p] {
            b'\\' => p += 2,
            b'"' => return Some((p + 1, subst)),
            b'$' if b.get(p + 1) == Some(&b'(') => {
                p = shell_subst_end(b, p + 2, depth + 1)?;
                subst = true;
            }
            _ => p += 1,
        }
    }
    None
}

/// End (after the matching `)`) of a command substitution whose body starts
/// at `p`.
fn shell_subst_end(b: &[u8], mut p: usize, depth: usize) -> Option<usize> {
    if depth > SHELL_NEST_MAX {
        return None;
    }
    let mut parens = 1usize;
    while p < b.len() {
        match b[p] {
            b'\\' => p += 2,
            b'(' => {
                parens += 1;
                p += 1;
            }
            b')' => {
                parens -= 1;
                p += 1;
                if parens == 0 {
                    return Some(p);
                }
            }
            b'"' => p = shell_dq_end(b, p + 1, depth + 1)?.0,
            q @ (b'\'' | b'`') => p += 2 + b[p + 1..].iter().position(|&c| c == q)?,
            _ => p += 1,
        }
    }
    None
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
            "no_slash_comments" => {
                o.no_line_slash_comments = true;
                o.no_block_slash_comments = true;
            }
            "hash_comments" => o.hash_comments = true,
            "hash_comments_line_start_only" => {
                o.hash_comments = true;
                o.hash_comments_line_start_only = true;
            }
            "dash_comments" => o.dash_comments = true,
            "haskell_block_comments" => o.haskell_block_comments = true,
            "ml_block_comments" => o.ml_block_comments = true,
            "semicolon_comments" => o.semicolon_comments = true,
            "triple_quote_strings" => o.triple_quote_strings = true,
            "raw_backtick_strings" => o.raw_backtick_strings = true,
            "sql_strings" => o.sql_strings = true,
            "shell_words" => o.shell_words = true,
            "hyphen_idents" => o.hyphen_idents = true,
            "triple_quote_raw" => {
                o.triple_quote_strings = true;
                o.triple_quote_raw = true;
            }
            "doubled_single_quotes" => o.doubled_single_quotes = true,
            "prime_idents" => o.prime_idents = true,
            "haskell_dashes" => {
                o.dash_comments = true;
                o.haskell_block_comments = true;
            }
            "python_string_prefixes" => {
                o.python_string_prefixes = true;
                o.single_quote_strings = true;
            }
            "r_raw_strings" => o.r_raw_strings = true,
            "sql_hash_comments" => o.sql_hash_comments = true,
            "rpg_compile_time_data" => o.rpg_compile_time_data = true,
            "fixed_cobol" => o.fixed_columns = Some(FixedLayout::Cobol),
            "fixed_rpg" => o.fixed_columns = Some(FixedLayout::Rpg),
            "PYTHON" => o = TokenizerOptions::PYTHON,
            "SHELL" => o = TokenizerOptions::SHELL,
            "SQL" => o = TokenizerOptions::SQL,
            "HASKELL" => o = TokenizerOptions::HASKELL,
            "FSHARP" => o = TokenizerOptions::FSHARP,
            "ASM" => o = TokenizerOptions::ASM,
            "COBOL" => o = TokenizerOptions::COBOL,
            "RPG" => o = TokenizerOptions::RPG,
            "GO" => o = TokenizerOptions::GO,
            "SCALA" => o = TokenizerOptions::SCALA,
            "JAVA" => o = TokenizerOptions::JAVA,
            "ELIXIR" => o = TokenizerOptions::ELIXIR,
            "R" => o = TokenizerOptions::R,
            _ => unreachable!("{name}"),
        }
        o
    }

    /// One golden per new flag and named dialect over `DIALECT_SOURCES`
    /// plus `LANG_SOURCES` (kept apart so `DIALECT_GOLDENS` stays as it was).
    const LANG_GOLDENS: &[(&str, u64)] = &[
        ("no_slash_comments", 0xc081f4ffcb4f1e9e),
        ("hash_comments", 0xb7edae0e1c879f50),
        ("hash_comments_line_start_only", 0x5e178ed4279298ce),
        ("dash_comments", 0xaacfa7dafaab54dc),
        ("haskell_block_comments", 0x6a13323b4f9395f5),
        ("ml_block_comments", 0xe9431c94af1b6cae),
        ("semicolon_comments", 0xdfc41c99186c0853),
        ("triple_quote_strings", 0x701a5e21713678e3),
        ("raw_backtick_strings", 0xe892b7f535bc8f01),
        ("sql_strings", 0x5344ea80475f30c5),
        ("shell_words", 0x7a04de4ac97c4f40),
        ("triple_quote_raw", 0x7de27b9f0b907695),
        ("doubled_single_quotes", 0x9cf41e9ae363977b),
        ("prime_idents", 0x9869e64012232154),
        ("haskell_dashes", 0xcf2d98975a2e4bca),
        ("hyphen_idents", 0xcc2fb60810b3912f),
        ("fixed_cobol", 0x2320cbe948c5c2df),
        ("fixed_rpg", 0xa4b95ac258618d66),
        // PYTHON and SQL changed with #127 (`f' '` is a prefixed string,
        // `# 1` at line start a MySQL comment).
        ("PYTHON", 0x678bbdc6a72d75d4),
        ("SHELL", 0x722243900ce93348),
        ("SQL", 0xc65d90d669af700e),
        ("HASKELL", 0xc63b1d790c2884d6),
        ("FSHARP", 0x9985481f3cf3cc21),
        ("ASM", 0xef2217eae3f3ece6),
        ("COBOL", 0x9312a3956c379811),
        ("RPG", 0x9115ed893962f08b),
        ("GO", 0xe892b7f535bc8f01),
        ("SCALA", 0x7de27b9f0b907695),
        ("JAVA", 0x701a5e21713678e3),
        ("ELIXIR", 0x186548073abbdec0),
        ("R", 0xed04254ba06a584a),
    ];

    const LANG_SOURCES: &[&str] = &[
        "def f(a):\n    '''doc ''' \"\"\"x\\\"\"\"\"\" # c\n    return a // 2 /* g */ '''open",
        "x=${a:-${b}} $1 $@ $# a#b # c\ncat <<-'EOF' | wc # t\n\tbody $x\n\tEOF\necho $(ls /tmp/*) 'a\\' <<< s\ncat <<X <<\"Y\"\nx\nX\ny\nY\ncat <<Z\nunterminated",
        "SELECT [my col], \"q\"\"x\", 'it''s' -- c\n/* b */ FROM t WHERE a[1] = 'op\nen",
        "{- a {- b -} c -} x -- d\n(* e (* f *) *) (*) y `raw\nstr` {- open",
        "mov r0, #1 ; c\n# 1 \"f.s\"\n  ldr x, =lbl // d\n WORKING-STORAGE a-1 -b",
        "000100 IDENTIFICATION DIVISION.                                         SEQ00001\n000200* comment line\n000300 01 WS-COUNT PIC 9. *> inline\n      - 'cont'\n      /page\r\nshort\n",
        "     H DFTACTGRP(*NO)\n     D name            S             10A\n     C* comment\n      dcl-proc foo; // c\n     C                   EVAL      X = 1                                     cmt\n",
        "**free\ndcl-s x int(10); // ok\n     C* not a comment here\n",
        "f' 'a' x' = y' -- c\na --> b |-- c --| d --- e\n\"\"\"r\\\"\"\" 'it''s' $'a\\'b' $((1<<2)) cat <<\\EOF\nx\nEOF\n",
    ];

    #[test]
    fn lang_dialects_are_pinned() {
        let fnv = |opts: TokenizerOptions| {
            let mut h: u64 = 0xcbf29ce484222325;
            for src in DIALECT_SOURCES.iter().chain(LANG_SOURCES) {
                for b in format!("{:?}", tokenize_with(src, opts)).bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
            }
            h
        };
        let changed: Vec<String> = LANG_GOLDENS
            .iter()
            .filter_map(|&(name, golden)| {
                let got = fnv(dialect(name));
                (got != golden).then(|| format!("(\"{name}\", {got:#x})"))
            })
            .collect();
        assert!(
            changed.is_empty(),
            "dialect output changed: bump TOKENIZER_VERSION and update LANG_GOLDENS to {}",
            changed.join(", ")
        );
    }

    /// One golden per flag and named dialect touched by #127, over all the
    /// sources plus `FOLLOWUP_SOURCES` (kept apart so the tables above stay
    /// as they were).
    const FOLLOWUP_GOLDENS: &[(&str, u64)] = &[
        ("python_string_prefixes", 0xc68eafb894bc0bee),
        ("r_raw_strings", 0x4c3452a992769b92),
        ("sql_hash_comments", 0x2b14e4c7f36d24f5),
        ("rpg_compile_time_data", 0xa1756ba5cf388766),
        ("shell_words", 0xe9344cd0f4766ede),
        ("PYTHON", 0xd36e08dcaa43df52),
        ("SHELL", 0x4895503d40809bcf),
        ("SQL", 0xc9068899c2738aa5),
        ("R", 0xf73a78f8a954f09),
        ("RPG", 0xe215f5f654279785),
    ];

    const FOLLOWUP_SOURCES: &[&str] = &[
        "x = r\"a\\\"b\" + rb'''c\nd''' + f\"{y}\" + Br'e' + bar\"z\" + u'' + rx'no'",
        "x <- r\"(a \"q\" b)\" + R'--[c]--' + r\"{d}\" + r\"(open",
        "# mysql comment\nSELECT #t, ##g FROM #temp # xor\n#nospace\n  # indented\n",
        "     C                   EVAL      X = 1\n**CTDATA ARR\nabc  def\n'x\n** \nmore\n",
        "echo \"$(basename \"$f\")\" \"$(a \"$(b 'c)' \\\")\")\" x\" \"$(open \"q\"",
    ];

    #[test]
    fn followup_dialects_are_pinned() {
        let fnv = |opts: TokenizerOptions| {
            let mut h: u64 = 0xcbf29ce484222325;
            for src in DIALECT_SOURCES
                .iter()
                .chain(LANG_SOURCES)
                .chain(FOLLOWUP_SOURCES)
            {
                for b in format!("{:?}", tokenize_with(src, opts)).bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
            }
            h
        };
        let changed: Vec<String> = FOLLOWUP_GOLDENS
            .iter()
            .filter_map(|&(name, golden)| {
                let got = fnv(dialect(name));
                (got != golden).then(|| format!("(\"{name}\", {got:#x})"))
            })
            .collect();
        assert!(
            changed.is_empty(),
            "dialect output changed: bump the affected extractors' versions and update FOLLOWUP_GOLDENS to {}",
            changed.join(", ")
        );
    }

    fn dtoks_with(opts: TokenizerOptions, s: &str) -> Vec<(String, TokenClass)> {
        tokenize_with(s, opts)
            .into_iter()
            .map(|t| (t.text, t.class))
            .collect()
    }

    fn lits(opts: TokenizerOptions, s: &str) -> Vec<String> {
        dtoks_with(opts, s)
            .into_iter()
            .filter(|t| t.1 == TokenClass::Literal)
            .map(|t| t.0)
            .collect()
    }

    #[test]
    fn python_string_prefixes() {
        let py = TokenizerOptions::PYTHON;
        assert_eq!(
            lits(py, r#"r"a\"b" rb'''x''' F"{y}" Br'z' u"" t'q' bar"s""#),
            [
                r#"r"a\"b""#,
                "rb'''x'''",
                "F\"{y}\"",
                "Br'z'",
                "u\"\"",
                "t'q'",
                "\"s\""
            ]
        );
        // Not prefixes: other words, and a word glued to a string.
        let t = dtoks_with(py, "rx'a' print(b)");
        assert_eq!(t[0], ("rx".into(), TokenClass::Identifier));
        assert_eq!(t[1], ("'a'".into(), TokenClass::Literal));
        assert_eq!(t[2], ("print".into(), TokenClass::Identifier));
        assert_eq!(dtoks_with(py, "b")[0].1, TokenClass::Identifier);
    }

    #[test]
    fn r_raw_strings() {
        let r = TokenizerOptions::R;
        assert_eq!(
            lits(r, r#"r"(a "q" \ b)" R'--[c]-]--' r"{d}""#),
            [r#"r"(a "q" \ b)""#, "R'--[c]-]--'", "r\"{d}\""]
        );
        // Unclosed or not bracketed: `r` is an identifier.
        assert_eq!(dtoks_with(r, "r\"(open")[0].1, TokenClass::Identifier);
        assert_eq!(dtoks_with(r, "r\"x\"")[0].1, TokenClass::Identifier);
    }

    #[test]
    fn sql_hash_comments() {
        let sql = TokenizerOptions::SQL;
        let c: Vec<_> = dtoks_with(sql, "# c\nSELECT #t, a # b\n#x\n  # d\n#")
            .into_iter()
            .filter(|t| t.1 == TokenClass::Comment)
            .map(|t| t.0)
            .collect();
        assert_eq!(c, ["# c", "# d", "#"]);
    }

    #[test]
    fn rpg_compile_time_data() {
        let rpg = TokenizerOptions::RPG;
        let src =
            "     C                   EVAL      X = 1\n**CTDATA ARR\n  abc 'x\n**ctdata b\nq\n";
        let t = dtoks_with(rpg, src);
        let n = t.len();
        assert_eq!(
            t[n - 2],
            ("**CTDATA ARR\n  abc 'x".into(), TokenClass::Literal)
        );
        assert_eq!(t[n - 1], ("**ctdata b\nq".into(), TokenClass::Literal));
        assert!(t.iter().any(|x| x.0 == "EVAL"));
        // Free form too; `**FREE`, an exponent and a free-form `** 2;`
        // continuation are not markers.
        let t = dtoks_with(rpg, "**FREE\nx = y\n**2;\nz = w\n** 3;\n**CTDATA a\ndata\n");
        assert_eq!(
            t.last().unwrap(),
            &("**CTDATA a\ndata".into(), TokenClass::Literal)
        );
        assert!(t.iter().any(|x| x.0 == "2") && t.iter().any(|x| x.0 == "3"));
        assert!(t.iter().any(|x| x.0 == "FREE"));
        // The `**FREE` first line (with a BOM too) never starts data.
        assert_eq!(rpg_data_start("**FREE\nx;\n"), 10);
        assert_eq!(rpg_data_start("\u{feff}**free\r\nx;\n"), 14);
        assert_eq!(rpg_data_start("**FREE\n**ctdata x\n"), 7);
        // Fixed form: `**` alone is a marker (and a first-line one too).
        assert_eq!(rpg_data_start("** \nx\n"), 0);
        assert_eq!(rpg_data_start("     C  X\n**\nx\n"), 10);
    }

    #[test]
    fn shell_quotes_nest_in_command_substitutions() {
        let sh = TokenizerOptions::SHELL;
        assert_eq!(
            lits(sh, r#"echo "$(basename "$f")" "$(a "$(b 'c)' \")") x" "a""#),
            [
                r#""$(basename "$f")""#,
                r#""$(a "$(b 'c)' \")") x""#,
                "\"a\""
            ]
        );
        // An unclosed substitution keeps the plain rule.
        assert_eq!(lits(sh, "\"$(open \"q\"")[0], "\"$(open \"");
        // No substitution: the plain rule (escaped quotes included).
        assert_eq!(lits(sh, r#"a "x \" y" b"#), [r#""x \" y""#]);
        // After one unterminated scan, later strings use the plain rule.
        assert_eq!(
            lits(sh, "\"$(a\n\"$(b \"c\")\""),
            ["\"$(a\n\"", "\"c\"", "\""]
        );
    }

    /// Unterminated nested strings must not make lexing quadratic (#150
    /// review: 40k such lines took seconds). Generous bound for debug CI.
    #[test]
    fn unterminated_nesting_stays_linear() {
        for (opts, line) in [
            (TokenizerOptions::SHELL, "echo \"$( x \" y\n"),
            (TokenizerOptions::R, "x <- r\"( y\n"),
        ] {
            let src = line.repeat(40_000);
            let t = std::time::Instant::now();
            let toks = tokenize_with(&src, opts);
            assert!(!toks.is_empty());
            assert!(
                t.elapsed() < std::time::Duration::from_secs(3),
                "{line:?}: {:?}",
                t.elapsed()
            );
        }
    }

    #[test]
    fn default_const_matches_default() {
        assert_eq!(TokenizerOptions::DEFAULT, TokenizerOptions::default());
        assert_eq!(TokenizerOptions::C, TokenizerOptions::default());
    }

    #[test]
    fn hash_comments_dialect() {
        use TokenClass::*;
        let t = dtoks("hash_comments", "a # c\nb");
        assert_eq!(t[1], ("# c".to_string(), Comment));
        // Line-start-only: ARM immediates stay code.
        let t = dtexts("hash_comments_line_start_only", "mov r0, #1\n  # c\n");
        assert_eq!(t, ["mov", "r0", ",", "#", "1", "# c"]);
        // Shell: only at a word start.
        let t = dtoks("SHELL", "a#b ${#x} $# x;# c");
        assert_eq!(t[0].0, "a");
        assert_eq!(t[1].0, "#");
        assert_eq!(t[3], ("${#x}".to_string(), Identifier));
        assert_eq!(t[4], ("$#".to_string(), Identifier));
        assert_eq!(t.last().unwrap(), &("# c".to_string(), Comment));
        // Off by default.
        assert_eq!(texts("# c"), ["#", "c"]);
    }

    #[test]
    fn dash_and_nested_comments() {
        use TokenClass::*;
        assert_eq!(dtoks("SQL", "a -- c\nb")[1], ("-- c".to_string(), Comment));
        let t = dtexts("HASKELL", "{- a {- b -} c -} x -- d\ny {- open");
        assert_eq!(t, ["{- a {- b -} c -}", "x", "-- d", "y", "{- open"]);
        let t = dtexts("FSHARP", "(* a (* b *) *) x (*) y // z");
        assert_eq!(t, ["(* a (* b *) *)", "x", "(", "*", ")", "y", "// z"]);
        assert_eq!(dtoks("FSHARP", "(*)")[0].1, Punctuation);
        // Default: all plain.
        assert_eq!(texts("--"), ["-", "-"]);
    }

    #[test]
    fn semicolon_comments_and_asm() {
        let t = dtexts("ASM", "mov eax, 'a;b' ; load\n# 1 \"f\"\nldr r0, #4 // x");
        assert_eq!(
            t,
            [
                "mov",
                "eax",
                ",",
                "'a;b'",
                "; load",
                "# 1 \"f\"",
                "ldr",
                "r0",
                ",",
                "#",
                "4",
                "// x"
            ]
        );
    }

    #[test]
    fn slash_comments_can_be_disabled() {
        let t = dtexts("PYTHON", "a // b /* c */ # d");
        assert_eq!(t, ["a", "/", "/", "b", "/", "*", "c", "*", "/", "# d"]);
        let t = dtexts("SQL", "a // b /* c */");
        assert_eq!(t, ["a", "/", "/", "b", "/* c */"]);
    }

    #[test]
    fn triple_quote_and_raw_backtick_strings() {
        use TokenClass::*;
        let t = dtoks("PYTHON", "x = \"\"\"a\n\\\"\"\" b\"\"\" + '''c''' + 'd'");
        assert_eq!(t[2], ("\"\"\"a\n\\\"\"\" b\"\"\"".to_string(), Literal));
        assert_eq!(t[4], ("'''c'''".to_string(), Literal));
        assert_eq!(t[6], ("'d'".to_string(), Literal));
        assert_eq!(dtexts("PYTHON", "'''open\nx"), ["'''open\nx"]);
        // `""` is still an empty string.
        assert_eq!(dtexts("PYTHON", "\"\" x"), ["\"\"", "x"]);
        let t = dtoks("GO", "s := `a\\` + `b\nc`");
        assert_eq!(t[3], ("`a\\`".to_string(), Literal));
        assert_eq!(t[5], ("`b\nc`".to_string(), Literal));
    }

    #[test]
    fn sql_strings_dialect() {
        use TokenClass::*;
        let t = dtoks("SQL", "'it''s' \"a\"\"b\" [my col] a[1] 'x\\' y");
        assert_eq!(t[0], ("'it''s'".to_string(), Literal));
        assert_eq!(t[1], ("\"a\"\"b\"".to_string(), Identifier));
        assert_eq!(t[2], ("[my col]".to_string(), Identifier));
        assert_eq!(t[4].0, "[");
        assert_eq!(t[7], ("'x\\'".to_string(), Literal));
        assert_eq!(dtexts("SQL", "'a\nb'"), ["'a\nb'"]);
    }

    #[test]
    fn shell_words_dialect() {
        use TokenClass::*;
        let t = dtoks("SHELL", "echo $HOME ${a:-${b}} $1 $(ls) '$x\\' 'o");
        assert_eq!(t[1], ("$HOME".to_string(), Identifier));
        assert_eq!(t[2], ("${a:-${b}}".to_string(), Identifier));
        assert_eq!(t[3], ("$1".to_string(), Identifier));
        assert_eq!(t[4].0, "$");
        assert_eq!(t[5].0, "(");
        assert_eq!(t[8], ("'$x\\'".to_string(), Literal));
        assert_eq!(t[9], ("'o".to_string(), Literal));
        // Heredocs: marker is one token, body one literal.
        let src = "cat <<-'EOF' | wc\n\tline $x\n\tEOF\necho done";
        let t = dtoks("SHELL", src);
        assert_eq!(t[1], ("<<-'EOF'".to_string(), Operator));
        assert_eq!(t[4], ("line $x\n\tEOF".to_string(), Literal));
        assert_eq!(t[5].0, "echo");
        // Two heredocs on one line, then an unterminated one.
        let t = dtexts("SHELL", "a <<X <<\"Y\"\n1\nX\n2\nY\nb <<Z\nopen");
        assert_eq!(
            t,
            ["a", "<<X", "<<\"Y\"", "1\nX", "2\nY", "b", "<<Z", "open"]
        );
        // Here-strings and arithmetic shifts are not heredocs.
        assert_eq!(dtexts("SHELL", "a <<< s")[1..4], ["<", "<", "<"]);
        assert_eq!(dtexts("SHELL", "$((1<<2))")[4..7], ["<", "<", "2"]);
    }

    #[test]
    fn shell_arithmetic_and_ansi_c() {
        use TokenClass::*;
        // `<<` inside arithmetic is a shift, with or without spaces.
        let t = dtexts("SHELL", "echo $((a << b)) ((x<<=1))\ny");
        assert!(!t.iter().any(|s| s.starts_with("<<")), "{t:?}");
        assert_eq!(t.last().unwrap(), "y");
        // After the arithmetic closes, heredocs work again.
        let t = dtexts("SHELL", "$(( (1) )) cat <<E\nb\nE");
        assert_eq!(t[t.len() - 2..], ["<<E", "b\nE"]);
        // `$(` command substitution is not arithmetic.
        let t = dtexts("SHELL", "x=$(cat <<E\nb\nE\n)");
        assert!(t.contains(&"b\nE".to_string()), "{t:?}");
        // ANSI-C quoting with backslash escapes.
        let t = dtoks("SHELL", r"echo $'it\'s' x");
        assert_eq!(t[1], (r"$'it\'s'".to_string(), Literal));
        assert_eq!(t[2].0, "x");
        // Escaped heredoc delimiter.
        let t = dtoks("SHELL", "cat <<\\EOF\n$x\nEOF\nz");
        assert_eq!(t[1], ("<<\\EOF".to_string(), Operator));
        assert_eq!(t[2], ("$x\nEOF".to_string(), Literal));
        assert_eq!(t[3].0, "z");
    }

    #[test]
    fn raw_triple_quotes() {
        use TokenClass::*;
        let src = "\"\"\"a\\\"\"\" b";
        // Raw (Scala, F#): the backslash does not escape.
        for d in ["SCALA", "FSHARP"] {
            let t = dtoks(d, src);
            assert_eq!(t[0], ("\"\"\"a\\\"\"\"".to_string(), Literal), "{d}");
            assert_eq!(t[1].0, "b", "{d}");
        }
        // Escaping (Python, Java, Elixir, GDScript).
        for d in ["PYTHON", "JAVA", "ELIXIR"] {
            assert_eq!(dtoks(d, src).len(), 1, "{d}");
        }
        assert_eq!(TokenizerOptions::GDSCRIPT, TokenizerOptions::PYTHON);
    }

    #[test]
    fn doubled_single_quotes_dialect() {
        use TokenClass::*;
        for d in ["ASM", "doubled_single_quotes"] {
            let t = dtoks(d, r"'it''s' 'a\' 'open");
            assert_eq!(t[0], ("'it''s'".to_string(), Literal), "{d}");
            assert_eq!(t[1], (r"'a\'".to_string(), Literal), "{d}");
            assert_eq!(t[2], ("'open".to_string(), Literal), "{d}");
        }
        // One line only.
        assert_eq!(dtexts("ASM", "'a\nb'"), ["'a", "b", "'"]);
        let rpg = dtoks("RPG", "**FREE\nx = 'it''s';");
        assert!(rpg.contains(&("'it''s'".to_string(), Literal)));
        let cob = dtoks("COBOL", "       DISPLAY 'it''s'.");
        assert!(cob.contains(&("'it''s'".to_string(), Literal)));
    }

    #[test]
    fn haskell_dash_rule() {
        use TokenClass::*;
        let t = dtoks("HASKELL", "a --> b |-- c --| d --- e\nf -- g");
        let texts: Vec<&str> = t.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(
            texts,
            [
                "a", "-", "-", ">", "b", "|", "-", "-", "c", "-", "-", "|", "d", "--- e", "f",
                "-- g"
            ]
        );
        assert_eq!(t[13].1, Comment);
        // SQL is unchanged: any `--` is a comment.
        assert_eq!(dtoks("SQL", "a -->b")[1], ("-->b".to_string(), Comment));
    }

    #[test]
    fn prime_identifiers() {
        use TokenClass::*;
        for d in ["HASKELL", "FSHARP"] {
            let t = dtoks(d, "f' 'a' x' = y'");
            assert_eq!(
                t,
                [
                    ("f'".to_string(), Identifier),
                    ("'a'".to_string(), Literal),
                    ("x'".to_string(), Identifier),
                    ("=".to_string(), Operator),
                    ("y'".to_string(), Identifier),
                ],
                "{d}"
            );
            assert_eq!(dtexts(d, "foldl' f 'a'"), ["foldl'", "f", "'a'"], "{d}");
        }
        // Default: the prime is separate.
        assert_eq!(texts("x' "), ["x", "'"]);
    }

    #[test]
    fn sql_bracket_rejects_nested() {
        use TokenClass::*;
        let t = dtoks("SQL", "[a[b] [ok]");
        assert_eq!(t[0], ("[".to_string(), Punctuation));
        assert!(t.contains(&("[ok]".to_string(), Identifier)));
    }

    #[test]
    fn cobol_columns_after_bom() {
        use TokenClass::*;
        let t = dtoks("COBOL", "\u{feff}000100* comment\n000200 MOVE A TO B.");
        assert_eq!(t[0], ("000100".to_string(), Comment));
        assert_eq!(t[0].1, Comment);
        assert_eq!(t[1], ("* comment".to_string(), Comment));
        assert_eq!(t[3], ("MOVE".to_string(), Identifier));
        assert_eq!(t[3].0, "MOVE");
    }

    #[test]
    fn hyphen_idents_dialect() {
        assert_eq!(
            dtexts("hyphen_idents", "WORKING-STORAGE a-1 b - c d-"),
            ["WORKING-STORAGE", "a-1", "b", "-", "c", "d", "-"]
        );
    }

    #[test]
    fn cobol_fixed_columns() {
        use TokenClass::*;
        let src = "000100 IDENTIFICATION DIVISION.                                         SEQ00001\n000200* comment line\n000300 01 WS-COUNT PIC 9. *> inline\n      - 'cont'\n      /page\r\nab\n";
        let t = dtoks("COBOL", src);
        let c = |s: &str| (s.to_string(), Comment);
        assert_eq!(t[0], c("000100"));
        assert_eq!(t[1], ("IDENTIFICATION".to_string(), Identifier));
        assert_eq!(t[4], c("SEQ00001"));
        assert_eq!(t[5], c("000200"));
        assert_eq!(t[6], c("* comment line"));
        assert!(t.contains(&("WS-COUNT".to_string(), Identifier)));
        assert!(t.contains(&c("*> inline")));
        assert!(t.contains(&("-".to_string(), Operator)));
        assert!(t.contains(&("'cont'".to_string(), Literal)));
        assert!(t.contains(&c("/page")));
        // A short line is all sequence area.
        assert_eq!(t.last().unwrap(), &c("ab"));
    }

    #[test]
    fn rpg_fixed_and_free() {
        use TokenClass::*;
        let src = "     H DFTACTGRP(*NO)\n     DNAME             S             10A\n     C* comment\n      dcl-proc foo; // c\n";
        let t = dtoks("RPG", src);
        assert_eq!(t[0], ("H".to_string(), Identifier));
        assert_eq!(t[1].0, "DFTACTGRP");
        assert!(t.contains(&("D".to_string(), Identifier)));
        assert!(t.contains(&("NAME".to_string(), Identifier)));
        assert!(t.contains(&("* comment".to_string(), Comment)));
        assert!(t.contains(&("dcl-proc".to_string(), Identifier)));
        assert!(t.contains(&("// c".to_string(), Comment)));
        // Columns 81+ are a comment.
        let line = format!("{:<80}trailing", "     C                   EVAL      X = 1");
        let t = dtoks("RPG", &line);
        assert_eq!(t.last().unwrap(), &("trailing".to_string(), Comment));
        // `**FREE`: no columns at all.
        let t = dtexts("RPG", "**FREE\n     C* x\n");
        assert_eq!(t, ["*", "*", "FREE", "C", "*", "x"]);
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

    const RUST: TokenizerOptions = TokenizerOptions::RUST;

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
        fn spans_match_source_lang_heavy(
            src in "([ \\t\\n]|\\r\\n|#|-|;|\\{|\\}|\\(|\\)|\\*|>|<|\\$|'|\"|`|\\[|\\]|/|@|x|EOF|é|\\\\|\\*\\*FREE\\n|      |r|rb|f|\\*\\*CTDATA\\n|\\*\\* \\n|\\$\\(){0,60}"
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
        for (name, _) in DIALECT_GOLDENS
            .iter()
            .chain(LANG_GOLDENS)
            .chain(FOLLOWUP_GOLDENS)
        {
            check_spans_with(src, dialect(name))?;
        }
        // Every flag at once, with each layout.
        for fixed in [None, Some(FixedLayout::Cobol), Some(FixedLayout::Rpg)] {
            check_spans_with(src, all_flags(fixed))?;
            check_spans_with(
                src,
                TokenizerOptions {
                    triple_quote_raw: true,
                    doubled_single_quotes: true,
                    ..all_flags(fixed)
                },
            )?;
        }
        Ok(())
    }

    fn all_flags(fixed_columns: Option<FixedLayout>) -> TokenizerOptions {
        TokenizerOptions {
            rust_literals: true,
            single_quote_strings: true,
            csharp_strings: true,
            markup: true,
            aspx: true,
            regex_literals: true,
            no_line_slash_comments: false,
            no_block_slash_comments: false,
            hash_comments: true,
            hash_comments_line_start_only: false,
            dash_comments: true,
            haskell_block_comments: true,
            ml_block_comments: true,
            semicolon_comments: true,
            triple_quote_strings: true,
            triple_quote_raw: false,
            doubled_single_quotes: false,
            prime_idents: true,
            raw_backtick_strings: true,
            sql_strings: true,
            shell_words: true,
            hyphen_idents: true,
            fixed_columns,
            python_string_prefixes: true,
            r_raw_strings: true,
            sql_hash_comments: true,
            rpg_compile_time_data: true,
        }
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
