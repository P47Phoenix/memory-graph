use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = AsmExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    ex.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn find<'a>(s: &'a [Sym], name: &str) -> &'a Sym {
    s.iter()
        .find(|x| x.0 == name)
        .unwrap_or_else(|| panic!("{name} not found in {s:#?}"))
}

fn names(s: &[Sym]) -> Vec<&str> {
    s.iter().map(|x| x.0.as_str()).collect()
}

/// Every pair of spans nests or is disjoint (what the store requires).
fn assert_nested(ex: &Extraction) {
    for a in &ex.symbols {
        for b in &ex.symbols {
            let (a, b) = (&a.span, &b.span);
            let disjoint = a.end <= b.start || b.end <= a.start;
            let nested =
                (a.start <= b.start && b.end <= a.end) || (b.start <= a.start && a.end <= b.end);
            assert!(disjoint || nested, "partial overlap {a:?} {b:?}");
        }
    }
}

fn contains(s: &[Sym], outer: &str, inner: &str) -> bool {
    let o = find(s, outer);
    let i = find(s, inner);
    o.3.contains(&i.3) && o.3.len() > i.3.len()
}

const NASM: &str = "; hello world, NASM x86-64
%define SYS_WRITE 1
BUFSZ equ 64
LIMIT: equ 128
%macro print 2
  mov rsi, %1
inner: nop
%endmacro

section .data
msg     db 'hi', 10
len     equ $ - msg
buf     resb BUFSZ

struc point
  .x resd 1
  .y resd 1
endstruc

section .text
global _start
_start:
    mov eax, SYS_WRITE  ; write
.loop:
    dec ecx
    jnz .loop
1:  jmp 1b
helper:
    push dword 5
    ret
";

#[test]
fn nasm() {
    let s = syms(NASM);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("SYS_WRITE"), (SymbolKind::Constant, "define"));
    assert_eq!(k("BUFSZ"), (SymbolKind::Constant, "equ"));
    assert_eq!(find(&s, "BUFSZ").3, "BUFSZ equ 64");
    assert_eq!(find(&s, "LIMIT").3, "LIMIT: equ 128");
    assert_eq!(k("print"), (SymbolKind::Other, "macro"));
    assert!(find(&s, "print").3.ends_with("%endmacro"));
    assert_eq!(k(".data"), (SymbolKind::Module, "section"));
    assert_eq!(k("msg"), (SymbolKind::Variable, "data"));
    assert_eq!(find(&s, "msg").3, "msg     db 'hi', 10");
    assert_eq!(k("len"), (SymbolKind::Constant, "equ"));
    assert_eq!(find(&s, "buf").3, "buf     resb BUFSZ");
    assert_eq!(k("point"), (SymbolKind::Type, "struct"));
    assert!(find(&s, "point").3.ends_with("endstruc"));
    assert_eq!(k("_start"), (SymbolKind::Function, "label"));
    assert_eq!(
        find(&s, "_start").3,
        "_start:\n    mov eax, SYS_WRITE  ; write\n.loop:\n    dec ecx\n    jnz .loop\n1:  jmp 1b"
    );
    assert_eq!(find(&s, "helper").3, "helper:\n    push dword 5\n    ret");
    assert_eq!(k("helper"), (SymbolKind::Function, "label"));
    // Local labels, macro bodies and field names are not symbols.
    for n in [".loop", "1", "inner", ".x", "push"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
    assert!(contains(&s, ".text", "_start"));
    assert!(contains(&s, ".data", "msg"));
    assert!(contains(&s, ".data", "point"));
    assert!(!contains(&s, ".data", "_start"));
}

const MASM: &str = "; MASM x64
.data
count   DWORD 0
text    BYTE 'it''s', 0
MAXV = 10
.data?
scratch QWORD ?

POINT STRUCT
  x DWORD ?
  y DWORD ?
POINT ENDS

Swap MACRO a, b
  xchg a, b
ENDM

_TEXT SEGMENT
Add2 PROC
    mov DWORD PTR [rcx], 1
