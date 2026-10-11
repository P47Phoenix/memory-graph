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
        query_nanos: 0,
        symbol_decode_nanos: 0,
        // Timers too: another test may switch timing on in parallel.
        search_posting_nanos: 0,
        search_ctx_nanos: 0,
        search_walk_nanos: 0,
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
    assert_eq!(first.queries, 1, "one StoreRead call is one query");
    // Each distinct token text is resolved from the dictionary once, then
    // answered from the per-query memo.
    let tokens = tokens1.expect("a.txt is indexed");
    let distinct: HashSet<&str> = tokens.iter().map(|t| t.name.as_str()).collect();
    assert!(distinct.contains("alpha") && distinct.contains("beta"));
    assert_eq!(first.dict_text_memo_misses, distinct.len() as u64);
    assert_eq!(first.dict_block_decodes, first.dict_text_memo_misses);
    assert_eq!(first.dict_strings_decoded, first.dict_text_memo_misses);
    assert!(first.dict_bytes > 0, "{first:?}");
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
    assert_eq!(first.queries, 1);
}

/// Every `StoreRead` method counts exactly one query: none is missed and
/// none double-counts by calling another counted method (the paged
/// defaults go through one counted call each).
#[test]
fn each_store_read_method_counts_exactly_one_query() {
    let (_d, s) = tiny_store();
    let root = s.roots().expect("roots")[0].id;
    let file = s
        .descendants(root)
        .expect("descendants")
        .into_iter()
        .find(|n| n.kind == NodeKind::File)
        .expect("a file node")
        .id;
    type Call<'a> = Box<dyn Fn(&V2Store) + 'a>;
    let calls: Vec<(&str, Call<'_>)> = vec![
        ("get", Box::new(|s| drop(s.get(file).expect("get")))),
        (
            "parent",
            Box::new(|s| drop(s.parent(file).expect("parent"))),
        ),
        (
            "count_nodes",
            Box::new(|s| {
                s.count_nodes(NodeKind::Token).expect("count");
            }),
        ),
        ("roots", Box::new(|s| drop(s.roots().expect("roots")))),
        (
            "children",
            Box::new(|s| drop(s.children(root).expect("children"))),
        ),
        (
            "descendants",
            Box::new(|s| drop(s.descendants(root).expect("descendants"))),
        ),
        (
            "ancestors",
            Box::new(|s| drop(s.ancestors(file).expect("ancestors"))),
        ),
        (
            "file_tokens",
            Box::new(|s| drop(s.file_tokens("o", "r", "a.txt").expect("tokens"))),
        ),
        (
            "describe",
            Box::new(|s| drop(s.describe(None, None).expect("describe"))),
        ),
        (
            "describe_by_scan",
            Box::new(|s| drop(s.describe_by_scan(None, None).expect("scan"))),
        ),
        (
            "search_symbols",
            Box::new(|s| drop(s.search_symbols(&SymbolQuery::new("*")).expect("symbols"))),
        ),
        (
            "search",
            Box::new(|s| drop(s.search(&alpha_query()).expect("search"))),
        ),
        (
            "children_page",
            Box::new(|s| drop(s.children_page(root, 0, 10).expect("page"))),
        ),
        (
            "descendants_page",
            Box::new(|s| drop(s.descendants_page(root, 0, 10).expect("page"))),
        ),
    ];
    for (name, call) in &calls {
        let (_, d) = measure(|| call(&s));
        assert_eq!(d.queries, 1, "{name}: {d:?}");
        assert_eq!(d.read_txns, 1, "{name}: {d:?}");
    }
}

#[test]
fn snapshot_reads_count_one_query_each_and_no_txn() {
    let (_d, s) = tiny_store();
    let snap = s.snapshot_owned().expect("snapshot");
    let (_, d) = measure(|| {
        snap.search(&alpha_query()).expect("search");
        snap.describe(None, None).expect("describe")
    });
    assert_eq!(d.queries, 2, "{d:?}");
    assert_eq!(d.read_txns, 0, "{d:?}");
}

#[test]
fn a_dictionary_scan_counts_the_encoded_block_bytes() {
    let (_d, s) = tiny_store();
    let alpha = term_id(&s, "alpha");
    let rt = s.db.begin_read().expect("begin_read");
    let block_len = {
        let t = rt.open_table(crate::v2::DICT_REV).expect("dict_rev table");
        let block = crate::v2::dict_rev_block(&t, alpha)
            .expect("block lookup")
            .expect("alpha's block");
        let len = block.value().len() as u64;
        len
    };
    let r = crate::v2::R::new(&rt).expect("reader");
    let (_, d) = measure(|| r.text(alpha).expect("text"));
    assert_eq!(d.dict_bytes, block_len, "{d:?}");
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
        full_bytes: bytes.len() as u64,
        full_decode_nanos: d.full_decode_nanos,
        ..Default::default()
    };
    assert_eq!(d, want);
}

#[test]
fn a_lazy_decode_then_symbols_counts_one_of_each() {
    let (_d, s) = tiny_store();
    let bytes = stream_bytes(&s);
    let sym_len = crate::codec::decode_lazy(&bytes)
        .expect("decode_lazy")
        .symbol_section_len() as u64;
    let (_, d) = measure(|| {
        crate::codec::decode_lazy(&bytes)
            .expect("decode_lazy")
            .symbols()
            .expect("symbols")
    });
    let want = ReadStats {
        lazy_stream_decodes: 1,
        symbol_section_decodes: 1,
        lazy_bytes: bytes.len() as u64,
        symbol_bytes: sym_len,
        lazy_decode_nanos: d.lazy_decode_nanos,
        symbol_decode_nanos: d.symbol_decode_nanos,
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
    let bytes = w.dict_bytes + w.symbol_bytes + w.lazy_bytes + w.full_bytes;
    assert_eq!(bytes, 0, "{w:?}");
    assert_eq!(w.queries, 0, "{w:?}");
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
        crate::codec::decode_lazy(&bytes)
            .expect("decode_lazy")
            .symbols()
            .expect("symbols");
        (hits, tokens)
    };

    read_stats::set_timing(false);
    let (off_answers, off) = measure(workload);
    assert_eq!(off.dict_decode_nanos, 0);
    assert_eq!(off.lazy_decode_nanos, 0);
    assert_eq!(off.full_decode_nanos, 0);
    assert_eq!(off.query_nanos, 0);
    assert_eq!(off.symbol_decode_nanos, 0);

    read_stats::set_timing(true);
    let (on_answers, on) = measure(workload);
    read_stats::set_timing(false);
    assert!(on.dict_decode_nanos > 0, "{on:?}");
    assert!(on.lazy_decode_nanos > 0, "{on:?}");
    assert!(on.full_decode_nanos > 0, "{on:?}");
    assert!(on.query_nanos > 0, "{on:?}");
    assert!(on.symbol_decode_nanos > 0, "{on:?}");
    assert_eq!(on_answers, off_answers);
}
