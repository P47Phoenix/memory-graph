//! Compact per-file stream codec for the v2 store (ADR 0003 story 2).
//!
//! One file's symbols and tokens become one byte string. Texts are dictionary
//! ids (the store interns them), spans are delta-coded against the previous
//! record, integers are LEB128 varints, signed deltas are zigzag. Every span
//! field is stored, so any `u32` span survives exactly, including agent-supplied
//! spans that a source scan could not reproduce (so no `irregular` escape is
//! needed in this slice). Byte 0 is [`STREAM_FORMAT`].
//!
//! Layout (after the format byte): `nsym ntok` as varints, then `nsym`
//! symbol records, then `ntok` token records:
//!
//! * symbol: `name_id kind lang_kind+1 parent+1 span`
//! * token: `term_id class parent+1 span`
//! * span: zigzag deltas `start, len, start_line, start_col, end_line-start_line`
//!   then `end_col`, where `start`, `start_line` and `start_col` are relative
//!   to the previous record of the same section and `len = end - start`.
//!
//! `parent` is the index of the enclosing symbol in the symbol section (0 =
//! the file itself). Records are in source order, so a token's index is its
//! ordinal.
use crate::StoreError;
use graph_core::{Span, SymbolKind, TokenClass};

/// Version byte of the stream layout. Bump on any layout change.
pub const STREAM_FORMAT: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymRec {
    pub name: u64,
    pub kind: SymbolKind,
    pub lang_kind: Option<u64>,
    pub parent: Option<u32>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokRec {
    pub term: u64,
    pub class: TokenClass,
    pub parent: Option<u32>,
    pub span: Span,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stream {
    pub symbols: Vec<SymRec>,
    pub tokens: Vec<TokRec>,
}

const KINDS: [SymbolKind; 7] = [
    SymbolKind::Module,
    SymbolKind::Type,
    SymbolKind::Function,
    SymbolKind::Method,
    SymbolKind::Variable,
    SymbolKind::Constant,
    SymbolKind::Other,
];
const CLASSES: [TokenClass; 7] = [
    TokenClass::Identifier,
    TokenClass::Keyword,
    TokenClass::Literal,
    TokenClass::Operator,
    TokenClass::Punctuation,
    TokenClass::Comment,
    TokenClass::Other,
];

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn diff(cur: u32, base: u32) -> u64 {
    zigzag(i64::from(cur) - i64::from(base))
}

fn put_span(out: &mut Vec<u8>, s: &Span, prev: &Span) {
    put_varint(out, diff(s.start, prev.start));
    put_varint(out, diff(s.end, s.start));
    put_varint(out, diff(s.start_line, prev.start_line));
    put_varint(out, diff(s.start_col, prev.start_col));
    put_varint(out, diff(s.end_line, s.start_line));
    put_varint(out, u64::from(s.end_col));
}

const ZERO: Span = Span {
    start: 0,
    end: 0,
    start_line: 0,
    start_col: 0,
    end_line: 0,
    end_col: 0,
};

pub fn encode(st: &Stream) -> Vec<u8> {
    let mut out = vec![STREAM_FORMAT];
    put_varint(&mut out, st.symbols.len() as u64);
    put_varint(&mut out, st.tokens.len() as u64);
    let mut prev = ZERO;
    for s in &st.symbols {
        put_varint(&mut out, s.name);
        out.push(KINDS.iter().position(|k| *k == s.kind).unwrap_or(6) as u8);
        put_varint(&mut out, s.lang_kind.map_or(0, |k| k + 1));
        put_varint(&mut out, s.parent.map_or(0, |p| u64::from(p) + 1));
        put_span(&mut out, &s.span, &prev);
        prev = s.span;
    }
    let mut prev = ZERO;
    for t in &st.tokens {
        put_varint(&mut out, t.term);
        out.push(CLASSES.iter().position(|c| *c == t.class).unwrap_or(6) as u8);
        put_varint(&mut out, t.parent.map_or(0, |p| u64::from(p) + 1));
        put_span(&mut out, &t.span, &prev);
        prev = t.span;
    }
    out
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

fn bad(what: &str) -> StoreError {
    StoreError::Corrupt(format!("stream: {what}"))
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, StoreError> {
        let v = *self.b.get(self.at).ok_or_else(|| bad("truncated"))?;
        self.at += 1;
        Ok(v)
    }
    #[inline]
    fn varint(&mut self) -> Result<u64, StoreError> {
        // Most values fit one byte.
        if let Some(&b) = self.b.get(self.at) {
            if b < 0x80 {
                self.at += 1;
                return Ok(u64::from(b));
            }
        }
        let (mut v, mut shift) = (0u64, 0u32);
        loop {
            let b = self.byte()?;
            if shift > 63 || (shift == 63 && b > 1) {
                return Err(bad("varint overflow"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    /// `base + zigzag delta`, which must be a `u32`.
    fn rel(&mut self, base: u32) -> Result<u32, StoreError> {
        let v = i64::from(base)
            .checked_add(unzigzag(self.varint()?))
            .ok_or_else(|| bad("span delta overflow"))?;
        u32::try_from(v).map_err(|_| bad("span field out of range"))
    }
    fn span(&mut self, prev: &Span) -> Result<Span, StoreError> {
        let start = self.rel(prev.start)?;
        let end = self.rel(start)?;
        let start_line = self.rel(prev.start_line)?;
        let start_col = self.rel(prev.start_col)?;
        let end_line = self.rel(start_line)?;
        let end_col = u32::try_from(self.varint()?).map_err(|_| bad("span col out of range"))?;
        Ok(Span {
            start,
            end,
            start_line,
            start_col,
            end_line,
            end_col,
        })
    }
    fn parent(&mut self) -> Result<Option<u32>, StoreError> {
        match self.varint()? {
            0 => Ok(None),
            p => Ok(Some(
                u32::try_from(p - 1).map_err(|_| bad("parent out of range"))?,
            )),
        }
    }
}

/// Header and symbol section of a stream; the token section is left
/// undecoded and [`Lazy::tokens`] walks it record by record. Symbol search and
/// term search use this to avoid materializing every token record.
pub struct Lazy<'a> {
    pub symbols: Vec<SymRec>,
    ntok: usize,
    r: Reader<'a>,
}

pub fn decode_lazy(b: &[u8]) -> Result<Lazy<'_>, StoreError> {
    let mut r = Reader { b, at: 0 };
    let fmt = r.byte()?;
    if fmt != STREAM_FORMAT {
        return Err(bad(&format!("unknown format byte {fmt}")));
    }
    let nsym = r.varint()? as usize;
    let ntok = r.varint()? as usize;
    // Every record is several bytes, so a count beyond the input is corrupt
    // (and must not drive a huge allocation).
    if nsym.saturating_add(ntok) > b.len() {
        return Err(bad("record count exceeds input"));
    }
    let mut symbols = Vec::with_capacity(nsym);
    let mut prev = ZERO;
    for i in 0..nsym {
        let name = r.varint()?;
        let kind = *KINDS
            .get(usize::from(r.byte()?))
            .ok_or_else(|| bad("bad symbol kind"))?;
        let lang_kind = r.varint()?.checked_sub(1);
        let parent = r.parent()?;
        // A parent must be an enclosing (earlier) symbol; readers index by it.
        if parent.is_some_and(|p| p as usize >= i) {
            return Err(bad("parent index out of range"));
        }
        let span = r.span(&prev)?;
        prev = span;
        symbols.push(SymRec {
            name,
            kind,
            lang_kind,
            parent,
            span,
        });
    }
    Ok(Lazy { symbols, ntok, r })
}

impl Lazy<'_> {
    /// Visit every token record in ordinal order without collecting them.
    /// Stops at the first error or when `f` returns `false`; a full pass also
    /// checks for trailing bytes.
    pub fn tokens(&mut self, mut f: impl FnMut(usize, &TokRec) -> bool) -> Result<(), StoreError> {
        let nsym = self.symbols.len();
        let r = &mut self.r;
        let mut prev = ZERO;
        for ord in 0..self.ntok {
            let term = r.varint()?;
            let class = *CLASSES
                .get(usize::from(r.byte()?))
                .ok_or_else(|| bad("bad token class"))?;
            let parent = r.parent()?;
            if parent.is_some_and(|p| p as usize >= nsym) {
                return Err(bad("parent index out of range"));
            }
            let span = r.span(&prev)?;
            prev = span;
            let rec = TokRec {
                term,
                class,
                parent,
                span,
            };
            if !f(ord, &rec) {
                return Ok(());
            }
        }
        if r.at != r.b.len() {
            return Err(bad("trailing bytes"));
        }
        Ok(())
    }
}

pub fn decode(b: &[u8]) -> Result<Stream, StoreError> {
    let mut lazy = decode_lazy(b)?;
    let mut tokens = Vec::with_capacity(lazy.ntok);
    lazy.tokens(|_, t| {
        tokens.push(t.clone());
        true
    })?;
    Ok(Stream {
        symbols: lazy.symbols,
        tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(start: u32, end: u32, l: u32, c: u32, el: u32, ec: u32) -> Span {
        Span {
            start,
            end,
            start_line: l,
            start_col: c,
            end_line: el,
            end_col: ec,
        }
    }

    fn sample() -> Stream {
        Stream {
            symbols: vec![
                SymRec {
                    name: 0,
                    kind: SymbolKind::Type,
                    lang_kind: Some(1),
                    parent: None,
                    span: sp(0, 20, 1, 1, 3, 2),
                },
                SymRec {
                    name: 2,
                    kind: SymbolKind::Method,
                    lang_kind: None,
                    parent: Some(0),
                    span: sp(4, 12, 2, 5, 2, 13),
                },
            ],
            tokens: vec![
                TokRec {
                    term: 3,
                    class: TokenClass::Keyword,
                    parent: None,
                    span: sp(0, 2, 1, 1, 1, 3),
                },
                TokRec {
                    term: 4,
                    class: TokenClass::Identifier,
                    parent: Some(1),
                    span: sp(5, 6, 2, 6, 2, 7),
                },
            ],
        }
    }

    /// Golden bytes: a change here is a format change and needs a bumped
    /// `STREAM_FORMAT` plus a migration story.
    #[test]
    fn golden_bytes() {
        let want: Vec<u8> = vec![
            1, 2, 2, // format, nsym, ntok
            0, 1, 2, 0, 0, 40, 2, 2, 4, 2, // symbol 0
            2, 3, 0, 1, 8, 16, 2, 8, 0, 13, // symbol 1: deltas from symbol 0
            3, 1, 0, 0, 4, 2, 2, 0, 3, // token 0
            4, 0, 2, 10, 2, 2, 10, 0, 7, // token 1: deltas from token 0
        ];
        assert_eq!(encode(&sample()), want);
    }

    #[test]
    fn round_trip_and_extremes() {
        let s = sample();
        assert_eq!(decode(&encode(&s)).unwrap(), s);
        assert_eq!(
            decode(&encode(&Stream::default())).unwrap(),
            Stream::default()
        );
        // Extreme, irregular and overlapping spans survive exactly.
        let m = u32::MAX;
        let s = Stream {
            symbols: vec![],
            tokens: vec![
                TokRec {
                    term: u64::MAX,
                    class: TokenClass::Other,
                    parent: None,
                    span: sp(m, m, m, m, m, m),
                },
                TokRec {
                    term: 0,
                    class: TokenClass::Comment,
                    parent: None,
                    span: sp(0, 0, 0, 0, 0, 0),
                },
                TokRec {
                    term: 1,
                    class: TokenClass::Literal,
                    parent: None,
                    span: sp(5, 2, 9, 9, 3, 1), // inverted
                },
            ],
        };
        assert_eq!(decode(&encode(&s)).unwrap(), s);
    }

    fn tok(term: u64) -> TokRec {
        TokRec {
            term,
            class: TokenClass::Other,
            parent: None,
            span: sp(0, 0, 0, 0, 0, 0),
        }
    }

    fn put(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    #[test]
    fn overlong_varint_tenth_byte_is_rejected() {
        // term = nine 0xff bytes then 0x02 (bit 64 set): overflows u64.
        let mut b = vec![1, 0, 1];
        b.extend([0xff; 9]);
        b.push(0x02);
        b.extend([0; 8]); // class, parent, six span fields
        assert!(decode(&b).is_err());
        // u64::MAX (tenth byte 0x01) is fine.
        let mut ok = vec![1, 0, 1];
        put(&mut ok, u64::MAX);
        ok.extend([0; 8]);
        assert_eq!(decode(&ok).unwrap().tokens[0].term, u64::MAX);
    }

    #[test]
    fn parent_bounds_are_strict() {
        let base = sample();
        // Symbol whose parent is itself (must be an earlier symbol).
        let mut s = base.clone();
        s.symbols[0].parent = Some(0);
        assert!(decode(&encode(&s)).is_err());
        // Symbol 1 with parent 1.
        let mut s = base.clone();
        s.symbols[1].parent = Some(1);
        assert!(decode(&encode(&s)).is_err());
        // Token parent == number of symbols; symbols themselves valid.
        let mut s = base.clone();
        s.tokens[0].parent = Some(2);
        assert!(decode(&encode(&s)).is_err());
        // Last valid parents are accepted.
        let mut s = base;
        s.tokens[0].parent = Some(1);
        assert_eq!(decode(&encode(&s)).unwrap(), s);
    }

    /// An extreme delta after a nonzero base must be an error, not an
    /// overflow panic (regression for the `checked_add` fix).
    #[test]
    fn extreme_delta_after_nonzero_base_is_an_error() {
        let mut first = tok(0);
        first.span = sp(5, 5, 1, 1, 1, 1);
        let mut b = encode(&Stream {
            symbols: vec![],
            tokens: vec![first],
        });
        b[2] = 2; // two tokens
        b.extend([0, 0, 0]); // term, class, parent
        put(&mut b, u64::MAX - 1); // unzigzag = i64::MAX
        b.extend([0; 5]);
        assert!(decode(&b).is_err());
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let good = encode(&sample());
        for n in 0..good.len() {
            assert!(decode(&good[..n]).is_err(), "prefix {n}");
        }
        let mut extra = good.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
        let mut fmt = good.clone();
        fmt[0] = 9;
        assert!(decode(&fmt).is_err());
        assert!(decode(&[1, 0xff, 0xff, 0xff, 0xff, 0x0f, 0]).is_err());
        // Crafted delta 2^64-2 (unzigzag = i64::MAX) must not overflow.
        let mut evil = encode(&Stream {
            symbols: vec![],
            tokens: vec![tok(0), tok(0)],
        });
        let n = evil.len();
        // Second token: term, class, parent, then the start delta.
        evil.truncate(n - 6);
        evil.extend_from_slice(&[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        evil.extend_from_slice(&[0, 0, 0, 0, 0]);
        assert!(decode(&evil).is_err());
        assert!(decode(
            &[1; 1]
                .iter()
                .chain(&[0x80; 12])
                .copied()
                .collect::<Vec<_>>()
        )
        .is_err());
    }
}

/// Property tests over source-text shapes (ADR 0003 story 2). The codec stores
/// every span field, so there is no `irregular` escape to exercise yet; these
/// tests pin the contract such an escape must keep: whatever the text (CRLF,
/// lone CR, BOM, combining marks, astral code points, tabs, multi-line
/// tokens), every span field survives encode/decode exactly.
#[cfg(test)]
mod props {
    use super::*;
    use graph_core::tokenizer::tokenize;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn stream_of(src: &str) -> (Stream, Vec<String>) {
        let mut ids: HashMap<String, u64> = HashMap::new();
        let mut texts: Vec<String> = Vec::new();
        let mut st = Stream::default();
        for t in tokenize(src) {
            let next = ids.len() as u64;
            let term = *ids.entry(t.text.clone()).or_insert_with(|| {
                texts.push(t.text.clone());
                next
            });
            st.tokens.push(TokRec {
                term,
                class: t.class,
                parent: None,
                span: t.span,
            });
        }
        (st, texts)
    }

    /// Round trip, and the spans still describe the source they came from.
    fn check(src: &str) {
        let (st, texts) = stream_of(src);
        let back = decode(&encode(&st)).unwrap();
        assert_eq!(back, st, "round trip of {src:?}");
        let toks = tokenize(src);
        for (r, t) in back.tokens.iter().zip(&toks) {
            assert_eq!(texts[r.term as usize], t.text);
            assert!(src.is_char_boundary(r.span.start as usize));
            assert!(src.is_char_boundary(r.span.end as usize));
            assert_eq!(&src[r.span.start as usize..r.span.end as usize], t.text);
        }
    }

    #[test]
    fn named_text_shapes_round_trip() {
        for src in [
            "fn a() {\r\n    foo();\r\n}\r\n",
            "fn a() {\rfoo();\r}\r",
            "\u{feff}fn a() {}\n",
            "\u{feff}\r\nfn a() {}\r\n",
            "let e\u{301}t\u{e9} = 1;\n",
            "let \u{1F600} = \"\u{1F600}\u{1F600}\";\r\n",
            "\tif x {\t\ty }\n",
            "/* multi\r\nline\rcomment\n*/ x // tail\r\n",
            "\"unterminated\r\nstring",
            "/* unterminated",
            "",
            "\r",
            "\r\n\r\n",
            "\u{feff}",
        ] {
            check(src);
        }
    }

    fn piece() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            "fn",
            "foo",
            "x1",
            "_",
            " ",
            "  ",
            "\t",
            "\n",
            "\r",
            "\r\n",
            "\u{feff}",
            "\u{1F600}",
            "\u{e9}",
            "e\u{301}",
            "(",
            ")",
            "{",
            "}",
            ";",
            "\"a\nb\"",
            "\"q\"",
            "/* c\r\nd */",
            "// c\r",
            "// c\n",
            "0x1F",
            "1.5e3",
            "'",
            "\"",
        ])
    }

    type SpanFields = (u32, u32, u32, u32, u32, u32);

    fn span_of((a, b, c, d, e, f): SpanFields) -> Span {
        Span {
            start: a,
            end: b,
            start_line: c,
            start_col: d,
            end_line: e,
            end_col: f,
        }
    }

    fn fields() -> impl Strategy<Value = SpanFields> {
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        )
    }

    proptest! {
        #[test]
        fn generated_text_round_trips(pieces in prop::collection::vec(piece(), 0..60)) {
            check(&pieces.concat());
        }

        /// Any `u32` span fields, including inverted and overlapping ones
        /// (which `validate_spans` rejects before a write but the codec can
        /// still represent), survive exactly.
        #[test]
        fn arbitrary_spans_round_trip(
            syms in prop::collection::vec((any::<u64>(), 0usize..7, prop::option::of(any::<u64>()), fields(), any::<prop::sample::Index>(), any::<bool>()), 0..8),
            toks in prop::collection::vec((any::<u64>(), 0usize..7, fields(), any::<prop::sample::Index>(), any::<bool>()), 0..8),
        ) {
            let mut st = Stream::default();
            for (i, (name, kind, lang_kind, f, pidx, has_parent)) in syms.iter().enumerate() {
                st.symbols.push(SymRec {
                    name: *name,
                    kind: KINDS[*kind],
                    lang_kind: *lang_kind,
                    parent: (i > 0 && *has_parent).then(|| pidx.index(i) as u32),
                    span: span_of(*f),
                });
            }
            for (term, class, f, pidx, has_parent) in &toks {
                st.tokens.push(TokRec {
                    term: *term,
                    class: CLASSES[*class],
                    parent: (!syms.is_empty() && *has_parent).then(|| pidx.index(syms.len()) as u32),
                    span: span_of(*f),
                });
            }
            prop_assert_eq!(decode(&encode(&st)).unwrap(), st);
        }
    }
}
