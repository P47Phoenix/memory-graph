//! Process-wide read-path counters (read cache phase 0, ADR 0008, epic story 45).
//!
//! Relaxed atomic counters for the decode and transaction work a query does,
//! so the share of query time spent decoding can be measured before any read
//! cache is built. Counting is always on and costs one relaxed add per event.
//! Timing (cumulative nanoseconds inside the decodes) is off by default,
//! because `Instant::now` is not free on every platform; turn it on with
//! [`set_read_timing`] for a measurement run. Nothing here changes what any
//! query returns.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// A snapshot of the read-path counters since process start (or the last
/// [`reset_read_stats`]). Counters are read one by one with relaxed loads,
/// so a snapshot taken while queries run is approximate, never torn per
/// field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// Reverse-dictionary blocks decoded to resolve one term id.
    pub dict_block_decodes: u64,
    /// Term-text lookups answered from the per-query memo.
    pub dict_text_memo_hits: u64,
    /// Term-text lookups that went to the reverse dictionary.
    pub dict_text_memo_misses: u64,
    /// Stream headers decoded lazily (`codec::decode_lazy`).
    pub lazy_stream_decodes: u64,
    /// Symbol sections decoded (`Lazy::symbols`).
    pub symbol_section_decodes: u64,
    /// Whole streams decoded (`codec::decode`).
    pub full_stream_decodes: u64,
    /// Read transactions opened by `StoreRead` calls on a `V2Store`.
    pub read_txns: u64,
    /// Nanoseconds inside dictionary block decodes (timing on only).
    pub dict_decode_nanos: u64,
    /// Nanoseconds inside lazy header and symbol-section decodes (timing on only).
    pub lazy_decode_nanos: u64,
    /// Nanoseconds inside full stream decodes (timing on only).
    pub full_decode_nanos: u64,
}

impl ReadStats {
    /// Field-wise `self - earlier`, saturating, for the work between two
    /// snapshots.
    pub fn since(&self, earlier: &ReadStats) -> ReadStats {
        ReadStats {
            dict_block_decodes: self
                .dict_block_decodes
                .saturating_sub(earlier.dict_block_decodes),
            dict_text_memo_hits: self
                .dict_text_memo_hits
                .saturating_sub(earlier.dict_text_memo_hits),
            dict_text_memo_misses: self
                .dict_text_memo_misses
                .saturating_sub(earlier.dict_text_memo_misses),
            lazy_stream_decodes: self
                .lazy_stream_decodes
                .saturating_sub(earlier.lazy_stream_decodes),
            symbol_section_decodes: self
                .symbol_section_decodes
                .saturating_sub(earlier.symbol_section_decodes),
            full_stream_decodes: self
                .full_stream_decodes
                .saturating_sub(earlier.full_stream_decodes),
            read_txns: self.read_txns.saturating_sub(earlier.read_txns),
            dict_decode_nanos: self
                .dict_decode_nanos
                .saturating_sub(earlier.dict_decode_nanos),
            lazy_decode_nanos: self
                .lazy_decode_nanos
                .saturating_sub(earlier.lazy_decode_nanos),
            full_decode_nanos: self
                .full_decode_nanos
                .saturating_sub(earlier.full_decode_nanos),
        }
    }

    /// Total nanoseconds measured inside decodes (zero unless timing is on).
    pub fn decode_nanos(&self) -> u64 {
        self.dict_decode_nanos + self.lazy_decode_nanos + self.full_decode_nanos
    }
}

pub(crate) static DICT_BLOCK_DECODES: AtomicU64 = AtomicU64::new(0);
pub(crate) static DICT_MEMO_HITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DICT_MEMO_MISSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static LAZY_DECODES: AtomicU64 = AtomicU64::new(0);
pub(crate) static SYMBOL_DECODES: AtomicU64 = AtomicU64::new(0);
pub(crate) static FULL_DECODES: AtomicU64 = AtomicU64::new(0);
pub(crate) static READ_TXNS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DICT_NANOS: AtomicU64 = AtomicU64::new(0);
pub(crate) static LAZY_NANOS: AtomicU64 = AtomicU64::new(0);
pub(crate) static FULL_NANOS: AtomicU64 = AtomicU64::new(0);

static TIMING: AtomicBool = AtomicBool::new(false);

const ALL: [&AtomicU64; 10] = [
    &DICT_BLOCK_DECODES,
    &DICT_MEMO_HITS,
    &DICT_MEMO_MISSES,
    &LAZY_DECODES,
    &SYMBOL_DECODES,
    &FULL_DECODES,
    &READ_TXNS,
    &DICT_NANOS,
    &LAZY_NANOS,
    &FULL_NANOS,
];

/// The current read-path counters.
pub fn read_stats() -> ReadStats {
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    ReadStats {
        dict_block_decodes: get(&DICT_BLOCK_DECODES),
        dict_text_memo_hits: get(&DICT_MEMO_HITS),
        dict_text_memo_misses: get(&DICT_MEMO_MISSES),
        lazy_stream_decodes: get(&LAZY_DECODES),
        symbol_section_decodes: get(&SYMBOL_DECODES),
        full_stream_decodes: get(&FULL_DECODES),
        read_txns: get(&READ_TXNS),
        dict_decode_nanos: get(&DICT_NANOS),
        lazy_decode_nanos: get(&LAZY_NANOS),
        full_decode_nanos: get(&FULL_NANOS),
    }
}

/// Zero every counter. Racy against concurrent queries; meant for
/// benchmarks, which should prefer [`ReadStats::since`].
pub fn reset_read_stats() {
    for c in ALL {
        c.store(0, Ordering::Relaxed);
    }
}

/// Turn decode timing on or off, process-wide. Off by default.
pub fn set_read_timing(on: bool) {
    TIMING.store(on, Ordering::Relaxed);
}

/// Count one event.
#[inline]
pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Count one event and, with timing on, add the time `f` takes to `nanos`.
#[inline]
pub(crate) fn timed<T>(counter: &AtomicU64, nanos: &AtomicU64, f: impl FnOnce() -> T) -> T {
    bump(counter);
    if !TIMING.load(Ordering::Relaxed) {
        return f();
    }
    let start = Instant::now();
    let out = f();
    let spent = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    nanos.fetch_add(spent, Ordering::Relaxed);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_field_wise_and_saturates() {
        let later = ReadStats {
            read_txns: 5,
            lazy_decode_nanos: 10,
            ..Default::default()
        };
        let earlier = ReadStats {
            read_txns: 2,
            full_stream_decodes: 1,
            ..Default::default()
        };
        let d = later.since(&earlier);
        assert_eq!(d.read_txns, 3);
        assert_eq!(d.full_stream_decodes, 0);
        assert_eq!(d.decode_nanos(), 10);
    }
}
