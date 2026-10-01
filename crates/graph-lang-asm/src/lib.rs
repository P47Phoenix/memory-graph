//! Assembly extractor (x86/x64 and ARM/AArch64; NASM, MASM and GNU as
//! syntax): a line-oriented token-stream scanner, not an assembler.
//!
//! Tokens come from the shared tokenizer's `ASM` dialect. Each line is split
//! into *words* (adjacent identifier, number and `.` `$` `@` `?` `%` tokens,
//! so `.text`, `@@`, `%macro` and `x$y` are one word) and punctuation.
//!
//! | Assembly | `SymbolKind` | `lang_kind` |
//! |---|---|---|
//! | `name:` followed by code, `name LABEL` | Function | `label` |
//! | `name:` or `name` followed by data (`db`, `.byte`, `DWORD`, ...) | Variable | `data` |
//! | `name equ x`, `name = x`, `.set`/`.equ`/`.equiv name, x` | Constant | `equ` |
//! | `%define`/`%assign name` | Constant | `define` |
//! | `name PROC` ... `ENDP` (MASM) | Function | `proc` |
//! | `.macro`/`%macro name` ... `.endm`/`%endmacro`, `name MACRO` ... `ENDM` | Other | `macro` |
//! | `section`/`segment`/`.section x`, `.text`, `.data`, `.bss`, `.rodata`, `.code`, ... | Module | `section` |
//! | `name SEGMENT` ... `name ENDS` (MASM) | Module | `segment` |
//! | `struc name` ... `endstruc` (NASM), `name STRUCT`/`UNION` ... `ENDS` (MASM) | Type | `struct` |
//!
//! A label's span runs from the label through the last code token before the
//! next non-local label, section, block start or block end, so the
//! instructions of a function nest inside it; a data label ends with its
//! own data directives. MASM repeat blocks (`REPT`, `WHILE`, `FOR`, ...)
//! are only recognised in files that use a bare `ENDM`, and an unclosed
//! one ends at the next `PROC`, segment or section. A section runs to the next
//! section or segment. `.type x, @function` / `%function` and
//! `global`/`.globl`/`public x` make a label a Function even when no
//! instruction follows it (`.type x, @object` makes it a Variable); a label
//! followed by neither code nor data nor such a promotion is not a symbol.
//!
//! Extensions: `asm` and `s`. `inc` is deliberately not claimed (PHP,
//! Pascal and POV-Ray include files use it); such files stay tokens-only.
//!
//! Not symbols: local labels (`.L1`, NASM `.loop`, MASM `@@`, numeric `1:`,
//! macro-local `%%x`, Mach-O compiler locals `Ltmp0`/`LBB0_1`), labels inside a MASM `PROC` (proc-scoped there),
//! and anything inside a macro body (a template, not code).
//!
//! Known limits (the `ASM` dialect's): `;` always starts a comment (GNU as
//! uses it as a statement separator on some targets); `#` is a comment only
//! at line start, so an AT&T trailing `# comment` is read as code; ARM32 `@`
//! comments are not recognised. Macro invocations are not expanded. An
//! unclosed block (`PROC` without `ENDP`) spans its header line only.
use graph_core::scan::{mark_keywords_ignore_case, span_between};
use graph_core::tokenizer::{tokenize_with, TokenizerOptions, TOKENIZER_VERSION};
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use std::collections::HashMap;

pub struct AsmExtractor;

/// Tokenizer dialect used for assembly.
pub const ASM_TOKENIZER: TokenizerOptions = TokenizerOptions::ASM;

impl Extractor for AsmExtractor {
    fn language(&self) -> &str {
        "asm"
    }

    fn extensions(&self) -> &[&str] {
        &["asm", "s"]
    }

    fn version(&self) -> String {
        // `kw1`: assembler directives are classed `keyword` (#143).
        format!("asm-scan-1+kw1+tok{TOKENIZER_VERSION}")
    }

