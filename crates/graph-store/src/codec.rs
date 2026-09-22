//! Compact per-file stream codec for the v2 store (ADR 0003 story 2, and story
//! 3's per-symbol token ranges).
//!
//! One file's symbols and tokens become one byte string. Texts are dictionary
//! ids (the store interns them), spans are delta-coded against the previous
//! record, integers are LEB128 varints, signed deltas are zigzag. Every span
//! field is stored, so any `u32` span survives exactly, including agent-supplied
//! spans that a source scan could not reproduce (so no `irregular` escape is
//! needed in this slice). Byte 0 is [`STREAM_FORMAT`].
//!
//! Layout (after the format byte, format 3): `nsym ntok symlen` as varints,
//! then one flag byte, then `symlen` bytes of symbol records, then `nck`
//! checkpoints, then `ntok` token records:
//!
//! * flag byte: bit 0 is `ranges_dense` (see [`Lazy::ranges_dense`]); the
//!   other bits are reserved and must be zero.
//! * symbol: `name_id kind lang_kind+1 parent+1 span tok_len_plus1
//!   [tok_first_delta]`. `tok_len_plus1` is a varint: `0` means the symbol
//!   transitively contains no tokens (no `tok_first_delta` follows, and the
//!   delta base below is left unchanged by this record); otherwise
//!   `last = first + (tok_len_plus1 - 1)` and `tok_first_delta` is a zigzag
//!   varint of `first - prev_first`, delta-coded against the `first` of the
//!   most recent symbol record that had a nonempty range (zero for the first
//!   one). `tok_len_plus1` is read before `tok_first_delta` (not the field
//!   order named above) because the decoder must know whether a delta
//!   follows before it can read one. The range is TRANSITIVE: every token
//!   ordinal under the symbol or any of its nested descendant symbols, not
//!   just its direct tokens (deriving direct-only ranges is slice 3i's job).
//! * token: `term_id class parent+1 span`
//! * span: zigzag deltas `start, len, start_line, start_col, end_line-start_line`
//!   then `end_col`, where `start`, `start_line` and `start_col` are relative
//!   to the previous record of the same section and `len = end - start`.
//! * checkpoint `j` (`nck = (ntok - 1) / CHECKPOINT_EVERY`, not stored) is the
//!   decoder state before token ordinal `(j + 1) * CHECKPOINT_EVERY`: zigzag
//!   deltas from checkpoint `j - 1` (zero for `j = 0`) of the byte offset of
//!   that record in the token section and of the previous record's `start`,
//!   `start_line` and `start_col`. With it a reader starts at a checkpoint
//!   and decodes at most `CHECKPOINT_EVERY - 1` records to reach an ordinal,
//!   instead of walking every record before it (ADR story 19).
//!
//! `parent` is the index of the enclosing symbol in the symbol section (0 =
//! the file itself). Records are in source order, so a token's index is its
//! ordinal. Format 2 (no flag byte, no per-symbol token range) is not read:
//! the v2 layout is unreleased and the store's schema version was bumped with
//! this change (ADR 0003 story 3, "Scoping decision (architecture board,
//! 2026-09-22)"; re-verified before this change via `gh release list` and
//! `git tag -l`, both empty, and no CHANGELOG exists -- v2 remains
//! unreleased).
//!
//! **Narrow-scope rule for embedding a derived field in this stream format**
//! (ADR 0003 story 3 scoping decision, condition 4/5): the per-symbol token
//! range below was embedded here, instead of a separate `derived_version`
//! table, because its access pattern (one field read alongside every
//! already-decoded symbol record) matches this format's existing per-record
//! layout. This is NOT a precedent: a future derived structure must
//! independently justify codec-embedding vs. a separate table on its own
//! access pattern, not cite this decision. See the story 3 row for the full
//! scoping decision and its five conditions.
use crate::StoreError;
use graph_core::{Span, SymbolKind, TokenClass};

/// Version byte of the stream layout. Bump on any layout change.
pub const STREAM_FORMAT: u8 = 3;

