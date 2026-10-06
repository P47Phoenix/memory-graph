//! Read-path counters (read cache phase 0, ADR 0008, epic story 45).
//!
//! Relaxed counters for the decode and transaction work a query does, so the
//! share of query time spent decoding can be measured before any read cache
//! is built. Counting is always on and costs one relaxed atomic add plus one
//! thread-local add per event. Nothing here changes what any query returns.
//!
//! Two views of the same events:
//!
//! - [`snapshot`]: **process-wide** totals. Every `V2Store` in the process
//!   (and every thread) adds to the same counters, so they are not
//!   attributable to one store; take the difference of two snapshots
//!   ([`ReadStats::since`]) around a workload that runs alone.
//! - [`thread_snapshot`]: the calling thread's own totals. A query runs on
//!   the thread that called it, so a test can measure exactly its own
//!   queries even while other tests run in parallel. If a single read is
//!   ever parallelised across threads, this view under-counts it: work done
//!   on helper threads lands in their counters, not the caller's.
//!
//! Only read-side work is counted: decodes done by writes (`dict_rev_append`
//! extending a dictionary block, removing a file's content, `vacuum`) are
//! not. Reads through a [`V2Snapshot`](crate::V2Snapshot) count their
//! decodes but not a read transaction, because the snapshot opened its
//! transaction once, up front.
//!
//! Timing (cumulative nanoseconds inside the decodes) is off by default,
//! because `Instant::now` is not free on every platform; [`set_timing`]
//! turns it on, process-wide, for a measurement run.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// A snapshot of the read-path counters since process start (or, for the
/// process-wide view, the last [`reset`]). Fields are read one by one with
/// relaxed loads, so a process-wide snapshot taken while queries run is
/// approximate, never torn per field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadStats {
    /// Reverse-dictionary blocks scanned by queries to resolve one term id
    /// (one per uncached term-text lookup; writes are not counted). Since
    /// read cache phase 1 a scan walks the encoded entries without decoding
    /// them, so this is a block visit, not a full decode.
    pub dict_block_decodes: u64,
    /// Term-text lookups answered from the per-query memo.
    pub dict_text_memo_hits: u64,
    /// Term-text lookups that went to the reverse dictionary.
    pub dict_text_memo_misses: u64,
    /// Stream headers decoded lazily by reads (`codec::decode_lazy`).
    pub lazy_stream_decodes: u64,
    /// Symbol sections decoded by reads (`Lazy::symbols`).
    pub symbol_section_decodes: u64,
    /// Whole streams decoded by reads (`codec::decode`), counted once, not
    /// also as a lazy and a symbol-section decode.
    pub full_stream_decodes: u64,
    /// Read transactions opened by `StoreRead` calls on a `V2Store` (not on a
    /// `V2Snapshot`, which holds one transaction for its whole life).
    pub read_txns: u64,
    /// Nanoseconds inside dictionary block decodes (timing on only).
    pub dict_decode_nanos: u64,
    /// Nanoseconds inside lazy header decodes (timing on only).
    pub lazy_decode_nanos: u64,
    /// Nanoseconds inside full stream decodes (timing on only).
    pub full_decode_nanos: u64,
    /// Dictionary strings allocated by query-side lookups: exactly one per
    /// uncached term-text lookup that finds its id (read cache phase 1).
    pub dict_strings_decoded: u64,
    /// `StoreRead` calls (queries) on a `V2Store` or a `V2Snapshot`.
    pub queries: u64,
    /// Wall nanoseconds inside `StoreRead` calls (timing on only).
    pub query_nanos: u64,
    /// Encoded bytes of the reverse-dictionary blocks scanned by queries.
    pub dict_bytes: u64,
    /// Encoded bytes of the symbol sections decoded by reads.
    pub symbol_bytes: u64,
    /// Encoded size of the streams whose header was decoded lazily (the whole
    /// stream, not just the header). Symbol bytes lie within it, so do not
    /// sum byte counters across kinds.
    pub lazy_bytes: u64,
    /// Encoded bytes of the streams decoded whole.
    pub full_bytes: u64,
    /// Nanoseconds inside symbol-section decodes (timing on only).
    pub symbol_decode_nanos: u64,
}

