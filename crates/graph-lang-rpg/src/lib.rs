//! RPG IV / RPGLE extractor: a token-stream scanner, not a compiler.
//!
//! Tokens come from the shared tokenizer's `RPG` dialect: fixed form
//! (column 6 form type, `*` comment lines, sequence and 81+ comment areas)
//! unless the first line is `**FREE`. Three layouts are handled, also mixed
//! in one file: fully free (`**FREE`), free-form statements in columns 8-80
//! of a fixed-form file (with or without `/free` ... `/end-free`), and
//! fixed-form column specifications. Keywords are case-insensitive.
//!
//! | RPG | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `dcl-proc` ... `end-proc`; `P name B` ... `P E` | Function | `procedure` |
//! | `BEGSR name` ... `ENDSR` (free or C spec) | Function | `subroutine` |
//! | `dcl-pr` ... `end-pr`; D spec `PR` | Other | `prototype` |
//! | `dcl-pi` ... `end-pi`; D spec `PI` | Other | `interface` |
//! | `dcl-ds` ... `end-ds` (or one statement with `likeds`/`likerec`); D spec `DS` | Type | `ds` |
//! | `dcl-s`; D spec `S` | Variable | `standalone` |
//! | `dcl-c`; D spec `C` | Constant | `constant` |
//! | C spec `name TAG` | Other | `tag` |
//!
//! Spans: a free-form declaration runs from its `dcl-` keyword through the
//! `;` ending its `end-` statement (or its own `;`); a fixed D spec runs
//! through the following D lines with a blank declaration type (subfields,
//! parameters, keyword continuations), and a `...` long name on the line
//! before is included. An unnamed `dcl-pi *n` / `dcl-ds *n` is named `*N`.
//! A block whose end is missing (before the next procedure, subroutine or
//! declaration block boundary) spans its header statement only.
//!
//! Not scanned: compile-time data after a `**CTDATA` / `**` line (the
//! tokenizer makes each data section one Literal), compiler
//! directives (`/copy`, `/free`, `/if` ...), `dcl-f` files, `dcl-subf` /
//! `dcl-parm` members and H/F/I/O specs. RPG III (RPG/400) specs are read
//! with the RPG IV columns. Columns count characters, so a tab in a fixed
//! spec counts as one column (as the tokenizer does); tab-indented fixed
//! specs lose their column positions.
//!
//! Keywords (#143): declaration keywords (`dcl-*`, `end-*`, `ctl-opt`) and
//! `and`/`or`/`not` anywhere; operation codes only in opcode position (first
//! in a free-form statement and not an assignment target, or a fixed C
//! spec's columns 26-35), and never as the first word of a subfield or
//! parameter line inside a `dcl-ds`/`dcl-pi`/`dcl-pr` block. Limits: a
//! statement continued onto a new line whose first word is an
//! opcode-named variable (`if a and\n  read;`) labels it a keyword; an
//! unterminated multi-line `dcl-ds` without `likeds`/`likerec` and without
//! `end-ds` suppresses opcodes until the next `end-*`/`dcl-proc`.
use graph_core::scan::{line_iter, mark_keywords_ignore_case, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};

pub struct RpgExtractor;

/// RPG IV free-form declaration keywords and the logical operators, upper
/// case; matched case-insensitively wherever they appear, except as names
/// (see [`is_name`]).
const DECL_KEYWORDS: &[&str] = &[
    "AND", "CTL-OPT", "DCL-C", "DCL-DS", "DCL-F", "DCL-PARM", "DCL-PI", "DCL-PR", "DCL-PROC",
    "DCL-S", "DCL-SUBF", "END-DS", "END-PI", "END-PR", "END-PROC", "NOT", "OR",
];

/// Common operation codes, upper case; matched case-insensitively. Opcodes
/// are not reserved (a variable may be named `read`), so they are keywords
/// only in opcode position (see [`opcode_positions`]).
const OPCODES: &[&str] = &[
    "BEGSR", "CALLP", "CHAIN", "CLOSE", "DELETE", "DOU", "DOW", "DSPLY", "ELSE", "ELSEIF", "ENDDO",
    "ENDFOR", "ENDIF", "ENDMON", "ENDSL", "ENDSR", "EVAL", "EXFMT", "EXSR", "FOR", "IF", "ITER",
    "LEAVE", "LEAVESR", "MONITOR", "ON-ERROR", "OPEN", "OTHER", "READ", "READE", "RETURN",
    "SELECT", "SETGT", "SETLL", "UPDATE", "WHEN", "WRITE",
];

