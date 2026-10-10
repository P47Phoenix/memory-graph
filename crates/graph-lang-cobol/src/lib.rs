//! COBOL extractor: a sentence-oriented token-stream scanner, not a parser.
//!
//! Tokens come from the shared tokenizer's `COBOL` dialect (reference
//! format: sequence area, indicator column, Area A/B, identification area).
//! A file whose first non-blank line is a free-format directive
//! (`>>SOURCE FORMAT FREE`, `>>SOURCE FREE`, `$SET SOURCEFORMAT"FREE"`), or
//! that has code in the sequence area which fixed format cannot have (two
//! words in columns 1-6, or a word running from column 6 into column 7, as
//! in GnuCOBOL `-free` sources without a directive), is tokenized free-form
//! instead, with `*>` floating comments. Directive lines (`>>...`,
//! `$SET ...`) are Comment tokens in both formats. A file with no
//! `DIVISION` header (a copybook) is scanned for data items and paragraphs.
//!
//! | COBOL | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `PROGRAM-ID. name.` (the whole program) | Module | `program` |
//! | `name DIVISION.` | Other | `division` |
//! | `name SECTION.` | Other | `section` |
//! | paragraph `name.` in the PROCEDURE DIVISION | Function | `paragraph` |
//! | `01` / `77` data items (with their subordinate items) | Variable | `level-01` / `level-77` |
//!
//! Spans: a program runs from its `IDENTIFICATION DIVISION` header (or its
//! `PROGRAM-ID` when the header is omitted) through its `END PROGRAM name.`,
//! or to the end of the file. A division, section, paragraph or data item
//! runs to the last code token before the next header of the same or a
//! higher rank (program > division > section > paragraph / data item; `FD`,
//! `SD` and `RD` entries also end a data item). Nested programs nest; a
//! program that never gets an `END PROGRAM` ends at the next
//! `IDENTIFICATION DIVISION` (siblings). Empty names are not symbols.
//!
//! Paragraph names are recognised at the start of a sentence (after a `.`)
//! when followed by `.`: in fixed format only in Area A (columns 8-11); in
//! free format when first on their line and not a lone verb (`EXIT.`,
//! `GOBACK.`, `END-IF.`). Known limits: a picture string with an inner
//! period (`PIC ZZ9.99`) is read as a sentence end, which is harmless unless
//! a level number `01`/`77` follows it; `COPY` members are not expanded; a
//! `>>SOURCE FORMAT` switch after the first line is not followed; unnamed
//! and `FILLER` level-01 items are not symbols (but still end the previous
//! item).
use graph_core::scan::{mark_keywords_ignore_case, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, Span, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct CobolExtractor;

/// Common COBOL reserved words (division and section headers, verbs and
/// their scope terminators, clauses), upper case; matched
/// case-insensitively. Reserved words cannot name data items or
/// paragraphs; this is a conservative subset of the standard's list.
const KEYWORDS: &[&str] = &[
    "ACCEPT",
    "ADD",
    "ALSO",
    "AND",
    "ARE",
    "ASSIGN",
    "AT",
    "BY",
    "CALL",
    "CLOSE",
    "COMPUTE",
    "CONFIGURATION",
    "CONTINUE",
    "COPY",
    "DATA",
    "DELETE",
    "DISPLAY",
    "DIVIDE",
    "DIVISION",
    "ELSE",
    "END",
    "END-CALL",
    "END-COMPUTE",
    "END-EVALUATE",
    "END-IF",
    "END-PERFORM",
    "END-READ",
    "END-SEARCH",
    "END-STRING",
    "END-WRITE",
    "ENVIRONMENT",
    "EVALUATE",
    "EXIT",
    "FD",
    "FILE",
    "FILE-CONTROL",
    "FROM",
    "GIVING",
    "GO",
    "GOBACK",
    "IDENTIFICATION",
    "IF",
    "INITIALIZE",
    "INPUT",
    "INPUT-OUTPUT",
    "INSPECT",
    "INTO",
    "IS",
    "LINKAGE",
    "MOVE",
    "MULTIPLY",
    "NOT",
    "OCCURS",
    "OF",
    "OPEN",
    "OR",
    "OTHER",
    "OUTPUT",
    "PERFORM",
    "PIC",
    "PICTURE",
    "PROCEDURE",
    "PROGRAM",
    "PROGRAM-ID",
    "READ",
    "REDEFINES",
    "RETURN",
    "REWRITE",
    "SEARCH",
    "SECTION",
    "SELECT",
    "SET",
    "STOP",
    "STRING",
    "SUBTRACT",
    "THEN",
    "THROUGH",
    "THRU",
    "TIMES",
    "TO",
    "UNSTRING",
    "UNTIL",
    "USING",
    "VALUE",
    "VARYING",
    "WHEN",
    "WITH",
    "WORKING-STORAGE",
    "WRITE",
];

/// Tokenizer dialect for fixed (reference) format COBOL.
pub const COBOL_TOKENIZER: TokenizerOptions = TokenizerOptions::COBOL;

/// Tokenizer dialect for free-format COBOL (`*>` comments are merged by
/// [`tokenize`], since the tokenizer only knows them in fixed format).
pub const COBOL_FREE_TOKENIZER: TokenizerOptions = TokenizerOptions {
    fixed_columns: None,
    ..TokenizerOptions::COBOL
};

impl Extractor for CobolExtractor {
    fn language(&self) -> &str {
        "cobol"
    }

    fn extensions(&self) -> &[&str] {
        &["cbl", "cob", "cpy"]
    }

    fn version(&self) -> String {
        // `kw1`: reserved words are classed `keyword` (#143).
        // `dw1`: digit-led words (`1000-READ-NEXT`) are one identifier (#196).
        // `dw2`: a lone digit-led paragraph header (`100A.`) is too (#209).
        // `dw3`: and a digit-led section header (`100A SECTION.`, #268).
        format!("cobol-scan-1+kw1+dw3+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let free = is_free_format(source);
        let mut tokens = tokenize(source, free);
        let symbols = symbols(&tokens, free);
        // After the symbol scan, which reads identifiers as it always has.
        // Reserved words cannot be user names, and hyphenated names
        // (`END-OF-FILE`) are whole tokens, so nothing is escaped.
        mark_keywords_ignore_case(&mut tokens, KEYWORDS, |_, _| false);
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Free format: the first non-blank line is a free-format directive, or
/// some line has code in the sequence area (columns 1-6) that fixed format
/// cannot have (GnuCOBOL `-free` sources often carry no directive).
pub fn is_free_format(src: &str) -> bool {
    let s = src.strip_prefix('\u{feff}').unwrap_or(src);
    let Some(line) = s.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return false;
    };
    let u = line.to_ascii_uppercase();
    (u.starts_with(">>SOURCE") && u.contains("FREE"))
        || (u.starts_with("$SET") && u.contains("SOURCEFORMAT") && u.contains("FREE"))
        || s.lines().any(code_in_sequence_area)
}

/// Columns 1-6 hold two words (`move 3`), or a word runs from column 6
/// into column 7 (`identification`). Sequence numbers are one run of
/// characters, and column 7 is an indicator (blank, `*`, `/`, `-`, `D`) in
/// fixed format, so neither happens there.
fn code_in_sequence_area(line: &str) -> bool {
    let cs: Vec<char> = line.chars().take(7).collect();
    if cs.len() < 7
        || cs[..6].contains(&'\t')
        || cs[..6].iter().all(|c| c.is_ascii_digit() || *c == ' ')
    {
        return false;
    }
    let seq = &cs[..6];
    let two_words = seq
        .iter()
        .skip_while(|c| **c == ' ')
        .skip_while(|c| **c != ' ')
        .find(|c| **c != ' ')
        .is_some();
    let word = |c: char| c.is_alphanumeric() || c == '-';
    let crosses = !matches!(cs[6], 'D' | 'd' | '-' | '*' | '/')
        && (cs[5].is_alphabetic() || cs[5] == '-')
        && word(cs[6])
        && seq.iter().any(|c| c.is_alphabetic());
    two_words || crosses
}

/// Tokens for COBOL source, fixed or free format.
pub fn tokenize(src: &str, free: bool) -> Vec<TokenDecl> {
    let raw = if free {
        tokenize_with(src, COBOL_FREE_TOKENIZER)
    } else {
        tokenize_with(src, COBOL_TOKENIZER)
    };
    let mut out: Vec<TokenDecl> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let t = &raw[i];
        let adjacent = |text: &str| {
            raw.get(i + 1).is_some_and(|n| {
                n.span.start == t.span.end && n.text.to_ascii_uppercase().starts_with(text)
            })
        };
        let code = matches!(t.class, TokenClass::Operator | TokenClass::Punctuation);
        // `*>` floating comment (free format; fixed format lexes it already).
        let floating =
            free && code && (t.text.starts_with("*>") || (t.text == "*" && adjacent(">")));
        // A compiler directive line (`>>SOURCE ...`, `$SET ...`) is a comment.
        let line_start = out
            .last()
            .is_none_or(|p| p.span.end_line < t.span.start_line);
        let directive = line_start
            && code
            && (t.text.starts_with(">>")
                || (t.text == ">" && adjacent(">"))
                || (t.text == "$" && adjacent("SET")));
        if floating || directive {
            let line = t.span.start_line;
            let mut j = i;
            while raw
                .get(j + 1)
                .is_some_and(|n| n.span.start_line == line && n.class != TokenClass::Comment)
            {
                j += 1;
            }
            let span = span_between(&t.span, &raw[j].span);
            out.push(TokenDecl {
                text: src[span.start as usize..span.end as usize].to_string(),
                class: TokenClass::Comment,
                span,
            });
            i = j + 1;
        } else {
            out.push(t.clone());
            i += 1;
        }
    }
    let mut out = join_digit_led_words(src, out);
    reclass_digit_led_headers(&mut out, free);
    out
}

/// A lone digit-led word with a letter (`100A`, `9X`) lexes as a number
/// literal. It is a user-defined word only where nothing but a word can
/// stand: a paragraph header, i.e. a sentence start in Area A (columns
/// 8-11 in fixed format, first on its line in free format) followed by `.`,
/// or a section header, followed by `SECTION` and `.`.
/// Exponent literals (`1E5`, `1.5E3`) are never reclassed, and a PIC
/// string or level-number operand is never at a sentence start.
fn reclass_digit_led_headers(tokens: &mut [TokenDecl], free: bool) {
    let code: Vec<usize> = (0..tokens.len())
        .filter(|&i| tokens[i].class != TokenClass::Comment)
        .collect();
    for (k, &i) in code.iter().enumerate() {
        let t = &tokens[i];
        let candidate = t.class == TokenClass::Literal
            && t.text.starts_with(|c: char| c.is_ascii_digit())
            && t.text.bytes().all(|b| b.is_ascii_alphanumeric())
            && t.text.bytes().any(|b| b.is_ascii_alphabetic())
            && !is_exponent_literal(&t.text);
        if !candidate {
            continue;
        }
        let prev = k.checked_sub(1).map(|p| &tokens[code[p]]);
        let sentence_start =
            prev.is_none_or(|p| p.text == "." && p.class == TokenClass::Punctuation);
        let is_period = |at: usize| {
            code.get(at).is_some_and(|&n| {
                tokens[n].text == "." && tokens[n].class == TokenClass::Punctuation
            })
        };
        // `100A.` (paragraph) or `100A SECTION [50].` (section, #268).
        let next_is_period = is_period(k + 1)
            || (code
                .get(k + 1)
                .is_some_and(|&n| tokens[n].text.eq_ignore_ascii_case("SECTION"))
                && (is_period(k + 2)
                    || (code.get(k + 2).is_some_and(|&n| {
                        tokens[n].class == TokenClass::Literal
                            && !tokens[n].text.is_empty()
                            && tokens[n].text.bytes().all(|b| b.is_ascii_digit())
                    }) && is_period(k + 3))));
        let area_a = if free {
            prev.is_none_or(|p| p.span.end_line < t.span.start_line)
        } else {
            (8..=11).contains(&t.span.start_col)
        };
        if sentence_start && next_is_period && area_a {
            tokens[i].class = TokenClass::Identifier;
        }
    }
}

/// `1E5`, `12E+3`: digits, `E`, digits (a sign or fraction would not lex
/// into one alphanumeric token, but digits-E-digits does).
fn is_exponent_literal(text: &str) -> bool {
    let u = text.to_ascii_uppercase();
    match u.split_once('E') {
        Some((m, e)) => {
            !m.is_empty()
                && m.bytes().all(|b| b.is_ascii_digit())
                && e.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Joins a user-defined word that starts with a digit (`1000-READ-NEXT`,
/// `9A-X`) into one `Identifier`. The shared tokenizer lexes it as a number,
/// `-` and an identifier; COBOL makes it one word as long as it has a letter
/// and the pieces touch (arithmetic operators need spaces around them).
/// A word split across a continuation line is not joined.
fn join_digit_led_words(src: &str, tokens: Vec<TokenDecl>) -> Vec<TokenDecl> {
    let mut out: Vec<TokenDecl> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let end = digit_led_word_end(&tokens, i);
        if end == i {
            out.push(tokens[i].clone());
        } else {
            let span = span_between(&tokens[i].span, &tokens[end].span);
            out.push(TokenDecl {
                text: src[span.start as usize..span.end as usize].to_string(),
                class: TokenClass::Identifier,
                span,
            });
        }
        i = end + 1;
    }
    out
}

/// Index of the last token of a digit-led word starting at `start`, or
/// `start` itself when there is none to join.
fn digit_led_word_end(tokens: &[TokenDecl], start: usize) -> usize {
    let is_word_piece = |t: &TokenDecl| {
        matches!(t.class, TokenClass::Literal | TokenClass::Identifier)
            && t.text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let first = &tokens[start];
    let digit_led = first.class == TokenClass::Literal
        && first.text.starts_with(|c: char| c.is_ascii_digit())
        && is_word_piece(first);
    if !digit_led {
        return start;
    }
    let mut end = start;
    let mut has_letter = first.text.bytes().any(|b| b.is_ascii_alphabetic());
    // Each step consumes a touching `-` and the touching piece after it.
    while let (Some(dash), Some(piece)) = (tokens.get(end + 1), tokens.get(end + 2)) {
        let touching = dash.span.start == tokens[end].span.end && piece.span.start == dash.span.end;
        if !(touching && dash.text == "-" && is_word_piece(piece)) {
            break;
        }
        has_letter |= piece.text.bytes().any(|b| b.is_ascii_alphabetic());
        end += 2;
    }
    if has_letter {
        end
    } else {
        start
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    /// `IDENTIFICATION DIVISION` or a bare `PROGRAM-ID`; the name is filled
    /// in by the `PROGRAM-ID` paragraph.
    ProgStart(Option<String>),
    ProgEnd(String),
    Division(String),
    Section(String),
    Paragraph(String),
    /// A level-01/77 item; `None` for FILLER/unnamed and FD/SD/RD entries,
    /// which only end the previous item.
    Data(Option<String>, &'static str),
}

impl Ev {
    fn rank(&self) -> u8 {
        match self {
            Ev::ProgStart(_) | Ev::ProgEnd(_) => 0,
            Ev::Division(_) => 1,
            Ev::Section(_) => 2,
            Ev::Paragraph(_) | Ev::Data(..) => 3,
        }
    }
}

struct E {
    ev: Ev,
    /// Code positions (indices into the code-token list).
    first: usize,
    last: usize,
}

/// Lone words that end a sentence but are not paragraph names.
const VERBS: &[&str] = &["EXIT", "GOBACK", "CONTINUE", "ELSE", "STOP", "RUN"];

/// Symbols in COBOL tokens (as produced by [`tokenize`]).
pub fn symbols(tokens: &[TokenDecl], free: bool) -> Vec<SymbolDecl> {
    let code: Vec<&TokenDecl> = tokens
        .iter()
        .filter(|t| t.class != TokenClass::Comment)
        .collect();
    let evs = events(&code, free);
    let span = |a: usize, b: usize| span_between(&code[a].span, &code[b].span);
    let mut out = Vec::new();
    let sym = |name: &str, kind, lk: &str, span: Span| SymbolDecl {
        owner: None,
        name: name.to_string(),
        kind,
        lang_kind: Some(lk.to_string()),
        span,
    };
    // End (code position) of a rank-`r` symbol opened at event `i`.
    let end_of = |i: usize, r: u8| -> usize {
        evs[i + 1..]
            .iter()
            .find(|x| x.ev.rank() <= r)
            .map_or(code.len() - 1, |x| x.first.saturating_sub(1))
            .max(evs[i].last)
    };
    let end_names: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.ev {
            Ev::ProgEnd(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    let mut progs: Vec<(Option<&str>, usize)> = Vec::new();
    let close = |out: &mut Vec<SymbolDecl>, p: (Option<&str>, usize), end: usize| {
        if let Some(n) = p.0 {
            out.push(sym(n, SymbolKind::Module, "program", span(p.1, end)));
        }
    };
    for (i, e) in evs.iter().enumerate() {
        match &e.ev {
            Ev::ProgStart(n) => {
                // An open program that never gets an END PROGRAM ends here:
                // only programs closed by END PROGRAM can contain others.
                while let Some(&p) = progs.last() {
                    let ended =
                        p.0.is_some_and(|p| end_names.iter().any(|n| n.eq_ignore_ascii_case(p)));
                    if ended {
                        break;
                    }
                    progs.pop();
                    close(&mut out, p, e.first.saturating_sub(1).max(p.1));
                }
                progs.push((n.as_deref(), e.first));
            }
            Ev::ProgEnd(n) => {
                let hit = progs
                    .iter()
                    .rposition(|p| p.0.is_some_and(|p| p.eq_ignore_ascii_case(n)))
                    .unwrap_or(progs.len().saturating_sub(1));
                if hit < progs.len() {
                    for p in progs.drain(hit + 1..).collect::<Vec<_>>() {
                        close(&mut out, p, e.first.saturating_sub(1).max(p.1));
                    }
                    let p = progs.pop().unwrap();
                    close(&mut out, p, e.last);
                }
            }
            Ev::Division(n) => {
                let s = span(e.first, end_of(i, 1));
                out.push(sym(n, SymbolKind::Other, "division", s));
            }
            Ev::Section(n) => {
                let s = span(e.first, end_of(i, 2));
                out.push(sym(n, SymbolKind::Other, "section", s));
            }
            Ev::Paragraph(n) => {
                let s = span(e.first, end_of(i, 3));
                out.push(sym(n, SymbolKind::Function, "paragraph", s));
            }
            Ev::Data(Some(n), lk) => {
                let s = span(e.first, end_of(i, 3));
                out.push(sym(n, SymbolKind::Variable, lk, s));
            }
            Ev::Data(None, _) => {}
        }
    }
    let last = code.len().saturating_sub(1);
    for p in std::mem::take(&mut progs) {
        close(&mut out, p, last.max(p.1));
    }
    no_partial_overlap(out)
}

fn events(code: &[&TokenDecl], free: bool) -> Vec<E> {
    let mut out: Vec<E> = Vec::new();
    let text = |c: usize| code.get(c).map_or("", |t| t.text.as_str());
    let is = |c: usize, s: &str| text(c).eq_ignore_ascii_case(s);
    let is_word = |c: usize| {
        code.get(c)
            .is_some_and(|t| t.class == TokenClass::Identifier)
    };
    let period = |c: usize| {
        code.get(c)
            .is_some_and(|t| t.text == "." && t.class == TokenClass::Punctuation)
    };
    // Code position of the period ending the sentence from `c` (or the last token).
    let sentence_end = |c: usize| {
        (c..code.len())
            .find(|&j| period(j))
            .unwrap_or(code.len() - 1)
    };
    // A copybook (no DIVISION header anywhere) may hold data items or
    // paragraphs.
    let copybook = !(1..code.len()).any(|c| is(c, "DIVISION") && is_word(c - 1));
    let mut in_procedure = copybook;
    let mut in_data = copybook;
    // An `IDENTIFICATION DIVISION` awaiting its PROGRAM-ID.
    let mut pending_id: Option<usize> = None;
    let mut c = 0;
    while c < code.len() {
        let start = c == 0 || period(c - 1);
        if !start {
            c += 1;
            continue;
        }
        let end = sentence_end(c);
        let push = |out: &mut Vec<E>, ev: Ev| {
            out.push(E {
                ev,
                first: c,
                last: end,
            })
        };
        if is_word(c) && is(c + 1, "DIVISION") {
            let name = text(c).to_string();
            let upper = name.to_ascii_uppercase();
            if upper == "IDENTIFICATION" || upper == "ID" {
                pending_id = Some(out.len());
                push(&mut out, Ev::ProgStart(None));
            }
            in_procedure = upper == "PROCEDURE";
            in_data = upper == "DATA";
            push(&mut out, Ev::Division(name));
        } else if is(c, "PROGRAM-ID") {
            // PROGRAM-ID. name [IS ... PROGRAM].
            let mut n = c + 1;
            if period(n) {
                n += 1;
            }
            let name = code
                .get(n)
                .filter(|t| !period(n) && t.class != TokenClass::Punctuation);
            let name = name
                .map(|t| t.text.trim_matches(['\'', '"']).to_string())
                .filter(|n| !n.is_empty());
            if let Some(name) = name {
                match pending_id.take() {
                    Some(p) => out[p].ev = Ev::ProgStart(Some(name)),
                    None => {
                        in_procedure = false;
                        push(&mut out, Ev::ProgStart(Some(name)));
                    }
                }
            }
            // The PROGRAM-ID paragraph is two sentences; skip past the name.
            c = n.max(c) + 1;
            continue;
        } else if is(c, "END") && is(c + 1, "PROGRAM") {
            let name = code
                .get(c + 2)
                .filter(|_| !period(c + 2))
                .map_or(String::new(), |t| {
                    t.text.trim_matches(['\'', '"']).to_string()
                });
            in_procedure = false;
            pending_id = None;
            push(&mut out, Ev::ProgEnd(name));
        } else if is_word(c) && is(c + 1, "SECTION") {
            push(&mut out, Ev::Section(text(c).to_string()));
        } else if in_procedure && is_word(c) && period(c + 1) && paragraph_ok(code, c, free) {
            push(&mut out, Ev::Paragraph(text(c).to_string()));
        } else if in_data {
            let t = code[c];
            let level = match t.text.trim_start_matches('0') {
                "1" if t.class == TokenClass::Literal => Some("level-01"),
                "77" if t.class == TokenClass::Literal => Some("level-77"),
                _ => None,
            };
            if let Some(lk) = level {
                let name = code
                    .get(c + 1)
                    .filter(|n| n.class == TokenClass::Identifier)
                    .filter(|n| !n.text.eq_ignore_ascii_case("FILLER"))
                    .filter(|n| {
                        !matches!(
                            n.text.to_ascii_uppercase().as_str(),
                            "PIC" | "PICTURE" | "VALUE" | "REDEFINES" | "USAGE"
                        )
                    });
                push(&mut out, Ev::Data(name.map(|n| n.text.clone()), lk));
            } else if ["FD", "SD", "RD"].iter().any(|k| is(c, k)) {
                push(&mut out, Ev::Data(None, "fd"));
            }
        }
        c += 1;
    }
    out
}

fn paragraph_ok(code: &[&TokenDecl], c: usize, free: bool) -> bool {
    let t = code[c];
    let upper = t.text.to_ascii_uppercase();
    if upper.starts_with("END-") || VERBS.contains(&upper.as_str()) {
        return false;
    }
    if free {
        c == 0 || code[c - 1].span.end_line < t.span.start_line
    } else {
        (8..=11).contains(&t.span.start_col)
    }
}

/// Drop any symbol whose span partially overlaps an earlier-kept one (the
/// store requires spans to nest or be disjoint).
fn no_partial_overlap(mut syms: Vec<SymbolDecl>) -> Vec<SymbolDecl> {
    syms.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then(b.span.end.cmp(&a.span.end))
    });
    let mut open: Vec<u32> = Vec::new();
    let mut out = Vec::with_capacity(syms.len());
    for s in syms {
        while open.last().is_some_and(|&e| e <= s.span.start) {
            open.pop();
        }
        if open.last().is_some_and(|&e| s.span.end > e) {
            continue;
        }
        open.push(s.span.end);
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests;