impl ReadStats {
    /// Field-wise `self - earlier`, saturating, for the work between two
    /// snapshots.
    pub fn since(&self, earlier: &ReadStats) -> ReadStats {
        let (a, b) = (self.to_array(), earlier.to_array());
        Self::from_array(std::array::from_fn(|i| a[i].saturating_sub(b[i])))
    }

    /// Total nanoseconds measured inside decodes (zero unless timing is on).
    pub fn decode_nanos(&self) -> u64 {
        self.dict_decode_nanos
            + self.lazy_decode_nanos
            + self.symbol_decode_nanos
            + self.full_decode_nanos
    }

    fn to_array(self) -> [u64; COUNTERS] {
        [
            self.dict_block_decodes,
            self.dict_text_memo_hits,
            self.dict_text_memo_misses,
            self.lazy_stream_decodes,
            self.symbol_section_decodes,
            self.full_stream_decodes,
            self.read_txns,
            self.dict_decode_nanos,
            self.lazy_decode_nanos,
            self.full_decode_nanos,
            self.dict_strings_decoded,
            self.queries,
            self.query_nanos,
            self.dict_bytes,
            self.symbol_bytes,
            self.lazy_bytes,
            self.full_bytes,
            self.symbol_decode_nanos,
        ]
    }

    fn from_array(v: [u64; COUNTERS]) -> Self {
        ReadStats {
            dict_block_decodes: v[Counter::DictBlockDecodes as usize],
            dict_text_memo_hits: v[Counter::DictMemoHits as usize],
            dict_text_memo_misses: v[Counter::DictMemoMisses as usize],
            lazy_stream_decodes: v[Counter::LazyDecodes as usize],
            symbol_section_decodes: v[Counter::SymbolDecodes as usize],
            full_stream_decodes: v[Counter::FullDecodes as usize],
            read_txns: v[Counter::ReadTxns as usize],
            dict_decode_nanos: v[Counter::DictNanos as usize],
            lazy_decode_nanos: v[Counter::LazyNanos as usize],
            full_decode_nanos: v[Counter::FullNanos as usize],
            dict_strings_decoded: v[Counter::DictStrings as usize],
            queries: v[Counter::QueryCount as usize],
            query_nanos: v[Counter::QueryNanos as usize],
            dict_bytes: v[Counter::DictBytes as usize],
            symbol_bytes: v[Counter::SymbolBytes as usize],
            lazy_bytes: v[Counter::LazyBytes as usize],
            full_bytes: v[Counter::FullBytes as usize],
            symbol_decode_nanos: v[Counter::SymbolNanos as usize],
        }
    }
}

/// One counter; the discriminant is its index in the counter arrays and in
/// [`ReadStats::to_array`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Counter {
    DictBlockDecodes,
    DictMemoHits,
    DictMemoMisses,
    LazyDecodes,
    SymbolDecodes,
    FullDecodes,
    ReadTxns,
    DictNanos,
    LazyNanos,
    FullNanos,
    DictStrings,
    QueryCount,
    QueryNanos,
    DictBytes,
    SymbolBytes,
    LazyBytes,
    FullBytes,
    SymbolNanos,
}

const COUNTERS: usize = 18;

static GLOBAL: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];
static TIMING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static LOCAL: [Cell<u64>; COUNTERS] = const { [const { Cell::new(0) }; COUNTERS] };
    /// Nesting depth of [`uncounted`] on this thread; nothing is counted
    /// while it is above zero.
    static PAUSED: Cell<u32> = const { Cell::new(0) };
}

/// The process-wide counters (all stores, all threads).
pub fn snapshot() -> ReadStats {
    ReadStats::from_array(std::array::from_fn(|i| GLOBAL[i].load(Ordering::Relaxed)))
}

/// The calling thread's own counters since the thread started. Unaffected
/// by [`reset`] and by other threads, so exact for the queries this thread
/// ran.
pub fn thread_snapshot() -> ReadStats {
    LOCAL.with(|l| ReadStats::from_array(std::array::from_fn(|i| l[i].get())))
}