/// A checkpoint is written before every this-many-th token record.
pub const CHECKPOINT_EVERY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymRec {
    pub name: u64,
    pub kind: SymbolKind,
    pub lang_kind: Option<u64>,
    pub parent: Option<u32>,
    pub span: Span,
    /// Inclusive `(first, last)` token ordinal transitively under this
    /// symbol, or `None` if it transitively contains no tokens. On
    /// [`encode`], this field is ignored -- the true range is always
    /// recomputed from `Stream::tokens`' `parent` chains (one O(ntok) pass,
    /// see [`compute_ranges`]) -- so callers building a `Stream` to encode
    /// need not set it. On decode it carries the value actually stored.
    pub toks: Option<(u32, u32)>,
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

#[cfg(test)]
thread_local! {
    /// Token records decoded on this thread (test instrumentation).
    pub(crate) static RECORDS_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The true `(first, last)` inclusive token-ordinal range transitively under
/// each symbol (by index), and whether every symbol's actual transitive
/// token set is exactly the contiguous range `[first, last]` (`false` when
/// out-of-order or agent-supplied tokens break contiguity; see the codec's
/// module docs). One pass over `tokens`, walking each token's `parent` chain
/// up through `symbols[..].parent` to update every enclosing ancestor: O(ntok)
/// for typical (shallow) nesting, O(ntok * depth) in the worst case.
fn compute_ranges(symbols: &[SymRec], tokens: &[TokRec]) -> (Vec<Option<(u32, u32)>>, bool) {
    let n = symbols.len();
    let mut first: Vec<Option<u32>> = vec![None; n];
    let mut last: Vec<Option<u32>> = vec![None; n];
    let mut count: Vec<u64> = vec![0; n];
    for (ord, t) in tokens.iter().enumerate() {
        let ord = ord as u32;
        let mut cur = t.parent;
        // A well-formed tree has each `parent` an earlier (lower-index)
        // symbol, so at most `n` ancestors. `encode` is also called on
        // deliberately malformed test fixtures (self- or forward-referencing
        // parents) that `decode` later rejects; bound the ascent by `n`
        // steps so a parent cycle can never spin this loop forever.
        for _ in 0..n {
            let Some(idx) = cur else { break };
            let i = idx as usize;
            let Some(sym) = symbols.get(i) else { break };
            first[i] = Some(first[i].map_or(ord, |f| f.min(ord)));
            last[i] = Some(last[i].map_or(ord, |l| l.max(ord)));
            count[i] += 1;
            cur = sym.parent;
        }
    }
    let mut dense = true;
    let mut ranges = Vec::with_capacity(n);
    for i in 0..n {
        match (first[i], last[i]) {
            (Some(f), Some(l)) => {
                if count[i] != u64::from(l - f) + 1 {
                    dense = false;
                }
                ranges.push(Some((f, l)));
            }
            _ => ranges.push(None),
        }
    }
    (ranges, dense)
}

/// Decoder state before a token record.
#[derive(Clone, Copy, Default)]
struct Ck {
    off: usize,
    start: u32,
    line: u32,
    col: u32,
}

pub fn encode(st: &Stream) -> Vec<u8> {
    let (ranges, ranges_dense) = compute_ranges(&st.symbols, &st.tokens);
    let mut syms = Vec::new();
    let mut prev = ZERO;
    let mut prev_first = 0u32;
    for (s, range) in st.symbols.iter().zip(&ranges) {
        put_varint(&mut syms, s.name);
        syms.push(KINDS.iter().position(|k| *k == s.kind).unwrap_or(6) as u8);
        put_varint(&mut syms, s.lang_kind.map_or(0, |k| k + 1));
        put_varint(&mut syms, s.parent.map_or(0, |p| u64::from(p) + 1));
        put_span(&mut syms, &s.span, &prev);
        prev = s.span;
        match *range {
            None => put_varint(&mut syms, 0),
            Some((first, last)) => {
                put_varint(&mut syms, u64::from(last - first) + 1);
                put_varint(&mut syms, diff(first, prev_first));
                prev_first = first;
            }
        }
    }
    let mut cks = Vec::new();
    let mut toks = Vec::new();
    let mut prev = ZERO;
    let mut last = Ck::default();
    for (i, t) in st.tokens.iter().enumerate() {
        if i > 0 && i % CHECKPOINT_EVERY == 0 {
            let ck = Ck {
                off: toks.len(),
                start: prev.start,
                line: prev.start_line,
                col: prev.start_col,
            };
            put_varint(&mut cks, (ck.off - last.off) as u64);
            put_varint(&mut cks, diff(ck.start, last.start));
            put_varint(&mut cks, diff(ck.line, last.line));
            put_varint(&mut cks, diff(ck.col, last.col));
            last = ck;
        }
        put_varint(&mut toks, t.term);
        toks.push(CLASSES.iter().position(|c| *c == t.class).unwrap_or(6) as u8);
        put_varint(&mut toks, t.parent.map_or(0, |p| u64::from(p) + 1));
        put_span(&mut toks, &t.span, &prev);
        prev = t.span;
    }
    let mut out = vec![STREAM_FORMAT];
    put_varint(&mut out, st.symbols.len() as u64);
    put_varint(&mut out, st.tokens.len() as u64);
    put_varint(&mut out, syms.len() as u64);
    out.push(u8::from(ranges_dense));
    out.extend_from_slice(&syms);
    out.extend_from_slice(&cks);
    out.extend_from_slice(&toks);
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

/// A parsed stream header: symbols and tokens are decoded on demand.
/// [`Lazy::tokens`] walks every token record, [`Lazy::tokens_at`] reads chosen
/// ordinals through the checkpoints. Symbol search and term search use this to
/// avoid materializing every record.
pub struct Lazy<'a> {
    nsym: usize,
    ntok: usize,
    ranges_dense: bool,
    sym_bytes: &'a [u8],
    cks: Vec<Ck>,
    toks: &'a [u8],
}

pub fn decode_lazy(b: &[u8]) -> Result<Lazy<'_>, StoreError> {
    let mut r = Reader { b, at: 0 };
    let fmt = r.byte()?;
    if fmt != STREAM_FORMAT {
        return Err(bad(&format!(
            "unknown or old stream format byte {fmt} (expected {STREAM_FORMAT}); \
             the v2 store's on-disk layout changed and old v2 files must be re-indexed"
        )));
    }
    let nsym = r.varint()? as usize;
    let ntok = r.varint()? as usize;
    let symlen = r.varint()? as usize;
    let ranges_dense = r.byte()? & 1 != 0;
    // Every record is several bytes, so a count beyond the input is corrupt
    // (and must not drive a huge allocation).
    if nsym.saturating_add(ntok) > b.len() {
        return Err(bad("record count exceeds input"));
    }
    let sym_end =
        r.at.checked_add(symlen)
            .filter(|&e| e <= b.len())
            .ok_or_else(|| bad("symbol section exceeds input"))?;
    let sym_bytes = &b[r.at..sym_end];
    r.at = sym_end;
    let nck = ntok.saturating_sub(1) / CHECKPOINT_EVERY;
    if nck > b.len() {
        return Err(bad("checkpoint count exceeds input"));
    }
    let mut cks = Vec::with_capacity(nck);
    let mut last = Ck::default();
    for _ in 0..nck {
        let off = usize::try_from(r.varint()?)
            .ok()
            .and_then(|d| last.off.checked_add(d))
            .ok_or_else(|| bad("checkpoint offset overflow"))?;
        last = Ck {
            off,
            start: r.rel(last.start)?,
            line: r.rel(last.line)?,
            col: r.rel(last.col)?,
        };
        cks.push(last);
    }
    let toks = &b[r.at..];
    if cks.last().is_some_and(|c| c.off >= toks.len()) {
        return Err(bad("checkpoint offset out of range"));
    }
    Ok(Lazy {
        nsym,
        ntok,
        ranges_dense,
        sym_bytes,
        cks,
        toks,
    })
}

impl Lazy<'_> {
    /// Number of symbol records, without decoding them.
    pub fn nsym(&self) -> usize {
        self.nsym
    }

    /// Number of token records, without decoding them.
    pub fn ntok(&self) -> usize {
        self.ntok
    }

    /// Whether every symbol's stored `toks` range is exactly the contiguous
    /// set of token ordinals transitively under it (set at encode time; see
    /// [`compute_ranges`]). When `false`, `toks` on every symbol is still the
    /// true min/max ordinal, not garbage, but a reader that needs the exact
    /// set must fall back to a full scan (not built in this slice -- ADR 0003
    /// story 3, slice 3i).
    pub fn ranges_dense(&self) -> bool {
        self.ranges_dense
    }

    /// The symbol section, decoded.
    pub fn symbols(&self) -> Result<Vec<SymRec>, StoreError> {
        let mut r = Reader {
            b: self.sym_bytes,
            at: 0,
        };
        let mut symbols = Vec::with_capacity(self.nsym);
        let mut prev = ZERO;
        let mut prev_first = 0u32;
        for i in 0..self.nsym {
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
            let tok_len_plus1 = r.varint()?;
            let toks = if tok_len_plus1 == 0 {
                None
            } else {
                let first = r.rel(prev_first)?;
                prev_first = first;
                let len_minus1 = u32::try_from(tok_len_plus1 - 1)
                    .map_err(|_| bad("token range length out of range"))?;
                let last = first
                    .checked_add(len_minus1)
                    .ok_or_else(|| bad("token range overflow"))?;
                Some((first, last))
            };
            symbols.push(SymRec {
                name,
                kind,
                lang_kind,
                parent,
                span,
                toks,
            });
        }
        if r.at != r.b.len() {
            return Err(bad("trailing bytes in symbol section"));
        }
        Ok(symbols)
    }

    fn record(&self, r: &mut Reader<'_>, prev: &Span) -> Result<TokRec, StoreError> {
        #[cfg(test)]
        RECORDS_DECODED.with(|c| c.set(c.get() + 1));
        let term = r.varint()?;
        let class = *CLASSES
            .get(usize::from(r.byte()?))
            .ok_or_else(|| bad("bad token class"))?;
        let parent = r.parent()?;
        if parent.is_some_and(|p| p as usize >= self.nsym) {
            return Err(bad("parent index out of range"));
        }
        let span = r.span(prev)?;
        Ok(TokRec {
            term,
            class,
            parent,
            span,
        })
    }

    /// Visit every token record in ordinal order without collecting them.
    /// Stops at the first error or when `f` returns `false`; a full pass also
    /// verifies the checkpoints and checks for trailing bytes.
    pub fn tokens(&self, mut f: impl FnMut(usize, &TokRec) -> bool) -> Result<(), StoreError> {
        let mut r = Reader {
            b: self.toks,
            at: 0,
        };
        let mut prev = ZERO;
        for ord in 0..self.ntok {
            if ord > 0 && ord % CHECKPOINT_EVERY == 0 {
                let c = self.cks[ord / CHECKPOINT_EVERY - 1];
                if (c.off, c.start, c.line, c.col)
                    != (r.at, prev.start, prev.start_line, prev.start_col)
                {
                    return Err(bad("checkpoint mismatch"));
                }
            }
            let rec = self.record(&mut r, &prev)?;
            prev = rec.span;
            if !f(ord, &rec) {
                return Ok(());
            }
        }
        if r.at != r.b.len() {
            return Err(bad("trailing bytes"));
        }
        Ok(())
    }

    /// Visit the token records at the given strictly ascending ordinals. Each
    /// one starts from the nearest checkpoint at or before it (or continues
    /// from the previous ordinal when that is closer), so a lookup decodes at
    /// most `CHECKPOINT_EVERY - 1` records before its target. The checkpoints
    /// are trusted here; a full [`Lazy::tokens`] pass verifies them. An
    /// ordinal at or past the token count is an error.
    pub fn tokens_at(
        &self,
        ords: &[usize],
        mut f: impl FnMut(usize, &TokRec),
    ) -> Result<(), StoreError> {
        let mut r = Reader {
            b: self.toks,
            at: 0,
        };
        let mut prev = ZERO;
        // Ordinal of the next undecoded record.
        let mut next = 0usize;
        let mut last: Option<usize> = None;
        for &ord in ords {
            if ord >= self.ntok || last.is_some_and(|l| ord <= l) {
                return Err(bad("token ordinal out of order or range"));
            }
            last = Some(ord);
            let block = ord / CHECKPOINT_EVERY;
            if block * CHECKPOINT_EVERY > next {
                let c = self.cks[block - 1];
                r.at = c.off;
                prev = Span {
                    start: c.start,
                    start_line: c.line,
                    start_col: c.col,
                    ..ZERO
                };
                next = block * CHECKPOINT_EVERY;
            }
            while next < ord {
                prev = self.record(&mut r, &prev)?.span;
                next += 1;
            }
            let rec = self.record(&mut r, &prev)?;
            prev = rec.span;
            next += 1;
            f(ord, &rec);
        }
        Ok(())
    }
}

