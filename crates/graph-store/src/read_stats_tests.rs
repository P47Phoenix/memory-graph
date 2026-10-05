//! Exact counts for the read-path counters (read cache phase 0, ADR 0008,
//! epic story 45). Every test measures its own work through
//! [`read_stats::thread_snapshot`], so the parallel test runner cannot
//! disturb the numbers.
use super::*;
use crate::read_stats::{thread_snapshot, ReadStats};
use std::collections::HashSet;

/// One file whose only identifier, `alpha`, occurs three times, next to one
/// occurrence of `beta`.
fn tiny_store() -> (tempfile::TempDir, V2Store) {
    let d = tempfile::tempdir().expect("tempdir");
    let s = V2Store::open(d.path().join("g.redb")).expect("open");
    let file = BatchFile {
        path: "a.txt",
        bytes: b"alpha alpha alpha beta\n",
        language: None,
        origin: None,
        ..Default::default()
    };
    for r in s
        .index_batch("o", "r", &[file], IndexOptions::default())
        .expect("index_batch")
    {
        r.expect("index file");
    }
    (d, s)
}

/// s with the nanosecond fields zeroed: they depend on the process-global
/// timing toggle, which a parallel test may flip.
fn counts(s: ReadStats) -> ReadStats {
    ReadStats {
        dict_decode_nanos: 0,
        lazy_decode_nanos: 0,
        full_decode_nanos: 0,
        ..s
    }
}

/// The counters for the work `f` does on this thread.
fn measure<T>(f: impl FnOnce() -> T) -> (T, ReadStats) {
    let before = thread_snapshot();
    let out = f();
    (out, thread_snapshot().since(&before))
}

fn alpha_query() -> Query {
    let mut q = Query::new("alpha");
    q.grain = Grain::Token;
    q
}

fn term_id(s: &V2Store, text: &str) -> u64 {
    let rt = s.db.begin_read().expect("begin_read");
    let t = rt.open_table(crate::v2::DICT).expect("dict table");
    t.get(text).expect("get").expect("term interned").value()
}

fn stream_bytes(s: &V2Store) -> Vec<u8> {
    let rt = s.db.begin_read().expect("begin_read");
    let t = rt.open_table(crate::v2::STREAMS).expect("streams table");
    let row = t.iter().expect("iter").next().expect("one stream");
    let bytes = row.expect("row").1.value().to_vec();
    bytes
}

#[test]
fn a_repeated_query_counts_the_same_one_txn_and_one_miss_per_distinct_term() {
    let (_d, s) = tiny_store();
    let read = || s.file_tokens("o", "r", "a.txt").expect("file_tokens");
    let (tokens1, first) = measure(read);
    let (tokens2, second) = measure(read);
    assert_eq!(tokens1, tokens2);
    assert_eq!(
        counts(first),
        counts(second),
        "a fixed query counts the same work each time"
    );
    assert_eq!(first.read_txns, 1);
    // Each distinct token text is resolved from the dictionary once, then
    // answered from the per-query memo.
    let tokens = tokens1.expect("a.txt is indexed");
    let distinct: HashSet<&str> = tokens.iter().map(|t| t.name.as_str()).collect();
    assert!(distinct.contains("alpha") && distinct.contains("beta"));
    assert_eq!(first.dict_text_memo_misses, distinct.len() as u64);
    assert_eq!(first.dict_block_decodes, first.dict_text_memo_misses);
    assert_eq!(first.dict_strings_decoded, first.dict_text_memo_misses);
    assert_eq!(
        first.dict_text_memo_hits,
        (tokens.len() - distinct.len()) as u64
    );
}

#[test]
fn a_token_search_reads_one_txn_the_same_way_each_time() {
    let (_d, s) = tiny_store();
    let (hits1, first) = measure(|| s.search(&alpha_query()).expect("search"));
    let (hits2, second) = measure(|| s.search(&alpha_query()).expect("search"));
    assert_eq!(hits1, hits2);
    assert_eq!(hits1.len(), 3);
    assert_eq!(counts(first), counts(second));
    assert_eq!(first.read_txns, 1);
}