/// Zero the process-wide counters. Racy against concurrent queries; meant
/// for benchmarks, which should prefer [`ReadStats::since`].
pub fn reset() {
    for c in &GLOBAL {
        c.store(0, Ordering::Relaxed);
    }
}

/// Turn decode timing on or off. This is a **process-global** toggle: it
/// affects every store and thread at once. Off by default.
pub fn set_timing(on: bool) {
    TIMING.store(on, Ordering::Relaxed);
}

/// Whether decode timing is on.
pub fn timing() -> bool {
    TIMING.load(Ordering::Relaxed)
}

fn add(counter: Counter, n: u64) {
    if PAUSED.with(Cell::get) > 0 {
        return;
    }
    let i = counter as usize;
    GLOBAL[i].fetch_add(n, Ordering::Relaxed);
    LOCAL.with(|l| l[i].set(l[i].get().wrapping_add(n)));
}

/// Count one event.
#[inline]
pub(crate) fn bump(counter: Counter) {
    add(counter, 1);
}

/// Count one event and, with timing on, add the time `f` takes to `nanos`.
#[inline]
pub(crate) fn timed<T>(counter: Counter, nanos: Counter, f: impl FnOnce() -> T) -> T {
    bump(counter);
    if !timing() {
        return f();
    }
    let start = Instant::now();
    let out = f();
    // At least 1 ns, so "timing on" is observable even on a coarse clock.
    let spent = u64::try_from(start.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1);
    add(nanos, spent);
    out
}

/// Add `bytes` (the length of an encoded input) to a byte counter.
#[inline]
pub(crate) fn add_bytes(counter: Counter, bytes: usize) {
    add(counter, u64::try_from(bytes).unwrap_or(u64::MAX));
}

/// Count one query and, with timing on, its wall time; wraps each
/// `StoreRead` call.
#[inline]
pub(crate) fn timed_query<T>(f: impl FnOnce() -> T) -> T {
    timed(Counter::QueryCount, Counter::QueryNanos, f)
}

/// Run `f` without counting anything it does on this thread; for write and
/// maintenance paths that reuse the read-side decoders.
pub(crate) fn uncounted<T>(f: impl FnOnce() -> T) -> T {
    struct Resume;
    impl Drop for Resume {
        fn drop(&mut self) {
            PAUSED.with(|p| p.set(p.get() - 1));
        }
    }
    PAUSED.with(|p| p.set(p.get() + 1));
    let _resume = Resume;
    f()
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

    #[test]
    fn array_round_trip_keeps_every_field_in_place() {
        let v: [u64; COUNTERS] = std::array::from_fn(|i| i as u64 + 1);
        assert_eq!(ReadStats::from_array(v).to_array(), v);
    }

    #[test]
    fn thread_view_sees_only_this_thread_and_uncounted_pauses() {
        let before = thread_snapshot();
        std::thread::spawn(|| bump(Counter::ReadTxns))
            .join()
            .expect("other thread");
        assert_eq!(thread_snapshot().since(&before).read_txns, 0);
        uncounted(|| uncounted(|| bump(Counter::ReadTxns)));
        bump(Counter::ReadTxns);
        assert_eq!(thread_snapshot().since(&before).read_txns, 1);
    }

    #[test]
    fn bytes_and_queries_count_exactly_per_thread() {
        let before = thread_snapshot();
        add_bytes(Counter::DictBytes, 7);
        add_bytes(Counter::SymbolBytes, 3);
        add_bytes(Counter::LazyBytes, 11);
        add_bytes(Counter::FullBytes, 13);
        uncounted(|| add_bytes(Counter::FullBytes, 100));
        assert_eq!(timed_query(|| 42), 42);
        timed_query(|| ());
        let d = thread_snapshot().since(&before);
        assert_eq!(
            (d.dict_bytes, d.symbol_bytes, d.lazy_bytes, d.full_bytes),
            (7, 3, 11, 13)
        );
        assert_eq!(d.queries, 2);
    }
}