/// Whether the word at `i` is the target of an assignment (`read = *on;`,
/// `n += 1;`) or qualified (`ds.x = 1;`): a variable, not an opcode.
fn is_assigned(t: &[TokenDecl], i: usize) -> bool {
    let next = |k: usize| t.get(k).filter(|x| x.class != TokenClass::Comment);
    match next(i + 1).map(|x| x.text.as_str()) {
        Some(s) if s.starts_with('=') || s.ends_with('=') => true,
        Some(".") => t[i].span.end == t[i + 1].span.start,
        Some("+" | "-" | "*" | "/") => {
            next(i + 2).is_some_and(|x| x.text == "=" && x.span.start == t[i + 1].span.end)
        }
        _ => false,
    }
}

/// Which tokens sit in opcode position: the first code token of a free-form
/// statement (after a `;`, or first on its line) that is not a subfield or
/// parameter line inside an open `dcl-ds` / `dcl-pi` / `dcl-pr` block, or a
/// fixed C spec's operation field (columns 26-35).
fn opcode_positions(t: &[TokenDecl]) -> Vec<bool> {
    let mut out = vec![false; t.len()];
    let mut prev: Option<usize> = None;
    let mut in_block = false;
    let mut line_first_col6_c = false;
    let mut line = u32::MAX;
    for i in 0..t.len() {
        if t[i].class == TokenClass::Comment {
            continue;
        }
        let new_line = t[i].span.start_line != line;
        if new_line {
            line = t[i].span.start_line;
            line_first_col6_c = t[i].span.start_col == 6 && t[i].text.eq_ignore_ascii_case("c");
        }
        let lower = t[i].text.to_ascii_lowercase();
        let starts =
            prev.is_none_or(|p| t[p].text == ";" || t[p].span.end_line < t[i].span.start_line);
        if starts {
            match lower.as_str() {
                "dcl-ds" | "dcl-pi" | "dcl-pr" => {
                    // A one-statement `dcl-ds x likeds(y);` opens no block.
                    let end = (i..t.len()).find(|&j| t[j].text == ";").unwrap_or(t.len());
                    in_block = !t[i..end].iter().any(|x| {
                        x.text.eq_ignore_ascii_case("likeds")
                            || x.text.eq_ignore_ascii_case("likerec")
                    });
                }
                "end-ds" | "end-pi" | "end-pr" | "dcl-proc" | "end-proc" => in_block = false,
                _ => out[i] = !in_block && !is_assigned(t, i),
            }
        } else if line_first_col6_c && (26..=35).contains(&t[i].span.start_col) {
            out[i] = true;
        }
        prev = Some(i);
    }
    out
}

/// Whether the listed word at `i` is a name, not a keyword: glued after
/// `%` (a built-in, `%open`), `*` (a special word) or `.` (a qualified
/// subfield), or the name declared by the `dcl-` keyword before it
/// (`dcl-s read ind;`).
fn is_name(t: &[TokenDecl], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let p = &t[i - 1];
    let glued = p.span.end == t[i].span.start;
    (glued && matches!(p.text.as_str(), "%" | "*" | "."))
        // `p` may already be relabeled (marking runs front to back).
        || (matches!(p.class, TokenClass::Identifier | TokenClass::Keyword)
            && p.text.to_ascii_lowercase().starts_with("dcl-"))
}

/// Tokenizer dialect used for RPG.
pub const RPG_TOKENIZER: TokenizerOptions = TokenizerOptions::RPG;

impl Extractor for RpgExtractor {
    fn language(&self) -> &str {
        "rpg"
    }

    fn extensions(&self) -> &[&str] {
        &["rpgle", "sqlrpgle", "rpgleinc", "rpg"]
    }