#[test]
fn an_uncached_text_lookup_scans_one_block_and_decodes_one_string() {
    let (_d, s) = tiny_store();
    let (alpha, beta) = (term_id(&s, "alpha"), term_id(&s, "beta"));
    let rt = s.db.begin_read().expect("begin_read");
    let r = crate::v2::R::new(&rt).expect("reader");

    let (text, miss) = measure(|| r.text(alpha).expect("text"));
    assert_eq!(&*text, "alpha");
    assert_eq!(miss.dict_block_decodes, 1);
    assert_eq!(miss.dict_strings_decoded, 1, "only the looked-up string");
    assert_eq!(miss.dict_text_memo_misses, 1);
    assert_eq!(miss.dict_text_memo_hits, 0);

    let (_, hit) = measure(|| r.text(alpha).expect("text"));
    assert_eq!(hit.dict_block_decodes, 0);
    assert_eq!(hit.dict_strings_decoded, 0);
    assert_eq!(hit.dict_text_memo_hits, 1);

    let (_, other) = measure(|| r.text(beta).expect("text"));
    assert_eq!(other.dict_block_decodes, 1);
    assert_eq!(other.dict_strings_decoded, 1);
}

#[test]
fn a_full_decode_counts_only_as_a_full_decode() {
    let (_d, s) = tiny_store();
    let bytes = stream_bytes(&s);
    let (_, d) = measure(|| crate::codec::decode(&bytes).expect("decode"));
    let want = ReadStats {
        full_stream_decodes: 1,
        full_decode_nanos: d.full_decode_nanos,
        ..Default::default()
    };
    assert_eq!(d, want);
}

#[test]
fn a_lazy_decode_then_symbols_counts_one_of_each() {
    let (_d, s) = tiny_store();
    let bytes = stream_bytes(&s);
    let (_, d) = measure(|| {
        crate::codec::decode_lazy(&bytes)
            .expect("decode_lazy")
            .symbols()
            .expect("symbols")
    });
    let want = ReadStats {
        lazy_stream_decodes: 1,
        symbol_section_decodes: 1,
        lazy_decode_nanos: d.lazy_decode_nanos,
        ..Default::default()
    };
    assert_eq!(d, want);
}

#[test]
fn writes_do_not_count_as_reads() {
    let d = tempfile::tempdir().expect("tempdir");
    let s = V2Store::open(d.path().join("g.redb")).expect("open");
    let index = |body: &[u8]| {
        let file = BatchFile {
            path: "a.txt",
            bytes: body,
            ..Default::default()
        };
        s.index_batch("o", "r", &[file], IndexOptions::default())
            .expect("index_batch")
    };
    let (_, w) = measure(|| {
        index(b"one two");
        index(b"three four"); // replaces a.txt: removes the old content
        s.vacuum().expect("vacuum")
    });
    assert_eq!(w.dict_block_decodes, 0, "{w:?}");
    assert_eq!(w.dict_strings_decoded, 0, "{w:?}");
    assert_eq!(w.lazy_stream_decodes + w.symbol_section_decodes, 0, "{w:?}");
    assert_eq!(w.full_stream_decodes, 0, "{w:?}");
}

/// One test for both timing states, because the toggle is process-global:
/// while it is on, other tests' nanosecond fields may be non-zero, so they
/// compare counts only ([`counts`]) or copy the measured nanos.
#[test]
fn timing_fills_the_nanos_only_when_on_and_never_changes_answers() {
    let (_d, s) = tiny_store();
    let workload = || {
        let hits = s.search(&alpha_query()).expect("search");
        let tokens = s.file_tokens("o", "r", "a.txt").expect("file_tokens");
        let bytes = stream_bytes(&s);
        crate::codec::decode(&bytes).expect("decode");
        (hits, tokens)
    };

    read_stats::set_timing(false);
    let (off_answers, off) = measure(workload);
    assert_eq!(off.dict_decode_nanos, 0);
    assert_eq!(off.lazy_decode_nanos, 0);
    assert_eq!(off.full_decode_nanos, 0);

    read_stats::set_timing(true);
    let (on_answers, on) = measure(workload);
    read_stats::set_timing(false);
    assert!(on.dict_decode_nanos > 0, "{on:?}");
    assert!(on.lazy_decode_nanos > 0, "{on:?}");
    assert!(on.full_decode_nanos > 0, "{on:?}");
    assert_eq!(on_answers, off_answers);
}