@@: dec rcx
    jnz @B
inner:
    ret
Add2 ENDP
REPT 3
  nop
ENDM
_TEXT ENDS

.code
Main PROC PUBLIC
    call Add2
Main ENDP
END
";

#[test]
fn masm() {
    let s = syms(MASM);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k(".data"), (SymbolKind::Module, "section"));
    assert_eq!(k(".data?"), (SymbolKind::Module, "section"));
    assert_eq!(k("count"), (SymbolKind::Variable, "data"));
    assert_eq!(find(&s, "count").3, "count   DWORD 0");
    assert_eq!(find(&s, "text").3, "text    BYTE 'it''s', 0");
    assert_eq!(k("MAXV"), (SymbolKind::Constant, "equ"));
    assert_eq!(k("POINT"), (SymbolKind::Type, "struct"));
    assert!(find(&s, "POINT").3.ends_with("POINT ENDS"));
    assert!(contains(&s, "POINT", "x"));
    assert_eq!(k("Swap"), (SymbolKind::Other, "macro"));
    assert_eq!(find(&s, "Swap").3, "Swap MACRO a, b\n  xchg a, b\nENDM");
    assert_eq!(k("_TEXT"), (SymbolKind::Module, "segment"));
    assert!(find(&s, "_TEXT").3.ends_with("_TEXT ENDS"));
    assert_eq!(k("Add2"), (SymbolKind::Function, "proc"));
    assert!(find(&s, "Add2").3.starts_with("Add2 PROC"));
    assert!(find(&s, "Add2").3.ends_with("Add2 ENDP"));
    assert!(contains(&s, "_TEXT", "Add2"));
    assert_eq!(k("Main"), (SymbolKind::Function, "proc"));
    assert!(contains(&s, ".code", "Main"));
    // Proc-scoped labels, `@@`, operand size words are not symbols.
    for n in ["inner", "@@", "mov", "a"] {
        assert!(!names(&s).contains(&n), "{n}: {s:#?}");
    }
}

const GAS: &str = "/* GNU as, AArch64 */
# 1 \"x.S\"
    .section .rodata
.Lstr:
    .asciz \"hi\"
    .set BUF_LEN, 256
    .equ FLAGS, 3
    .data
    .globl table
    .type table, %object
table:
    .quad 1, 2
    .text
    .globl add
    .type add, @function