/// Posting value of `(term, file)`: the number of occurrences, then the
/// token ordinals in ascending order as gaps (the first is absolute).
pub fn encode_posting(ords: &[usize]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ords.len() + 2);
    put_varint(&mut out, ords.len() as u64);
    let mut prev = 0;
    for &o in ords {
        put_varint(&mut out, (o - prev) as u64);
        prev = o;
    }
    out
}

/// Occurrence count of a posting value.
pub fn posting_count(b: &[u8]) -> Result<usize, StoreError> {
    let n = Reader { b, at: 0 }.varint()?;
    usize::try_from(n).map_err(|_| bad("posting count out of range"))
}

/// The ordinals of a posting value, strictly ascending.
pub fn posting_ordinals(b: &[u8]) -> Result<Vec<usize>, StoreError> {
    let mut r = Reader { b, at: 0 };
    let n = usize::try_from(r.varint()?).map_err(|_| bad("posting count out of range"))?;
    if n > b.len() {
        return Err(bad("posting count exceeds input"));
    }
    let mut out = Vec::with_capacity(n);
    let mut prev = 0usize;
    for i in 0..n {
        let gap = usize::try_from(r.varint()?).map_err(|_| bad("posting gap out of range"))?;
        if i > 0 && gap == 0 {
            return Err(bad("posting ordinals not ascending"));
        }
        prev = prev
            .checked_add(gap)
            .ok_or_else(|| bad("posting ordinal overflow"))?;
        out.push(prev);
    }
    if r.at != b.len() {
        return Err(bad("trailing bytes in posting"));
    }
    Ok(out)
}