    fn extract(&self, source: &str) -> Extraction {
        let mut tokens = tokenize_with(source, ASM_TOKENIZER);
        let symbols = symbols(&tokens);
        // After the symbol scan, which reads identifiers as it always has.
        mark_keywords_ignore_case(&mut tokens, DOT_DIRECTIVES, |t, i| {
            directive_at(t, i) != Some(DirectiveForm::Dotted)
        });
        mark_keywords_ignore_case(&mut tokens, BARE_DIRECTIVES, |t, i| {
            match directive_at(t, i) {
                Some(DirectiveForm::Bare) => false,
                Some(DirectiveForm::Named) => !NAMED_DIRECTIVES
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(&t[i].text)),
                _ => true,
            }
        });
        Extraction {
            symbols,
            tokens,
            has_errors: false,
        }
    }
}

/// Keywords (#143) are assembler *directives* only, never instruction
/// mnemonics (thousands, per architecture, and often reused as label
/// names). GNU as directives, written with a glued leading `.` (`.globl`;
/// the word after the `.` is the keyword) as the first statement on a line.
const DOT_DIRECTIVES: &[&str] = &[
    "align",
    "arch",
    "ascii",
    "asciz",
    "balign",
    "bss",
    "byte",
    "comm",
    "data",
    "else",
    "end",
    "endif",
    "endm",
    "endr",
    "equ",
    "equiv",
    "extern",
    "file",
    "fill",
    "global",
    "globl",
    "hidden",
    "ident",
    "if",
    "ifdef",
    "ifndef",
    "include",
    "int",
    "lcomm",
    "long",
    "macro",
    "octa",
    "p2align",
    "popsection",
    "previous",
    "pushsection",
    "quad",
    "rept",
    "rodata",
    "section",
    "set",
    "short",
    "size",
    "skip",
    "space",
    "string",
    "syntax",
    "text",
    "type",
    "weak",
    "word",
    "zero",
];

/// NASM and MASM directives written bare (any case), as the first word of a
/// statement (after an optional `label:`).
const BARE_DIRECTIVES: &[&str] = &[
    "align",
    "assume",
    "bits",
    "db",
    "dd",
    "default",
    "dq",
    "dt",
    "dw",
    "end",
    "endp",
    "ends",
    "endm",
    "endstruc",
    "equ",
    "extern",
    "externdef",
    "extrn",
    "global",
    "incbin",
    "include",
    "includelib",
    "macro",
    "org",
    "proc",
    "public",
    "resb",
    "resd",
    "resq",
    "resw",
    "section",
    "segment",
    "struc",
    "times",
];

/// The [`BARE_DIRECTIVES`] that may also follow a name (`x db 1`,
/// `main PROC`, `_TEXT SEGMENT`, `n equ 3`).
const NAMED_DIRECTIVES: &[&str] = &[
    "db", "dd", "dq", "dt", "dw", "endp", "ends", "equ", "macro", "proc", "resb", "resd", "resq",
    "resw", "segment", "times",
];

#[derive(PartialEq, Eq)]
enum DirectiveForm {
    /// `.word` first on its line (after an optional `label:`).
    Dotted,
    /// `word` first on its line (after an optional `label:`).
    Bare,
    /// `name word`.
    Named,
}

/// Where the word at `i` sits, if it is in a directive position: whole (not
/// glued to a following word), not itself a label (`end:`) or assignment
/// (`size = 4`), and first in its statement as described by
/// [`DirectiveForm`]. `None` otherwise.
fn directive_at(t: &[TokenDecl], i: usize) -> Option<DirectiveForm> {
    let line = t[i].span.start_line;
    let glued = |a: &TokenDecl, b: &TokenDecl| a.span.end == b.span.start;
    if let Some(n) = t.get(i + 1).filter(|n| n.span.start_line == line) {
        if glued(&t[i], n) || matches!(n.text.as_str(), ":" | "=") {
            return None;
        }
    }
    let mut start = i;
    while start > 0 && t[start - 1].span.end_line == line {
        if i - start >= 3 {
            return None; // too far into the line (and keeps this O(1))
        }
        start -= 1;
    }
    let mut before = &t[start..i];
    if before.len() >= 2 && before[1].text == ":" {
        before = &before[2..];
    }
    let word = |x: &TokenDecl| matches!(x.class, TokenClass::Identifier | TokenClass::Keyword);
    match before {
        [] => Some(DirectiveForm::Bare),
        [dot] if dot.text == "." && glued(dot, &t[i]) => Some(DirectiveForm::Dotted),
        [name] if word(name) && !glued(name, &t[i]) => Some(DirectiveForm::Named),
        _ => None,
    }
}