add:
    .cfi_startproc
    add x0, x0, x1  // sum
    ret
    .cfi_endproc
    .size add, .-add
    .macro PUSH2 a, b
    stp \\a, \\b, [sp, #-16]!
    .endm
    .globl entry
entry:
alias:
    b add
";

#[test]
fn gnu_as() {
    let s = syms(GAS);
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k(".rodata"), (SymbolKind::Module, "section"));
    assert_eq!(k("BUF_LEN"), (SymbolKind::Constant, "equ"));
    assert_eq!(find(&s, "BUF_LEN").3, ".set BUF_LEN, 256");
    assert_eq!(k("FLAGS"), (SymbolKind::Constant, "equ"));
    assert_eq!(k("table"), (SymbolKind::Variable, "data"));
    assert_eq!(find(&s, "table").3, "table:\n    .quad 1, 2");
    assert_eq!(k("add"), (SymbolKind::Function, "label"));
    assert_eq!(
        find(&s, "add").3,
        "add:\n    .cfi_startproc\n    add x0, x0, x1  // sum\n    ret\n    .cfi_endproc\n    .size add, .-add"
    );
    assert_eq!(k("PUSH2"), (SymbolKind::Other, "macro"));
    // `entry` is promoted by `.globl` though only a label follows it.
    assert_eq!(find(&s, "entry").3, "entry:");
    assert_eq!(k("entry"), (SymbolKind::Function, "label"));
    assert_eq!(k("alias"), (SymbolKind::Function, "label"));
    assert!(!names(&s).contains(&".Lstr"));
    assert!(contains(&s, ".text", "add"));
    assert!(contains(&s, ".text", "PUSH2"));
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}; é\nmsg db 'é'\nfoo:\n  ret\n";
    let ex = AsmExtractor.extract(src);
    let foo = ex.symbols.iter().find(|s| s.name == "foo").unwrap();
    assert_eq!((foo.span.start_line, foo.span.start_col), (3, 1));
    assert_eq!((foo.span.end_line, foo.span.end_col), (4, 6));
    assert_eq!(
        &src[foo.span.start as usize..foo.span.end as usize],
        "foo:\n  ret"
    );
    let msg = ex.symbols.iter().find(|s| s.name == "msg").unwrap();
    assert_eq!((msg.span.start_line, msg.span.start_col), (2, 1));
    assert_eq!(msg.span.end_col, 11);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "Foo PROC\n  ret\n",
        "ENDP\nENDS\nENDM\n.endm\n%endmacro\nendstruc\n",
        ".macro m\nfoo:\n  nop\n",
        "x STRUCT\n y PROC\n x ENDS\n y ENDP\n",
        "section\n.section\nglobal\n.type\n.set\n%define\n: :: ::: \n",
        "a PROC\nsection .text\nb:\n nop\na ENDP\n",
        "'unterminated\nfoo: nop",
    ] {
        let ex = AsmExtractor.extract(src);
        assert!(!ex.has_errors);
        assert_nested(&ex);
        for s in &ex.symbols {
            assert!(s.span.end as usize <= src.len());
        }
    }
    // An unclosed PROC spans its header.
    let s = syms("Foo PROC\n  ret\n");
    assert_eq!(find(&s, "Foo").3, "Foo PROC");
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(parts in proptest::collection::vec(prop_oneof![
        Just("foo:"), Just("bar"), Just(" PROC"), Just(" ENDP"), Just(" MACRO"), Just("ENDM"),
        Just(" SEGMENT"), Just(" ENDS"), Just(" STRUCT"), Just("struc x"), Just("endstruc"),
        Just(".macro m"), Just(".endm"), Just("section .text"), Just(".data"), Just(" db 1"),
        Just(" equ 3"), Just(" = 4"), Just("global foo"), Just(".type foo, @function"),
        Just("\n"), Just("\n"), Just("  mov eax, 1"), Just(".L1:"), Just("@@:"), Just("1:"),
        Just("; c"), Just("é"), Just("'x"), Just("REPT 2"), Just("%macro p 1"), Just("%endmacro"),
    ], 0..40)) {
        let src: String = parts.concat();
        let ex = AsmExtractor.extract(&src);
        assert_nested(&ex);
        for s in &ex.symbols {
            prop_assert!(s.span.start <= s.span.end && s.span.end as usize <= src.len());
            prop_assert!(src.is_char_boundary(s.span.start as usize));
            prop_assert!(src.is_char_boundary(s.span.end as usize));
        }
    }
}

#[test]
fn inc_is_not_claimed() {
    assert_eq!(AsmExtractor.extensions(), ["asm", "s"]);
}

#[test]
fn repeat_words_only_open_blocks_in_masm_files() {
    // GNU/NASM file (no bare ENDM): `while`/`for` are not blocks.
    let s = syms("for:\n  nop\nwhile x\nfoo:\n  ret\n");
    assert_eq!(find(&s, "foo").1, SymbolKind::Function);
    // MASM file: an unclosed REPT ends at the next PROC.
    let src = "M MACRO\nENDM\nREPT 3\n  nop\nLater PROC\n  ret\nLater ENDP\n.data\nv DWORD 0\n";
    let s = syms(src);
    assert_eq!(find(&s, "Later").2, "proc");
    assert_eq!(find(&s, "v").1, SymbolKind::Variable);
    // A closed REPT hides its body.
    let s = syms("M MACRO\nENDM\nWHILE x\nhidden:\n  nop\nENDM\nshown:\n  nop\n");
    assert!(!names(&s).contains(&"hidden"));
    assert!(names(&s).contains(&"shown"));
}