pub fn decode(b: &[u8]) -> Result<Stream, StoreError> {
    let lazy = decode_lazy(b)?;
    let symbols = lazy.symbols()?;
    let mut tokens = Vec::with_capacity(lazy.ntok);
    lazy.tokens(|_, t| {
        tokens.push(t.clone());
        true
    })?;
    // Independent check (ADR 0003 story 3 slice 3h, board condition 2): when
    // the stream claims its ranges are dense, re-derive every symbol's range
    // from the tokens actually decoded above and confirm it matches what was
    // stored, instead of trusting the encode-time gate alone. Debug-only: a
    // full re-derivation on every decode is not something a release build
    // should pay for, and a mismatch here is a codec bug, not bad input, so
    // it belongs behind `debug_assert!`, not a returned `StoreError`.
    #[cfg(debug_assertions)]
    if lazy.ranges_dense() {
        let (want, _) = compute_ranges(&symbols, &tokens);
        for (i, (sym, w)) in symbols.iter().zip(&want).enumerate() {
            debug_assert_eq!(
                sym.toks, *w,
                "symbol {i}: stored dense range {:?} does not match the range \
                 re-derived from the decoded tokens {:?}",
                sym.toks, w
            );
        }
    }
    Ok(Stream { symbols, tokens })
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

    /// Fills every symbol's `toks` with the range [`compute_ranges`] derives
    /// from `s.tokens`, so `s` is a valid expected value to compare a decoded
    /// stream against (`encode` ignores whatever `toks` was set to on input).
    fn with_ranges(mut s: Stream) -> Stream {
        let (ranges, _) = compute_ranges(&s.symbols, &s.tokens);
        for (sym, r) in s.symbols.iter_mut().zip(ranges) {
            sym.toks = r;
        }
        s
    }

    fn sample() -> Stream {
        with_ranges(Stream {
            symbols: vec![
                SymRec {
                    name: 0,
                    kind: SymbolKind::Type,
                    lang_kind: Some(1),
                    parent: None,
                    span: sp(0, 20, 1, 1, 3, 2),
                    toks: None,
                },
                SymRec {
                    name: 2,
                    kind: SymbolKind::Method,
                    lang_kind: None,
                    parent: Some(0),
                    span: sp(4, 12, 2, 5, 2, 13),
                    toks: None,
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
        })
    }

    /// Golden bytes: a change here is a format change and needs a bumped
    /// `STREAM_FORMAT` plus a migration story.
    #[test]
    fn golden_bytes() {
        let want: Vec<u8> = vec![
            3, 2, 2, 24, 1, // format, nsym, ntok, symlen, flag (ranges_dense)
            0, 1, 2, 0, 0, 40, 2, 2, 4, 2, 1,
            2, // symbol 0 (+ tok_len_plus1, tok_first_delta)
            2, 3, 0, 1, 8, 16, 2, 8, 0, 13, 1, 0, // symbol 1: deltas from symbol 0
            3, 1, 0, 0, 4, 2, 2, 0, 3, // token 0
            4, 0, 2, 10, 2, 2, 10, 0, 7, // token 1: deltas from token 0
        ];
        assert_eq!(encode(&sample()), want);
        assert!(decode_lazy(&want).unwrap().ranges_dense());
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
        let mut b = vec![3, 0, 1, 0, 0]; // format, nsym, ntok, symlen, flag
        b.extend([0xff; 9]);
        b.push(0x02);
        b.extend([0; 8]); // class, parent, six span fields
        assert!(decode(&b).is_err());
        // u64::MAX (tenth byte 0x01) is fine.
        let mut ok = vec![3, 0, 1, 0, 0];
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
        let want = with_ranges(s.clone());
        assert_eq!(decode(&encode(&s)).unwrap(), want);
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

    /// A stream of `n` tokens with irregular spans, a symbol, and some
    /// terms repeated so postings have several ordinals.
    fn long(n: usize) -> Stream {
        let mut s = sample();
        s.tokens.clear();
        let mut at = 0u32;
        for i in 0..n {
            let len = 1 + (i % 7) as u32;
            at += 1 + (i % 5) as u32 * 40;
            s.tokens.push(TokRec {
                term: (i % 11) as u64,
                class: CLASSES[i % 7],
                parent: (i % 3 == 0).then_some((i % 2) as u32),
                span: sp(
                    at,
                    at + len,
                    1 + at / 80,
                    at % 80,
                    1 + at / 80,
                    at % 80 + len,
                ),
            });
        }
        with_ranges(s)
    }

    #[test]
    fn tokens_at_matches_full_decode_across_checkpoints() {
        for n in [
            0,
            1,
            CHECKPOINT_EVERY - 1,
            CHECKPOINT_EVERY,
            CHECKPOINT_EVERY + 1,
            3 * CHECKPOINT_EVERY,
            3 * CHECKPOINT_EVERY + 5,
            500,
        ] {
            let s = long(n);
            let b = encode(&s);
            assert_eq!(decode(&b).unwrap(), s, "n={n}");
            let lazy = decode_lazy(&b).unwrap();
            assert_eq!(lazy.symbols().unwrap(), s.symbols);
            // Every ordinal alone, and several ascending selections.
            let all: Vec<usize> = (0..n).collect();
            let sel: [Vec<usize>; 4] = [
                all.clone(),
                all.iter().copied().step_by(7).collect(),
                all.iter()
                    .copied()
                    .filter(|o| o % CHECKPOINT_EVERY == 0)
                    .collect(),
                all.iter()
                    .copied()
                    .filter(|o| (o + 1) % CHECKPOINT_EVERY == 0)
                    .collect(),
            ];
            for ords in sel
                .iter()
                .chain(all.iter().map(|&o| vec![o]).collect::<Vec<_>>().iter())
            {
                let mut got = Vec::new();
                lazy.tokens_at(ords, |o, t| got.push((o, t.clone())))
                    .unwrap();
                let want: Vec<_> = ords.iter().map(|&o| (o, s.tokens[o].clone())).collect();
                assert_eq!(got, want, "n={n} ords={ords:?}");
            }
            // Out of range and out of order are errors.
            assert!(lazy.tokens_at(&[n], |_, _| {}).is_err());
            if n > 1 {
                assert!(lazy.tokens_at(&[1, 0], |_, _| {}).is_err());
                assert!(lazy.tokens_at(&[1, 1], |_, _| {}).is_err());
            }
        }
    }

    #[test]
    fn checkpoints_are_written_and_verified() {
        let s = long(3 * CHECKPOINT_EVERY);
        let b = encode(&s);
        // Header: fmt nsym ntok(2 bytes, 192) symlen flag, symbols, then checkpoints.
        let lazy = decode_lazy(&b).unwrap();
        assert_eq!(lazy.cks.len(), 2);
        // Corrupt one checkpoint field: a full decode must notice.
        let sym_end = 1 + 1 + 2 + 1 + 1 + lazy.sym_bytes.len();
        let mut bad_b = b.clone();
        bad_b[sym_end + 1] ^= 1; // first checkpoint's start delta
        assert!(decode(&bad_b).is_err());
        // A stream with a single record block has no checkpoints.
        assert!(decode_lazy(&encode(&long(CHECKPOINT_EVERY)))
            .unwrap()
            .cks
            .is_empty());
    }

    /// Records decoded to visit `ords`; also asserts that exactly those
    /// ordinals came back, in order, with the records of the source stream.
    fn decoded_by(lazy: &Lazy<'_>, want: &Stream, ords: &[usize]) -> usize {
        RECORDS_DECODED.with(|c| c.set(0));
        let mut got = Vec::new();
        lazy.tokens_at(ords, |o, t| got.push((o, t.clone())))
            .unwrap();
        let n = RECORDS_DECODED.with(|c| c.get());
        let exp: Vec<_> = ords.iter().map(|&o| (o, want.tokens[o].clone())).collect();
        assert_eq!(got, exp, "records returned for {ords:?}");
        n
    }

    /// `tokens_at` must use the checkpoints (bounded work per lookup) and
    /// must not re-decode when continuing forward.
    #[test]
    fn tokens_at_decodes_a_bounded_number_of_records() {
        let src = long(3 * CHECKPOINT_EVERY + 5);
        let b = encode(&src);
        let lazy = decode_lazy(&b).unwrap();
        let ce = CHECKPOINT_EVERY;
        // One lookup: jump to its checkpoint, decode the gap plus the target.
        assert_eq!(decoded_by(&lazy, &src, &[2 * ce + 10]), 11);
        assert_eq!(decoded_by(&lazy, &src, &[2 * ce]), 1);
        assert_eq!(decoded_by(&lazy, &src, &[2 * ce - 1]), ce);
        assert_eq!(decoded_by(&lazy, &src, &[3 * ce + 4]), 5);
        for o in 0..3 * ce + 5 {
            assert!(decoded_by(&lazy, &src, &[o]) <= ce, "ordinal {o}");
        }
        // Ascending lookups in one block continue: no re-decode, no jump back.
        assert_eq!(decoded_by(&lazy, &src, &[ce + 3, ce + 4, ce + 9]), 10);
        // A far second lookup jumps instead of walking the gap.
        assert_eq!(decoded_by(&lazy, &src, &[1, 3 * ce + 2]), 2 + 3);
        // A full ascending pass decodes each record exactly once.
        let all: Vec<usize> = (0..3 * ce + 5).collect();
        assert_eq!(decoded_by(&lazy, &src, &all), 3 * ce + 5);
    }

    /// Every checkpoint field (offset, start, line, column) is verified by a
    /// full pass, each on its own.
    #[test]
    fn every_checkpoint_field_is_verified() {
        let b = encode(&long(2 * CHECKPOINT_EVERY));
        let good = decode_lazy(&b).unwrap();
        assert_eq!(good.cks.len(), 1);
        let run = |f: &dyn Fn(&mut Ck)| {
            let mut l = decode_lazy(&b).unwrap();
            f(&mut l.cks[0]);
            l.tokens(|_, _| true)
        };
        assert!(run(&|_| {}).is_ok());
        assert!(run(&|c| c.off -= 1).is_err(), "off");
        assert!(run(&|c| c.start += 1).is_err(), "start");
        assert!(run(&|c| c.line += 1).is_err(), "line");
        assert!(run(&|c| c.col += 1).is_err(), "col");
    }

    /// A checkpoint stands before a token that exists, so its offset must lie
    /// strictly inside the token section: `off == toks.len()` is corrupt
    /// (kills `>=` -> `>`), `off == toks.len() - 1` is not rejected by the
    /// bound itself.
    #[test]
    fn checkpoint_offset_must_be_strictly_inside_the_token_section() {
        let b = encode(&long(CHECKPOINT_EVERY + 1));
        let lazy = decode_lazy(&b).unwrap();
        assert_eq!(lazy.cks.len(), 1);
        let toks_len = lazy.toks.len();
        let sym_end = 5 + lazy.sym_bytes.len(); // fmt, nsym, ntok, symlen, flag (1 byte each)
                                                // Skip the original first varint (the offset) of the only checkpoint.
        let mut skip = sym_end;
        while b[skip] & 0x80 != 0 {
            skip += 1;
        }
        skip += 1;
        let forged = |off: usize| {
            let mut v = b[..sym_end].to_vec();
            put_varint(&mut v, off as u64);
            v.extend_from_slice(&b[skip..]);
            v
        };
        let bad_bytes = forged(toks_len);
        let at_end = decode_lazy(&bad_bytes);
        assert!(at_end.is_err(), "offset == section length must be rejected");
        // Just inside passes the bound (a full pass would still catch it).
        let ok_bytes = forged(toks_len - 1);
        assert!(decode_lazy(&ok_bytes).is_ok());
    }

    /// The checkpoint interval is part of the on-disk layout: pin it.
    #[test]
    fn checkpoint_interval_is_pinned() {
        assert_eq!(CHECKPOINT_EVERY, 64);
        for (n, nck) in [(1, 0), (64, 0), (65, 1), (128, 1), (129, 2), (193, 3)] {
            let b = encode(&long(n));
            assert_eq!(decode_lazy(&b).unwrap().cks.len(), nck, "n={n}");
        }
        // The first checkpoint sits exactly after the 64th record.
        let first_64 = encode(&long(64));
        let l64 = decode_lazy(&first_64).unwrap();
        let b65 = encode(&long(65));
        let l65 = decode_lazy(&b65).unwrap();
        assert_eq!(l65.cks[0].off, l64.toks.len());
    }

    /// A posting count larger than the input is rejected up front (no huge
    /// allocation, no panic), whatever the count.
    #[test]
    fn posting_ordinals_rejects_huge_counts() {
        let mut b = Vec::new();
        put(&mut b, u64::MAX >> 1);
        b.push(1);
        let e = posting_ordinals(&b).unwrap_err().to_string();
        assert!(e.contains("count exceeds input"), "{e}");
        // A count above the input length is caught by the guard; at the
        // length it passes the guard and fails on the truncated read.
        let e = posting_ordinals(&[5, 1, 1, 1]).unwrap_err().to_string();
        assert!(e.contains("count exceeds input"), "{e}");
        let e = posting_ordinals(&[4, 1, 1, 1]).unwrap_err().to_string();
        assert!(e.contains("truncated"), "{e}");
        // Exactly as many entries as bytes after the count is fine.
        assert_eq!(posting_ordinals(&[3, 1, 1, 1]).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn postings_round_trip() {
        for ords in [
            vec![],
            vec![0],
            vec![5],
            vec![0, 1, 2],
            vec![3, 70, 71, 5000],
        ] {
            let b = encode_posting(&ords);
            assert_eq!(posting_count(&b).unwrap(), ords.len());
            assert_eq!(posting_ordinals(&b).unwrap(), ords);
        }
        // Non-ascending (zero gap after the first), truncated and trailing.
        assert!(posting_ordinals(&[2, 3, 0]).is_err());
        assert!(posting_ordinals(&[2, 3]).is_err());
        assert!(posting_ordinals(&[1, 3, 0]).is_err());
        assert!(posting_ordinals(&[]).is_err());
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

    /// A hand-built format-2 (pre-slice-3h) byte string -- no flag byte, no
    /// per-symbol token range -- must be refused with a clear error, never
    /// silently misread as format 3 (ADR 0003 story 3 board condition 1: v2
    /// is unreleased, so this is a refuse-and-reindex bump, not a migration).
    #[test]
    fn old_format_2_streams_are_rejected_not_misread() {
        let old_format_2: Vec<u8> = vec![2, 0, 0, 0]; // fmt, nsym, ntok, symlen
        let err = decode(&old_format_2).unwrap_err().to_string();
        assert!(err.contains("format"), "{err}");
        let err = match decode_lazy(&old_format_2) {
            Ok(_) => panic!("old format-2 bytes must not decode"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("format"), "{err}");
    }

    /// The stored dense range is not the only thing correctness rests on:
    /// [`decode`]'s independent `debug_assert` re-derives every symbol's
    /// range from the tokens it just decoded and must fire when the stored
    /// range is wrong, proving the check is load-bearing, not decorative
    /// (ADR 0003 story 3 board condition 2). Debug builds only, since
    /// `debug_assert!` is a no-op in release.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "does not match")]
    fn debug_assert_catches_a_corrupted_dense_range() {
        let mut b = encode(&sample());
        // Byte 15 is symbol 0's `tok_len_plus1` (see `golden_bytes`'s layout);
        // corrupting it to a still-decodable-but-wrong length (1 -> 3) must
        // trip the independent decode-time check, not decode silently.
        assert_eq!(b[15], 1, "test assumes golden_bytes' byte layout");
        b[15] = 3;
        let _ = decode(&b);
    }

    /// <2% storage-growth gate (ADR 0003 story 3 board condition 3, test
    /// requirement 5): encodes this repo's own `crates/` corpus (the same
    /// corpus choice as `examples/churn.rs` and `examples/compact_churn.rs`)
    /// through the real Rust extractor and the v2 store's own
    /// symbol/token-merge logic (mirroring `V2Store::index_batch`'s
    /// open-symbol stack in `v2.rs`), then compares the new encoder's real
    /// byte count against a frozen replica of the pre-slice-3h (format 2)
    /// encoder on the identical streams. A real measurement, not a reported
    /// number: this `assert!` fails CI if the range fields blow the budget.
    #[test]
    fn range_fields_grow_encoded_size_by_under_two_percent_on_this_repos_corpus() {
        use graph_core::Extractor;
        use graph_lang_rust::RustExtractor;

        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(p) else {
                return;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    if !path.ends_with("target") && !path.ends_with(".git") {
                        walk(&path, out);
                    }
                } else if path.extension().is_some_and(|x| x == "rs") {
                    out.push(path);
                }
            }
        }

        /// Format-2 replica (pre-slice-3h): frozen copy of `encode` before
        /// the flag byte and per-symbol `tok_len_plus1`/`tok_first_delta`
        /// fields existed, so the delta below is real bytes, not a guess.
        fn old_encode(st: &Stream) -> Vec<u8> {
            let mut syms = Vec::new();
            let mut prev = ZERO;
            for s in &st.symbols {
                put_varint(&mut syms, s.name);
                syms.push(KINDS.iter().position(|k| *k == s.kind).unwrap_or(6) as u8);
                put_varint(&mut syms, s.lang_kind.map_or(0, |k| k + 1));
                put_varint(&mut syms, s.parent.map_or(0, |p| u64::from(p) + 1));
                put_span(&mut syms, &s.span, &prev);
                prev = s.span;
            }
            let mut cks = Vec::new();
            let mut toks = Vec::new();
            let mut prev = ZERO;
            let mut last = Ck::default();
            for (i, t) in st.tokens.iter().enumerate() {
                if i > 0 && i % CHECKPOINT_EVERY == 0 {
                    let ck = Ck {
                        off: toks.len(),
                        start: prev.start,
                        line: prev.start_line,
                        col: prev.start_col,
                    };
                    put_varint(&mut cks, (ck.off - last.off) as u64);
                    put_varint(&mut cks, diff(ck.start, last.start));
                    put_varint(&mut cks, diff(ck.line, last.line));
                    put_varint(&mut cks, diff(ck.col, last.col));
                    last = ck;
                }
                put_varint(&mut toks, t.term);
                toks.push(CLASSES.iter().position(|c| *c == t.class).unwrap_or(6) as u8);
                put_varint(&mut toks, t.parent.map_or(0, |p| u64::from(p) + 1));
                put_span(&mut toks, &t.span, &prev);
                prev = t.span;
            }
            let mut out = vec![2u8]; // old STREAM_FORMAT, frozen
            put_varint(&mut out, st.symbols.len() as u64);
            put_varint(&mut out, st.tokens.len() as u64);
            put_varint(&mut out, syms.len() as u64);
            out.extend_from_slice(&syms);
            out.extend_from_slice(&cks);
            out.extend_from_slice(&toks);
            out
        }

        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_crates = manifest
            .parent()
            .expect("graph-store's parent is crates/")
            .to_path_buf();
        let mut files = Vec::new();
        walk(&repo_crates, &mut files);
        assert!(
            files.len() > 10,
            "expected a real multi-file corpus under {}, found {}",
            repo_crates.display(),
            files.len()
        );

        let extractor = RustExtractor;
        let (mut old_total, mut new_total) = (0u64, 0u64);
        for path in &files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let ex = extractor.extract(&src);
            // Mirror `V2Store::index_batch`'s open-symbol stack (v2.rs): sort
            // symbols and tokens by position, then merge them, deriving each
            // record's `parent` from span containment.
            let mut syms: Vec<_> = ex.symbols.iter().collect();
            syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
            let mut toks: Vec<_> = ex.tokens.iter().collect();
            toks.sort_by_key(|t| t.span.start);
            let mut stream = Stream::default();
            let mut open: Vec<(u32, u32)> = Vec::new();
            let (mut si, mut ti) = (0, 0);
            while si < syms.len() || ti < toks.len() {
                let take_sym = si < syms.len()
                    && (ti >= toks.len() || syms[si].span.start <= toks[ti].span.start);
                let pos = if take_sym {
                    syms[si].span.start
                } else {
                    toks[ti].span.start
                };
                while open.last().is_some_and(|&(_, end)| end <= pos) {
                    open.pop();
                }
                let parent = open.last().map(|&(i, _)| i);
                if take_sym {
                    let s = syms[si];
                    si += 1;
                    let idx = stream.symbols.len();
                    stream.symbols.push(SymRec {
                        name: idx as u64,
                        kind: s.kind,
                        lang_kind: None,
                        parent,
                        span: s.span,
                        toks: None,
                    });
                    open.push((idx as u32, s.span.end));
                } else {
                    let t = toks[ti];
                    ti += 1;
                    stream.tokens.push(TokRec {
                        term: stream.tokens.len() as u64,
                        class: t.class,
                        parent,
                        span: t.span,
                    });
                }
            }
            old_total += old_encode(&stream).len() as u64;
            new_total += encode(&stream).len() as u64;
        }
        assert!(old_total > 0, "corpus produced no encoded bytes");
        let delta_pct = (new_total as f64 - old_total as f64) / old_total as f64 * 100.0;
        println!(
            "range-codec growth over {} files: old {old_total} B, new {new_total} B, \
             delta {delta_pct:.3}%",
            files.len()
        );
        assert!(
            delta_pct < 2.0,
            "encoded size grew {delta_pct:.3}% (old {old_total} B -> new {new_total} B \
             over {} files); must stay under 2% (ADR 0003 story 3 board condition 3)",
            files.len()
        );
    }

    /// A symbol with an empty transitive range (`tok_len_plus1 == 0`) has no
    /// token transitively under it, and a stream whose out-of-order/agent-
    /// supplied tokens break contiguity for a symbol is stored with
    /// `ranges_dense == false` but still the true min/max, not garbage.
    #[test]
    fn non_contiguous_ranges_are_flagged_not_dense_with_true_min_max() {
        // Symbol 0 spans tokens 0 and 2 (parent Some(0)); token 1 sits
        // between them but is NOT under symbol 0 (parent None), so symbol
        // 0's actual transitive set is {0, 2}, not the contiguous [0, 2].
        let mut s = Stream {
            symbols: vec![SymRec {
                name: 0,
                kind: SymbolKind::Function,
                lang_kind: None,
                parent: None,
                span: sp(0, 10, 1, 1, 1, 10),
                toks: None,
            }],
            tokens: vec![
                TokRec {
                    term: 0,
                    class: TokenClass::Identifier,
                    parent: Some(0),
                    span: sp(0, 1, 1, 1, 1, 2),
                },
                TokRec {
                    term: 1,
                    class: TokenClass::Identifier,
                    parent: None,
                    span: sp(2, 3, 1, 3, 1, 4),
                },
                TokRec {
                    term: 2,
                    class: TokenClass::Identifier,
                    parent: Some(0),
                    span: sp(4, 5, 1, 5, 1, 6),
                },
            ],
        };
        let b = encode(&s);
        let lazy = decode_lazy(&b).unwrap();
        assert!(!lazy.ranges_dense());
        let decoded = decode(&b).unwrap();
        assert_eq!(
            decoded.symbols[0].toks,
            Some((0, 2)),
            "true min/max, not garbage"
        );

        // A symbol whose `tok_len_plus1 == 0` (empty range) has no token
        // transitively under it: give symbol 0 no tokens at all.
        s.tokens.iter_mut().for_each(|t| t.parent = None);
        let b = encode(&s);
        assert!(decode_lazy(&b).unwrap().ranges_dense(), "vacuously dense");
        assert_eq!(decode(&b).unwrap().symbols[0].toks, None);
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
        /// still represent), survive exactly. Token parents are arbitrary
        /// indices (not built by any nesting stack), so this also exercises
        /// out-of-order and overlapping *token ranges*: whatever the actual
        /// transitive set under a symbol is, [`compute_ranges`] gives its
        /// true min/max and correctly flags `ranges_dense` when that set
        /// isn't the exact contiguous range -- checked here against the
        /// decoded stream, independently of whatever `encode` did internally.
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
                    toks: None,
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
            let (want_ranges, want_dense) = compute_ranges(&st.symbols, &st.tokens);
            let b = encode(&st);
            let lazy = decode_lazy(&b).unwrap();
            prop_assert_eq!(lazy.ranges_dense(), want_dense);
            let decoded = decode(&b).unwrap();
            let mut want = st;
            for (sym, r) in want.symbols.iter_mut().zip(&want_ranges) {
                sym.toks = *r;
            }
            prop_assert_eq!(&decoded, &want);
            // Range invariants (ADR 0003 story 3 slice 3h test requirement 2):
            // a symbol's range contains every ancestor's range, and an empty
            // range (`toks == None`) means no token is transitively under it.
            for (i, sym) in decoded.symbols.iter().enumerate() {
                prop_assert_eq!(sym.toks, want_ranges[i]);
                if let (Some(p), Some((f, l))) = (sym.parent, sym.toks) {
                    let (pf, pl) = decoded.symbols[p as usize]
                        .toks
                        .expect("a symbol with a nonempty range makes its parent's nonempty too");
                    prop_assert!(f >= pf && l <= pl);
                }
            }
        }

        /// A tree built by a nesting stack (mirroring how the v2 store's
        /// ingest builds `parent` links from span containment) always
        /// produces token ordinals that are contiguous under every symbol,
        /// so `ranges_dense` must be true and sibling ranges must be disjoint.
        #[test]
        fn well_nested_streams_are_dense_with_disjoint_sibling_ranges(
            instrs in prop::collection::vec(0u8..3, 0..80),
        ) {
            let mut st = Stream::default();
            let mut stack: Vec<u32> = Vec::new();
            for (k, instr) in instrs.iter().enumerate() {
                match instr % 3 {
                    0 => {
                        let idx = st.symbols.len() as u32;
                        st.symbols.push(SymRec {
                            name: idx as u64,
                            kind: SymbolKind::Other,
                            lang_kind: None,
                            parent: stack.last().copied(),
                            span: ZERO,
                            toks: None,
                        });
                        stack.push(idx);
                    }
                    1 => st.tokens.push(TokRec {
                        term: k as u64,
                        class: TokenClass::Other,
                        parent: stack.last().copied(),
                        span: ZERO,
                    }),
                    _ => {
                        stack.pop();
                    }
                }
            }
            let b = encode(&st);
            let lazy = decode_lazy(&b).unwrap();
            prop_assert!(lazy.ranges_dense());
            let decoded = decode(&b).unwrap();
            // Every pair of symbols is either ancestor/descendant (nested
            // stack) or unrelated; unrelated ranges (including siblings)
            // must not overlap.
            let is_ancestor = |mut i: usize, j: usize| -> bool {
                loop {
                    match decoded.symbols[i].parent {
                        Some(p) if p as usize == j => return true,
                        Some(p) => i = p as usize,
                        None => return false,
                    }
                }
            };
            for i in 0..decoded.symbols.len() {
                for j in (i + 1)..decoded.symbols.len() {
                    let (Some((fi, li)), Some((fj, lj))) =
                        (decoded.symbols[i].toks, decoded.symbols[j].toks)
                    else {
                        continue;
                    };
                    if is_ancestor(i, j) || is_ancestor(j, i) {
                        continue;
                    }
                    prop_assert!(li < fj || lj < fi, "unrelated symbols {i}, {j} overlap");
                }
            }
        }
    }
}