    fn version(&self) -> String {
        // `kw1`: declaration keywords and operation codes are classed
        // `keyword` (#143).
        format!("rpg-scan-2+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, RPG_TOKENIZER);
        let symbols = symbols(source, &tokens);
        // After the symbol scan, which reads identifiers as it always has.
        let at_opcode = opcode_positions(&tokens);
        mark_keywords_ignore_case(&mut tokens, DECL_KEYWORDS, is_name);
        mark_keywords_ignore_case(&mut tokens, OPCODES, |t, i| !at_opcode[i] || is_name(t, i));
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// `**FREE` (any case) as the whole first line, after an optional BOM.
fn is_free(src: &str) -> bool {
    let s = src.strip_prefix('\u{feff}').unwrap_or(src);
    let first = s.split('\n').next().unwrap_or("");
    first.trim_end().eq_ignore_ascii_case("**free")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blk {
    Pr,
    Pi,
    Ds,
}

impl Blk {
    fn from(word: &str) -> Option<Blk> {
        match word {
            "pr" => Some(Blk::Pr),
            "pi" => Some(Blk::Pi),
            "ds" => Some(Blk::Ds),
            _ => None,
        }
    }

    fn kind(self) -> (SymbolKind, &'static str) {
        match self {
            Blk::Pr => (SymbolKind::Other, "prototype"),
            Blk::Pi => (SymbolKind::Other, "interface"),
            Blk::Ds => (SymbolKind::Type, "ds"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    ProcBegin(String),
    ProcEnd,
    SrBegin(String),
    SrEnd,
    BlockBegin(Blk, String),
    BlockEnd(Blk),
    /// A complete declaration.
    Single(String, SymbolKind, &'static str),
}

struct E {
    ev: Ev,
    first: usize,
    last: usize,
}

/// Symbols in RPG tokens (as produced with [`RPG_TOKENIZER`] from `src`).
pub fn symbols(src: &str, tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let evs = events(src, tokens);
    let mut out = Vec::new();
    let push = |out: &mut Vec<SymbolDecl>, n: &str, k, lk: &str, a: usize, b: usize| {
        out.push(SymbolDecl {
            owner: None,
            name: n.to_string(),
            kind: k,
            lang_kind: Some(lk.to_string()),
            span: span_between(&tokens[a].span, &tokens[b].span),
        })
    };
    // The first event after `i` satisfying `end`, unless `stop` comes first.
    let close = |i: usize, end: &dyn Fn(&Ev) -> bool, stop: &dyn Fn(&Ev) -> bool| {
        evs[i + 1..]
            .iter()
            .find(|x| end(&x.ev) || stop(&x.ev))
            .filter(|x| end(&x.ev))
            .map(|x| x.last)
    };
    for (i, e) in evs.iter().enumerate() {
        match &e.ev {
            Ev::ProcBegin(n) => {
                let last = close(i, &|x| matches!(x, Ev::ProcEnd), &|x| {
                    matches!(x, Ev::ProcBegin(_))
                });
                let f = SymbolKind::Function;
                push(&mut out, n, f, "procedure", e.first, last.unwrap_or(e.last));
            }
            Ev::SrBegin(n) => {
                let last = close(i, &|x| matches!(x, Ev::SrEnd), &|x| {
                    matches!(x, Ev::SrBegin(_) | Ev::ProcBegin(_) | Ev::ProcEnd)
                });
                let f = SymbolKind::Function;
                push(
                    &mut out,
                    n,
                    f,
                    "subroutine",
                    e.first,
                    last.unwrap_or(e.last),
                );
            }
            Ev::BlockBegin(b, n) => {
                let last = close(i, &|x| matches!(x, Ev::BlockEnd(k) if k == b), &|x| {
                    !matches!(x, Ev::Single(..) | Ev::BlockEnd(_))
                });
                let (k, lk) = b.kind();
                push(&mut out, n, k, lk, e.first, last.unwrap_or(e.last));
            }
            Ev::Single(n, k, lk) => push(&mut out, n, *k, lk, e.first, e.last),
            Ev::ProcEnd | Ev::SrEnd | Ev::BlockEnd(_) => {}
        }
    }
    no_partial_overlap(out)
}

/// A fixed-form D spec entry being built.
struct DEntry {
    name: String,
    kind: (SymbolKind, &'static str),
    first: usize,
    last: usize,
}

fn events(src: &str, tokens: &[TokenDecl]) -> Vec<E> {
    let free_file = is_free(src);
    let cut = graph_core::tokenizer::rpg_data_start(src);
    let tokens_in = &tokens[..tokens.partition_point(|t| (t.span.start as usize) < cut)];
    let mut out: Vec<E> = Vec::new();
    let mut stmt: Vec<usize> = Vec::new();
    let mut d_entry: Option<DEntry> = None;
    // A `...` long name awaiting its D/P spec line: (name, first token).
    let mut long_name: Option<(String, usize)> = None;
    let mut first_line = free_file;
    for (_, range) in line_iter(tokens_in) {
        let code: Vec<usize> = range
            .filter(|&i| tokens[i].class != TokenClass::Comment)
            .collect();
        if std::mem::take(&mut first_line) {
            continue; // the `**FREE` line
        }
        let Some(&f) = code.first() else {
            continue;
        };
        let ft = &tokens[f];
        let t = |i: usize| &tokens[code[i]];
        // Compiler directive: `/copy`, `/free`, `/if` ... (not `//` comments).
        if ft.text == "/"
            && code.get(1).is_some_and(|&n| {
                tokens[n].span.start == ft.span.end && tokens[n].class == TokenClass::Identifier
            })
            && stmt.is_empty()
        {
            continue;
        }
        let spec = (!free_file
            && ft.span.start_col == 6
            && ft.class == TokenClass::Identifier
            && ft.text.len() == 1)
            .then(|| ft.text.to_ascii_uppercase());
        let Some(spec) = spec else {
            // Free-form statement text.
            flush_d(&mut d_entry, &mut out);
            long_name = None;
            for &i in &code {
                stmt.push(i);
                if tokens[i].text == ";" {
                    free_statement(tokens, &std::mem::take(&mut stmt), &mut out);
                }
            }
            continue;
        };
        // A fixed spec line ends any unterminated free statement.
        if !stmt.is_empty() {
            free_statement(tokens, &std::mem::take(&mut stmt), &mut out);
        }
        let line_last = *code.last().unwrap();
        let in_cols = |lo: u32, hi: u32| {
            (1..code.len()).map(t).find(|x| {
                (lo..=hi).contains(&x.span.start_col) && x.class == TokenClass::Identifier
            })
        };
        // Name in columns 7-21, possibly `*N`-less; `...` marks a long name.
        let name_tok = (1..code.len())
            .map(t)
            .find(|x| x.span.start_col <= 21 && x.class == TokenClass::Identifier);
        // A long name continued on the next line ends in `...`.
        let dots = code.len() >= 4 && (code.len() - 3..code.len()).all(|i| t(i).text == ".");
        match spec.as_str() {
            "D" | "P" => {
                let decl = in_cols(24, 25).map(|x| x.text.to_ascii_lowercase());
                if dots && decl.is_none() {
                    // Only the first line of a name continued over several
                    // lines starts the entry.
                    let n = name_tok.map_or(String::new(), |x| x.text.clone());
                    match &mut long_name {
                        Some((prev, _)) => prev.push_str(&n),
                        None => long_name = Some((n, f)),
                    }
                    continue;
                }
                let (name, first) = match long_name.take() {
                    Some((n, lf)) => {
                        let mut n = n;
                        if let Some(x) = name_tok {
                            n.push_str(&x.text);
                        }
                        (n, lf)
                    }
                    None => (name_tok.map_or(String::new(), |x| x.text.clone()), f),
                };
                if spec == "P" {
                    flush_d(&mut d_entry, &mut out);
                    match decl.as_deref() {
                        Some("b") => out.push(E {
                            ev: Ev::ProcBegin(name),
                            first,
                            last: line_last,
                        }),
                        Some("e") => out.push(E {
                            ev: Ev::ProcEnd,
                            first,
                            last: line_last,
                        }),
                        _ => {}
                    }
                    continue;
                }
                let kind = match decl.as_deref() {
                    Some("ds") => Some(Blk::Ds.kind()),
                    Some("pr") => Some(Blk::Pr.kind()),
                    Some("pi") => Some(Blk::Pi.kind()),
                    Some("s") => Some((SymbolKind::Variable, "standalone")),
                    Some("c") => Some((SymbolKind::Constant, "constant")),
                    _ => None,
                };
                match kind {
                    Some(kind) => {
                        flush_d(&mut d_entry, &mut out);
                        let name = if name.is_empty() {
                            "*N".to_string()
                        } else {
                            name
                        };
                        d_entry = Some(DEntry {
                            name,
                            kind,
                            first,
                            last: line_last,
                        });
                    }
                    None => extend(&mut d_entry, line_last),
                }
            }
            "C" => {
                flush_d(&mut d_entry, &mut out);
                long_name = None;
                let factor1 = in_cols(12, 25).map(|x| x.text.clone());
                let op = in_cols(26, 35).map(|x| x.text.to_ascii_lowercase());
                let ev = match (op.as_deref(), factor1) {
                    (Some("begsr"), Some(n)) => Some(Ev::SrBegin(n)),
                    (Some("endsr"), _) => Some(Ev::SrEnd),
                    (Some("tag"), Some(n)) => Some(Ev::Single(n, SymbolKind::Other, "tag")),
                    _ => None,
                };
                if let Some(ev) = ev {
                    out.push(E {
                        ev,
                        first: f,
                        last: line_last,
                    });
                }
            }
            _ => {
                flush_d(&mut d_entry, &mut out);
                long_name = None;
            }
        }
    }
    flush_d(&mut d_entry, &mut out);
    if !stmt.is_empty() {
        free_statement(tokens, &stmt, &mut out);
    }
    out
}

fn extend(d: &mut Option<DEntry>, last: usize) {
    if let Some(d) = d {
        d.last = last;
    }
}

fn flush_d(d: &mut Option<DEntry>, out: &mut Vec<E>) {
    if let Some(d) = d.take() {
        out.push(E {
            ev: Ev::Single(d.name, d.kind.0, d.kind.1),
            first: d.first,
            last: d.last,
        });
    }
}

/// One free-form statement (token indices, comments excluded, usually
/// ending in `;`).
fn free_statement(tokens: &[TokenDecl], st: &[usize], out: &mut Vec<E>) {
    let Some((&first, &last)) = st.first().zip(st.last()) else {
        return;
    };
    let word = |k: usize| st.get(k).map(|&i| tokens[i].text.to_ascii_lowercase());
    let Some(w0) = word(0) else {
        return;
    };
    // The declared name: an identifier, or `*N`.
    let name = || -> Option<String> {
        let &i = st.get(1)?;
        let t = &tokens[i];
        if t.text == "*" {
            let &j = st.get(2)?;
            let n = &tokens[j];
            (n.span.start == t.span.end && n.text.eq_ignore_ascii_case("n"))
                .then(|| "*N".to_string())
        } else {
            (t.class == TokenClass::Identifier).then(|| t.text.clone())
        }
    };
    let has = |w: &str| st.iter().any(|&i| tokens[i].text.eq_ignore_ascii_case(w));
    let ev = match w0.as_str() {
        "dcl-proc" => name().map(Ev::ProcBegin),
        "end-proc" => Some(Ev::ProcEnd),
        "begsr" => name().map(Ev::SrBegin),
        "endsr" => Some(Ev::SrEnd),
        "dcl-s" => name().map(|n| Ev::Single(n, SymbolKind::Variable, "standalone")),
        "dcl-c" => name().map(|n| Ev::Single(n, SymbolKind::Constant, "constant")),
        w if w.starts_with("dcl-") => Blk::from(&w[4..]).and_then(|b| {
            let n = name()?;
            let one = has(&format!("end-{}", &w[4..]))
                || (b == Blk::Ds && (has("likeds") || has("likerec")));
            Some(if one {
                let (k, lk) = b.kind();
                Ev::Single(n, k, lk)
            } else {
                Ev::BlockBegin(b, n)
            })
        }),
        w if w.starts_with("end-") => Blk::from(&w[4..]).map(Ev::BlockEnd),
        _ => None,
    };
    if let Some(ev) = ev {
        out.push(E { ev, first, last });
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