#[test]
fn data_label_ends_at_its_data() {
    let src = "section .data\nmsg: db 'hi', 10\n     db 0\nlen equ $ - msg\nSIZE: equ 4\n%define K 1\n.set J, 2\n";
    let ex = AsmExtractor.extract(src);
    let s = syms(src);
    assert_eq!(find(&s, "msg").3, "msg: db 'hi', 10\n     db 0");
    // The constants are siblings of `msg`, not nested inside it.
    let msg = ex.symbols.iter().find(|x| x.name == "msg").unwrap();
    for c in ["len", "SIZE", "K", "J"] {
        let c = ex.symbols.iter().find(|x| x.name == c).unwrap();
        assert!(c.span.start >= msg.span.end, "{c:?}");
    }
}

#[test]
fn local_label_rules() {
    let src = "_main:\n  nop\nLtmp0:\n  nop\nLBB0_1:\n  nop\nLloh2:\n  nop\n@@:\n  nop\n%%skip:\n  nop\n.Lx:\n  nop\n$escaped:\n  nop\nLoop:\n  nop\n";
    let s = syms(src);
    let n = names(&s);
    for local in ["Ltmp0", "LBB0_1", "Lloh2", "@@", "%%skip", ".Lx"] {
        assert!(!n.contains(&local), "{local}: {n:?}");
    }
    assert!(n.contains(&"$escaped"), "{n:?}");
    assert!(n.contains(&"Loop"), "{n:?}");
    assert!(n.contains(&"_main"), "{n:?}");
}

fn classes_of(toks: &[TokenDecl], text: &str) -> Vec<TokenClass> {
    toks.iter()
        .filter(|t| t.text == text)
        .map(|t| t.class)
        .collect()
}

/// #143: every listed directive, bare in its position (`.word` for GNU as,
/// `word` first on a line for NASM/MASM, `x word` for the named ones), in
/// lower and upper case, is a keyword.
#[test]
fn every_listed_keyword_is_classed_keyword() {
    let cases = DOT_DIRECTIVES
        .iter()
        .map(|d| {
            (
                *d,
                format!(".{d} x\n"),
                format!(".{} x\n", d.to_uppercase()),
            )
        })
        .chain(
            BARE_DIRECTIVES
                .iter()
                .map(|d| (*d, format!("{d} x\n"), format!("{} x\n", d.to_uppercase()))),
        )
        .chain(NAMED_DIRECTIVES.iter().map(|d| {
            (
                *d,
                format!("n {d} 1\n"),
                format!("n {} 1\n", d.to_uppercase()),
            )
        }));
    for (d, lower, upper) in cases {
        for (src, w) in [(lower, d.to_string()), (upper, d.to_uppercase())] {
            let toks = AsmExtractor.extract(&src).tokens;
            assert_eq!(classes_of(&toks, &w), [TokenClass::Keyword], "{src:?}");
        }
    }
}

/// Mnemonics, operands, labels, assignments, glued words and directives out
/// of statement position stay identifiers; symbols are unchanged.
#[test]
fn asm_directive_positions() {
    let src = "section .text\nglobal main\nmain:\n  mov eax, end\n  jmp end\nend: ret\nsize = 4\nmsg db 1\n.globl f\nf: .type f, @function\n";
    let ex = AsmExtractor.extract(src);
    let toks = &ex.tokens;
    assert_eq!(classes_of(toks, "section"), [TokenClass::Keyword]);
    assert_eq!(classes_of(toks, "global"), [TokenClass::Keyword]);
    assert_eq!(classes_of(toks, "db"), [TokenClass::Keyword]);
    assert_eq!(classes_of(toks, "globl"), [TokenClass::Keyword]);
    // `f: .type` is after a label.
    assert_eq!(classes_of(toks, "type"), [TokenClass::Keyword]);
    // `section .text`: the section name is an operand.
    assert_eq!(classes_of(toks, "text"), [TokenClass::Identifier]);
    // Operands, a label and an assignment named like directives.
    assert_eq!(classes_of(toks, "end"), [TokenClass::Identifier; 3]);
    assert_eq!(classes_of(toks, "size"), [TokenClass::Identifier]);
    for m in ["mov", "jmp", "ret", "eax", "main"] {
        assert!(
            classes_of(toks, m)
                .iter()
                .all(|c| *c == TokenClass::Identifier),
            "{m}"
        );
    }
    // Glued to a following word (`text.x`), or more than one word in.
    let toks = AsmExtractor.extract(".text.x\na b c section\n").tokens;
    assert!(
        toks.iter().all(|t| t.class != TokenClass::Keyword),
        "{toks:?}"
    );
    let names: Vec<_> = ex.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"main") && names.contains(&"end"),
        "{names:?}"
    );
    assert!(AsmExtractor
        .version()
        .starts_with("asm-scan-1+kw2+cmt1+tok"));
}