/// Data-defining directives: a label before one of these is data.
const DATA: &[&str] = &[
    "db",
    "dw",
    "dd",
    "dq",
    "dt",
    "do",
    "dy",
    "dz",
    "ddq",
    "resb",
    "resw",
    "resd",
    "resq",
    "rest",
    "reso",
    "resy",
    "resz",
    "times",
    "incbin",
    "byte",
    "sbyte",
    "word",
    "sword",
    "dword",
    "sdword",
    "fword",
    "qword",
    "tbyte",
    "real4",
    "real8",
    "real10",
    "oword",
    "xmmword",
    "ymmword",
    "zmmword",
    ".byte",
    ".2byte",
    ".4byte",
    ".8byte",
    ".word",
    ".hword",
    ".short",
    ".int",
    ".long",
    ".quad",
    ".octa",
    ".ascii",
    ".asciz",
    ".string",
    ".string8",
    ".string16",
    ".zero",
    ".space",
    ".skip",
    ".fill",
    ".float",
    ".single",
    ".double",
    ".dc",
    ".dcb",
    ".ds",
    ".incbin",
    ".uleb128",
    ".sleb128",
];

/// MASM data types that are also operand size keywords (`push dword 5`,
/// `mov DWORD PTR [x], 1`): a `name DWORD 0` data label only counts when the
/// name starts in column 1, where MASM declarations are written.
const SIZE_WORDS: &[&str] = &[
    "byte", "sbyte", "word", "sword", "dword", "sdword", "fword", "qword", "tbyte", "real4",
    "real8", "real10", "oword", "xmmword", "ymmword", "zmmword",
];

/// Directives written without a leading `.` or `%` (not instructions).
const DIRECTIVES: &[&str] = &[
    "align",
    "alignb",
    "bits",
    "default",
    "extern",
    "extrn",
    "externdef",
    "global",
    "public",
    "assume",
    "include",
    "includelib",
    "option",
    "org",
    "end",
    "even",
    "cpu",
    "use16",
    "use32",
    "use64",
    "absolute",
    "common",
    "static",
    "proto",
    "title",
    "subtitle",
    "page",
    "invoke",
];

/// Section-switching directives that stand alone (`.text`, `.data`, ...).
const SECTION_WORDS: &[&str] = &[
    ".text",
    ".data",
    ".bss",
    ".rodata",
    ".code",
    ".const",
    ".data?",
    ".fardata",
    ".fardata?",
    ".stack",
];

/// Section directives followed by the section's name.
const SECTION_NAMED: &[&str] = &["section", "segment", ".section"];