/// Labels and assignments named like bare directives stay identifiers;
/// so does a directive word more than three tokens into a line, and the
/// Named-form edge cases behave as documented.
#[test]
fn asm_directive_edge_positions() {
    let toks = AsmExtractor
        .extract("global:\nalign = 4\norg: ret\n")
        .tokens;
    assert!(
        toks.iter().all(|t| t.class != TokenClass::Keyword),
        "{toks:?}"
    );
    let toks = AsmExtractor
        .extract("a b c d section\nl: x y z db\n")
        .tokens;
    assert!(
        toks.iter().all(|t| t.class != TokenClass::Keyword),
        "{toks:?}"
    );
    let toks = AsmExtractor
        .extract("l: x db 1\nmain proc\ntimes 10 db 0\n")
        .tokens;
    assert_eq!(
        classes_of(&toks, "db"),
        [TokenClass::Keyword, TokenClass::Identifier]
    );
    assert_eq!(classes_of(&toks, "proc"), [TokenClass::Keyword]);
    assert_eq!(classes_of(&toks, "times"), [TokenClass::Keyword]);
}

/// #200: a named directive after an operand-taking mnemonic or directive is
/// an operand, and a bare directive followed by a named one is a name.
#[test]
fn named_directive_not_after_mnemonic() {
    let src = "call proc\njmp times\nJNZ DB\npush dword\nextern segment\nsection db 1\n";
    let toks = AsmExtractor.extract(src).tokens;
    for w in ["proc", "times", "DB", "segment", "section"] {
        assert_eq!(classes_of(&toks, w), [TokenClass::Identifier], "{w}");
    }
    assert_eq!(classes_of(&toks, "db"), [TokenClass::Keyword]);
    assert_eq!(classes_of(&toks, "extern"), [TokenClass::Keyword]);
    // Still keywords: a name, a label, and a bare directive whose next
    // word is not a named directive.
    let toks = AsmExtractor
        .extract("msg db 1\nl: x dw 2\nsection .data\nsection\ndb 3\n")
        .tokens;
    assert_eq!(classes_of(&toks, "db"), [TokenClass::Keyword; 2]);
    assert_eq!(classes_of(&toks, "dw"), [TokenClass::Keyword]);
    assert_eq!(classes_of(&toks, "section"), [TokenClass::Keyword; 2]);
    // Label names that are also mnemonics or directives keep their data.
    let toks = AsmExtractor
        .extract("str db 'x',0\nb db 1\ntest dd 0\npage dw 0\n")
        .tokens;
    assert_eq!(classes_of(&toks, "db"), [TokenClass::Keyword; 2]);
    assert_eq!(classes_of(&toks, "dd"), [TokenClass::Keyword]);
    assert_eq!(classes_of(&toks, "dw"), [TokenClass::Keyword]);
    // AT&T size suffixes and ARM branch variants take a label operand.
    let src =
        "callq proc\njmpq times\npushq db\npushl dw\ncbz proc\ncbnz times\nbne proc\nbleq times\n";
    let toks = AsmExtractor.extract(src).tokens;
    assert!(
        toks.iter().all(|t| t.class != TokenClass::Keyword),
        "{toks:?}"
    );
}