/// Anonymous MASM blocks that also end with `ENDM`.
const MASM_REPEAT: &[&str] = &["rept", "repeat", "irp", "irpc", "for", "forc", "while"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Proc,
    /// A macro; anonymous (`REPT`, ...) when the open event has no name.
    Macro,
    Segment,
    /// MASM `STRUCT`/`UNION` (closed by `ENDS`, like a segment).
    Struct,
    /// NASM `struc` (closed by `endstruc`).
    Struc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Label(String),
    DataLabel(String),
    Constant(String, &'static str),
    Section(String),
    Open(Block, Option<String>),
    /// `ENDS` closes a segment or a MASM struct: `Close(Segment)`.
    Close(Block),
    Promote(String, Option<bool>),
    Data,
    Instr,
}

struct E {
    ev: Ev,
    /// First and last token index of the event.
    first: usize,
    last: usize,
}

/// A word (adjacent word-ish tokens) or a single punctuation token.
struct Item {
    text: String,
    lo: usize,
    hi: usize,
    word: bool,
    col: u32,
}

fn wordish(t: &TokenDecl) -> bool {
    match t.class {
        TokenClass::Identifier | TokenClass::Keyword => true,
        TokenClass::Literal => t.text.starts_with(|c: char| c.is_ascii_digit()),
        _ => matches!(t.text.as_str(), "." | "$" | "@" | "?" | "%"),
    }
}

fn items(tokens: &[TokenDecl], idx: &[usize]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for &i in idx {
        let t = &tokens[i];
        let w = wordish(t);
        if let Some(last) = out.last_mut() {
            if w && last.word && tokens[last.hi].span.end == t.span.start {
                last.text.push_str(&t.text);
                last.hi = i;
                continue;
            }
        }
        out.push(Item {
            text: t.text.clone(),
            lo: i,
            hi: i,
            word: w,
            col: t.span.start_col,
        });
    }
    out
}

/// Local labels are not symbols.
/// Local labels are not symbols: `.L1`/`.loop`, `%%x`, `@@`, numeric
/// `1:`, and the Apple/Mach-O compiler locals (`Ltmp0`, `LBB0_1`, ...).
/// NASM `$name` is an escaped identifier, not a local.
fn is_local(name: &str) -> bool {
    const MACHO: &[&str] = &[
        "Ltmp",
        "LBB",
        "LCPI",
        "Lfunc_",
        "Lloh",
        "Lset",
        "Lexception",
        "LJTI",
        "Lcst",
        "L_",
    ];
    name.starts_with(['.', '%'])
        || name.starts_with("@@")
        || name.starts_with(|c: char| c.is_ascii_digit())
        || MACHO.iter().any(|p| name.starts_with(p))
}

/// Symbols in assembly tokens (as produced with [`ASM_TOKENIZER`]).
pub fn symbols(tokens: &[TokenDecl]) -> Vec<SymbolDecl> {
    let evs = events(tokens);
    let code: Vec<usize> = (0..tokens.len())
        .filter(|&i| tokens[i].class != TokenClass::Comment)
        .collect();
    // Last code token strictly before token index `b` (or the last one).
    let last_before = |b: Option<usize>| -> Option<usize> {
        let n = match b {
            Some(b) => code.partition_point(|&i| i < b),
            None => code.len(),
        };
        n.checked_sub(1).map(|k| code[k])
    };
    let mut out = Vec::new();
    let mut push = |name: &str, kind, lk: &str, a: usize, b: usize| {
        out.push(SymbolDecl {
            owner: None,
            name: name.to_string(),
            kind,
            lang_kind: Some(lk.to_string()),
            span: span_between(&tokens[a].span, &tokens[b].span),
        });
    };
    // Per name: the `.type` (true = function), and whether it is global.
    let mut promo: HashMap<&str, (Option<bool>, bool)> = HashMap::new();
    for e in &evs {
        if let Ev::Promote(n, f) = &e.ev {
            let v = promo.entry(n.as_str()).or_default();
            match f {
                // A function type wins over an object one.
                Some(f) => v.0 = Some(v.0.unwrap_or(false) | f),
                None => v.1 = true,
            }
        }
    }
    let is_boundary = |ev: &Ev| {
        matches!(
            ev,
            Ev::Label(_) | Ev::DataLabel(_) | Ev::Section(_) | Ev::Open(..) | Ev::Close(_)
        )
    };
    let mut stack: Vec<(Block, Option<&str>, usize, usize)> = Vec::new();
    let header_only = |out: &mut dyn FnMut(&str, SymbolKind, &str, usize, usize),
                       b: (Block, Option<&str>, usize, usize)| {
        if let Some(n) = b.1 {
            let (k, lk) = block_kind(b.0);
            out(n, k, lk, b.2, b.3);
        }
    };
    for (i, e) in evs.iter().enumerate() {
        match &e.ev {
            Ev::Constant(n, lk) => push(n, SymbolKind::Constant, lk, e.first, e.last),
            Ev::Section(n) => {
                let next = evs[i + 1..]
                    .iter()
                    .find(|x| matches!(x.ev, Ev::Section(_) | Ev::Open(Block::Segment, _)))
                    .map(|x| x.first);
                let end = last_before(next).unwrap_or(e.last).max(e.last);
                push(n, SymbolKind::Module, "section", e.first, end);
            }
            Ev::Label(n) | Ev::DataLabel(n) => {
                let j = evs[i + 1..]
                    .iter()
                    .position(|x| is_boundary(&x.ev))
                    .map_or(evs.len(), |p| i + 1 + p);
                // A colonless data label (`msg db 1`) is its own line.
                let end = match e.ev {
                    Ev::DataLabel(_) => e.last,
                    _ => last_before(evs.get(j).map(|x| x.first))
                        .unwrap_or(e.last)
                        .max(e.last),
                };
                let mut kind = match e.ev {
                    Ev::DataLabel(_) => Some(SymbolKind::Variable),
                    _ => evs[i + 1..j].iter().find_map(|x| match x.ev {
                        Ev::Data => Some(SymbolKind::Variable),
                        Ev::Instr => Some(SymbolKind::Function),
                        _ => None,
                    }),
                };
                match promo.get(n.as_str()) {
                    Some((Some(true), _)) => kind = Some(SymbolKind::Function),
                    Some((Some(false), _)) => kind = Some(SymbolKind::Variable),
                    Some((None, true)) if kind.is_none() => kind = Some(SymbolKind::Function),
                    _ => {}
                }
                // A data label ends with its own data directives, so a
                // following `len equ $ - msg` does not nest inside it.
                let end = match (&e.ev, kind) {
                    (Ev::Label(_), Some(SymbolKind::Variable)) => evs[i + 1..j]
                        .iter()
                        .take_while(|x| x.ev == Ev::Data)
                        .last()
                        .map_or(e.last, |x| x.last),
                    _ => end,
                };
                match kind {
                    Some(SymbolKind::Function) => {
                        push(n, SymbolKind::Function, "label", e.first, end)
                    }
                    Some(k) => push(n, k, "data", e.first, end),
                    None => {}
                }
            }
            Ev::Open(b, n) => stack.push((*b, n.as_deref(), e.first, e.last)),
            Ev::Close(b) => {
                let fits = |o: Block| match b {
                    Block::Segment => matches!(o, Block::Segment | Block::Struct),
                    _ => o == *b,
                };
                if let Some(p) = stack.iter().rposition(|x| fits(x.0)) {
                    for x in stack.drain(p + 1..).collect::<Vec<_>>() {
                        header_only(&mut push, x);
                    }
                    let (ob, name, first, _) = stack.pop().unwrap();
                    if let Some(n) = name {
                        let (k, lk) = block_kind(ob);
                        push(n, k, lk, first, e.last);
                    }
                }
            }
            Ev::Promote(..) | Ev::Data | Ev::Instr => {}
        }
    }
    for x in std::mem::take(&mut stack) {
        header_only(&mut push, x);
    }
    no_partial_overlap(out)
}

fn block_kind(b: Block) -> (SymbolKind, &'static str) {
    match b {
        Block::Proc => (SymbolKind::Function, "proc"),
        Block::Macro => (SymbolKind::Other, "macro"),
        Block::Segment => (SymbolKind::Module, "segment"),
        Block::Struct | Block::Struc => (SymbolKind::Type, "struct"),
    }
}

/// Parse lines into events. Nothing inside a macro body is reported except
/// nested macro starts and ends; labels inside a MASM `PROC` are dropped.
fn events(tokens: &[TokenDecl]) -> Vec<E> {
    let mut out: Vec<E> = Vec::new();
    // Open macro bodies; true for an anonymous MASM repeat block.
    let mut macros: Vec<bool> = Vec::new();
    // MASM style: a bare `ENDM` (not GNU `.endm`) appears somewhere.
    let masm = tokens.iter().enumerate().any(|(i, t)| {
        t.class == TokenClass::Identifier
            && t.text.eq_ignore_ascii_case("endm")
            && !(i > 0 && tokens[i - 1].text == "." && tokens[i - 1].span.end == t.span.start)
    });
    let mut proc_depth = 0usize;
    let mut records: Vec<Block> = Vec::new();
    for (_, range) in graph_core::scan::line_iter(tokens) {
        let idx: Vec<usize> = range
            .filter(|&i| tokens[i].class != TokenClass::Comment)
            .collect();
        let Some(&line_last) = idx.last() else {
            continue;
        };
        let it = items(tokens, &idx);
        let mut line = Vec::new();
        let in_struct = matches!(records.last(), Some(Block::Struct | Block::Struc));
        parse_line(&it, line_last, in_struct, masm, &mut line);
        for e in line {
            // An unclosed repeat block ends at the next PROC, segment or
            // section, so later symbols are not lost inside it.
            if matches!(
                e.ev,
                Ev::Open(Block::Proc | Block::Segment, _) | Ev::Section(_)
            ) {
                while macros.last() == Some(&true) {
                    macros.pop();
                    out.push(E {
                        ev: Ev::Close(Block::Macro),
                        first: e.first,
                        last: e.first,
                    });
                }
            }
            match &e.ev {
                Ev::Open(Block::Macro, n) => macros.push(n.is_none()),
                Ev::Close(Block::Macro) if !macros.is_empty() => {
                    macros.pop();
                }
                _ if !macros.is_empty() => continue,
                Ev::Open(Block::Proc, _) => proc_depth += 1,
                Ev::Open(b @ (Block::Segment | Block::Struct | Block::Struc), _) => {
                    records.push(*b)
                }
                Ev::Close(Block::Segment) => {
                    if matches!(records.last(), Some(Block::Segment | Block::Struct)) {
                        records.pop();
                    }
                }
                Ev::Close(Block::Struc) => {
                    if records.last() == Some(&Block::Struc) {
                        records.pop();
                    }
                }
                Ev::Close(Block::Proc) => proc_depth = proc_depth.saturating_sub(1),
                Ev::Label(_) | Ev::DataLabel(_) if proc_depth > 0 => continue,
                _ => {}
            }
            out.push(e);
        }
    }
    out
}

fn parse_line(it: &[Item], line_last: usize, in_struct: bool, masm: bool, out: &mut Vec<E>) {
    let lower = |k: usize| {
        it.get(k)
            .filter(|x| x.word)
            .map(|x| x.text.to_ascii_lowercase())
    };
    let is = |k: usize, s: &str| it.get(k).is_some_and(|x| x.text == s);
    let mut k = 0;
    // Leading `name:` / `name::` labels.
    let mut last_label: Option<usize> = None;
    while it.get(k).is_some_and(|x| x.word) && is(k + 1, ":") {
        let name = &it[k].text;
        let colons = if is(k + 2, ":") { 3 } else { 2 };
        if !is_local(name) {
            out.push(E {
                ev: Ev::Label(name.clone()),
                first: it[k].lo,
                last: it[k + colons - 1].hi,
            });
            last_label = Some(out.len() - 1);
        }
        k += colons;
    }
    let Some(w0) = lower(k) else {
        return;
    };
    let first = it[k].lo;
    let whole = |ev: Ev| E {
        ev,
        first,
        last: line_last,
    };
    // `foo: equ 5` is a constant, not a label.
    if w0 == "equ" {
        if let Some(l) = last_label {
            if let Ev::Label(n) = &out[l].ev {
                let n = n.clone();
                out[l].ev = Ev::Constant(n, "equ");
                out[l].last = line_last;
            }
        }
        return;
    }
    let w1 = lower(k + 1);
    let name1 = || it.get(k + 1).filter(|x| x.word).map(|x| x.text.clone());
    match w0.as_str() {
        s if SECTION_NAMED.contains(&s) && name1().is_some() => {
            out.push(whole(Ev::Section(name1().unwrap())));
            return;
        }
        s if SECTION_WORDS.contains(&s) => {
            out.push(whole(Ev::Section(it[k].text.clone())));
            return;
        }
        "global" | ".globl" | ".global" | "public" | "globl" => {
            for (j, x) in it.iter().enumerate().skip(k + 1) {
                if x.word && !is(j - 1, ":") {
                    out.push(whole(Ev::Promote(x.text.clone(), None)));
                }
            }
            return;
        }
        ".type" => {
            if let Some(n) = name1() {
                let rest: Vec<String> = it[k + 2..]
                    .iter()
                    .map(|x| x.text.to_ascii_lowercase())
                    .collect();
                let has = |w: &str| {
                    rest.iter()
                        .any(|r| r.trim_start_matches(['@', '%', '#']) == w)
                };
                if has("function") || has("stt_func") {
                    out.push(whole(Ev::Promote(n, Some(true))));
                } else if has("object") || has("stt_object") {
                    out.push(whole(Ev::Promote(n, Some(false))));
                }
            }
            return;
        }
        ".set" | ".equ" | ".equiv" | ".eqv" => {
            if let Some(n) = name1() {
                out.push(whole(Ev::Constant(n, "equ")));
            }
            return;
        }
        "%define" | "%xdefine" | "%idefine" | "%assign" | "%iassign" => {
            if let Some(n) = name1() {
                out.push(whole(Ev::Constant(n, "define")));
            }
            return;
        }
        ".macro" | "%macro" | "%imacro" => {
            out.push(whole(Ev::Open(Block::Macro, name1())));
            return;
        }
        ".endm" | ".endmacro" | "%endmacro" | "endm" => {
            out.push(whole(Ev::Close(Block::Macro)));
            return;
        }
        s if masm && MASM_REPEAT.contains(&s) => {
            // A MASM repeat block (only in files using MASM's `ENDM`).
            out.push(whole(Ev::Open(Block::Macro, None)));
            return;
        }
        "struc" => {
            out.push(whole(Ev::Open(Block::Struc, name1())));
            return;
        }
        "endstruc" => {
            out.push(whole(Ev::Close(Block::Struc)));
            return;
        }
        _ => {}
    }
    let name0 = || it[k].text.clone();
    if is(k + 1, "=") {
        out.push(whole(Ev::Constant(name0(), "equ")));
        return;
    }
    if let Some(w1) = w1.as_deref() {
        let ev = match w1 {
            "proc" => Some(Ev::Open(Block::Proc, Some(name0()))),
            "endp" => Some(Ev::Close(Block::Proc)),
            "macro" => Some(Ev::Open(Block::Macro, Some(name0()))),
            "segment" => Some(Ev::Open(Block::Segment, Some(name0()))),
            "ends" => Some(Ev::Close(Block::Segment)),
            "struct" | "struc" | "union" => Some(Ev::Open(Block::Struct, Some(name0()))),
            "equ" | "textequ" => Some(Ev::Constant(name0(), "equ")),
            "label" if !is_local(&w0) => Some(Ev::Label(name0())),
            s if DATA.contains(&s)
                && !DATA.contains(&w0.as_str())
                && !is_local(&w0)
                && (!SIZE_WORDS.contains(&s) || in_struct || it[k].col == 1) =>
            {
                // `name db 1`: a data label; the line itself is data.
                out.push(E {
                    ev: Ev::DataLabel(name0()),
                    first,
                    last: line_last,
                });
                return;
            }
            _ => None,
        };
        if let Some(ev) = ev {
            out.push(whole(ev));
            return;
        }
    }
    let ev = if DATA.contains(&w0.as_str()) {
        Ev::Data
    } else if w0.starts_with(['.', '%']) || DIRECTIVES.contains(&w0.as_str()) {
        return;
    } else {
        Ev::Instr
    };
    out.push(whole(ev));
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