fn comments(src: &str) -> Vec<String> {
    let ex = AsmExtractor.extract(src);
    assert_exact_spans(src, &ex.tokens);
    ex.tokens
        .into_iter()
        .filter(|t| t.class == TokenClass::Comment)
        .map(|t| t.text)
        .collect()
}

/// Every token's text, byte range, line and column match the source.
fn assert_exact_spans(src: &str, toks: &[TokenDecl]) {
    let pos = |b: usize| {
        let before = &src[..b];
        let line = 1 + before.matches('\n').count() as u32;
        let col = 1 + before.rsplit('\n').next().unwrap().chars().count() as u32;
        (line, col)
    };
    let mut prev = 0;
    for t in toks {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        assert!(s >= prev, "{t:?}");
        assert_eq!(&src[s..e], t.text);
        assert_eq!((t.span.start_line, t.span.start_col), pos(s), "{t:?}");
        assert_eq!((t.span.end_line, t.span.end_col), pos(e), "{t:?}");
        prev = e;
    }
}

/// #197: AT&T trailing `#` comments, in files with `%`-registers.
#[test]
fn att_trailing_hash_comments() {
    let src =
        "# head\n  xorw %ax,%ax   # Set %ax to zero  \r\n  call f@PLT # é 'x\n  movl $1, %eax\n";
    assert_eq!(comments(src), ["# head", "# Set %ax to zero  \r", "# é 'x"]);
    let toks = AsmExtractor.extract(src).tokens;
    assert_eq!(classes_of(&toks, "PLT"), [TokenClass::Identifier]);
    assert_ne!(classes_of(&toks, "@"), [TokenClass::Comment]);
    // A `#` inside a string, or followed on its line by a block comment
    // crossing the line end, is left as tokenized.
    let src = "  movl %eax, %ebx\n  .ascii \"a # b\"\n  ret # x /* y\n z */\n";
    assert_eq!(comments(src), ["/* y\n z */"]);
}

/// #197: ARM32 `@` comments, in files with `.arm`/`.thumb`/`.syntax`.
#[test]
fn arm32_at_comments() {
    let src = "@ banner #1\n.arm\ncommands:  @ mem. #0x0\n  mov r0, #1 @ one\n";
    assert_eq!(comments(src), ["@ banner #1", "@ mem. #0x0", "@ one"]);
    let toks = AsmExtractor.extract(src).tokens;
    assert_eq!(classes_of(&toks, "#"), [TokenClass::Punctuation]);
    assert_eq!(comments(".syntax unified\n  bx lr @ ret\n"), ["@ ret"]);
    assert_eq!(comments(".arm\r\n  bx lr @ ret\r\n"), ["@ ret\r"]);
}

/// A marker at end of file with no newline.
#[test]
fn trailing_comment_at_eof() {
    assert_eq!(comments(".arm\n  bx lr @ end"), ["@ end"]);
    assert_eq!(comments("  ret %eax # end"), ["# end"]);
    assert_eq!(comments(".thumb\n  bx lr @"), ["@"]);
}

/// #197 negatives: no marker without the syntax evidence, and symbols do
/// not change either way.
#[test]
fn trailing_comment_markers_need_syntax_evidence() {
    // ARM/AArch64 immediates, Mach-O `@PAGE`, NASM, and AT&T `@` suffixes.
    for src in [
        "  mov r0, #1\n  add x0, x1, #2\n",
        "  adrp x0, msg@PAGE @ x\n",
        "section .text\n  mov eax, 1 # x\n%define n 1\n",
        "  call f@PLT\n  movq foo@GOTPCREL(%rip), %rax\n",
    ] {
        let toks = AsmExtractor.extract(src).tokens;
        assert!(
            toks.iter().all(|t| t.class != TokenClass::Comment),
            "{src:?}: {toks:?}"
        );
    }
    let src = "f:\n  movl %eax, %ebx # done\n  ret # x\n";
    let tokenized = tokenize_with(src, ASM_TOKENIZER);
    assert_eq!(AsmExtractor.extract(src).symbols, symbols(&tokenized));
}
