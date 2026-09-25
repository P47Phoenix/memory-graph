//! v2 test gaps (issue #19 QA comment) and configuration-equivalence checks
//! (`two_configs`: the same store with different chunk and cache settings).
//! Each named test kills the mutant it is named for.
use super::*;

/// Build an extraction from `(name, kind, start, end)` symbols and
/// `(text, start, end)` tokens (one line, so columns follow bytes).
pub(crate) fn span_ext(
    syms: &[(&str, SymbolKind, u32, u32)],
    toks: &[(&str, u32, u32)],
) -> graph_core::Extraction {
    use graph_core::{Extraction, Span, SymbolDecl, TokenClass, TokenDecl};
    let sp = |s: u32, e: u32| Span {
        start: s,
        end: e,
        start_line: 1,
        start_col: s + 1,
        end_line: 1,
        end_col: e + 1,
    };
    Extraction {
        has_errors: false,
        symbols: syms
            .iter()
            .map(|&(n, kind, s, e)| SymbolDecl {
                name: n.into(),
                kind,
                lang_kind: None,
                span: sp(s, e),
            })
            .collect(),
        tokens: toks
            .iter()
            .map(|&(t, s, e)| TokenDecl {
                text: t.into(),
                class: TokenClass::Identifier,
                span: sp(s, e),
            })
            .collect(),
    }
}

/// Two empty stores in different configurations: `a` with the defaults,
/// `b` committing one file per chunk with a small cache. Query-visible
/// behaviour must not depend on either setting (`run_differential`).
pub(crate) fn two_configs() -> (tempfile::TempDir, Box<dyn Store>, Box<dyn Store>) {
    let d = tempfile::tempdir().unwrap();
    let a = open_store(&d.path().join("a.redb"), vec![]).unwrap();
    let mut b = V2Store::open_with_cache_bytes(d.path().join("b.redb"), Some(1 << 20)).unwrap();
    b.set_chunk_bytes(1);
    (d, a, Box::new(b))
}

fn names(v: Vec<graph_core::Node>) -> Vec<String> {
    v.into_iter().map(|n| n.name).collect()
}

fn hit_names(s: &dyn Store, p: &str) -> Vec<String> {
    s.search_symbols(&SymbolQuery::new(p))
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect()
}

#[test]
fn search_symbols_repo_filter() {
    let (_d, a, b) = two_configs();
    let ex = span_ext(&[("S", SymbolKind::Function, 0, 4)], &[("t", 0, 1)]);
    for s in [&a, &b] {
        s.ingest_file("o", "r1", "x.rs", "rust", &ex).unwrap();
        s.ingest_file("o", "r2", "y.rs", "rust", &ex).unwrap();
        let mut q = SymbolQuery::new("S");
        assert_eq!(s.search_symbols(&q).unwrap().len(), 2);
        q.repo = Some("r2".into());
        let hits = s.search_symbols(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].repo.as_str(), hits[0].file.as_str()),
            ("r2", "y.rs")
        );
        q.repo = Some("nope".into());
        assert!(s.search_symbols(&q).unwrap().is_empty());
        q.repo = Some("r1".into());
        q.org = Some("other".into());
        assert!(s.search_symbols(&q).unwrap().is_empty());
    }
}

#[test]
fn search_symbols_literal_star_and_prefix() {
    let f = SymbolKind::Function;
    let (_d, a, b) = two_configs();
    let ex = span_ext(
        &[
            ("a*", f, 0, 4),
            ("a", f, 5, 9),
            ("ab", f, 10, 14),
            ("a**", f, 15, 19),
        ],
        &[],
    );
    for s in [&a, &b] {
        s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        assert_eq!(hit_names(&**s, "a\\*"), ["a*"], "literal star, exact");
        assert_eq!(hit_names(&**s, "a*"), ["a*", "a", "ab", "a**"], "prefix");
        assert_eq!(hit_names(&**s, "a"), ["a"]);
        assert!(hit_names(&**s, "\\*").is_empty());
    }
}

#[test]
fn search_symbols_rows_sorted_by_source_position_within_a_file() {
    let f = SymbolKind::Function;
    let (_d, a, b) = two_configs();
    // Name order (index order) is the reverse of source order.
    let ex = span_ext(&[("a", f, 20, 24), ("b", f, 0, 4), ("c", f, 10, 14)], &[]);
    for s in [&a, &b] {
        s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        assert_eq!(hit_names(&**s, "*"), ["b", "c", "a"]);
    }
}

#[test]
fn traversal_by_symbol_id_and_out_of_range_boundaries() {
    let d = tempfile::tempdir().unwrap();
    let s = open_store(&d.path().join("b.redb"), vec![]).unwrap();
    // S(0..20) > T(2..10); tokens: t0 in T, t1 in S, t2 outside.
    let ex = span_ext(
        &[
            ("S", SymbolKind::Type, 0, 20),
            ("T", SymbolKind::Function, 2, 10),
        ],
        &[("t0", 3, 4), ("t1", 12, 13), ("t2", 25, 26)],
    );
    s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    let any = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[0].id;
    let id = |tag: u64, i: u64| (any & 0x3fff_ffff_0000_0000) | (tag << 62) | i;
    // Descendants of a symbol id (kills "always empty").
    assert_eq!(names(s.descendants(id(1, 0)).unwrap()), ["T", "t0", "t1"]);
    assert_eq!(names(s.descendants(id(1, 1)).unwrap()), ["t0"]);
    assert_eq!(names(s.children(id(1, 0)).unwrap()), ["T", "t1"]);
    assert_eq!(
        names(s.ancestors(id(2, 0)).unwrap()),
        ["T", "S", "x.rs", "r", "o"]
    );
    // One past the end: 2 symbols, 3 tokens.
    assert!(s.ancestors(id(1, 2)).unwrap().is_empty());
    assert!(s.ancestors(id(2, 3)).unwrap().is_empty());
    assert!(s.children(id(1, 2)).unwrap().is_empty());
    assert!(s.descendants(id(1, 2)).unwrap().is_empty());
    assert!(s.get(id(1, 2)).unwrap().is_none());
    // Far out of range and a missing file.
    assert!(s.ancestors(id(1, 1000)).unwrap().is_empty());
    assert!(s.ancestors(id(2, 1000)).unwrap().is_empty());
    assert!(s.children(id(1, 1000)).unwrap().is_empty());
    assert!(s.descendants(id(1, 1000)).unwrap().is_empty());
    let ghost = (1u64 << 62) | (999 << 32);
    assert!(s.ancestors(ghost).unwrap().is_empty());
    assert!(s.children(ghost).unwrap().is_empty());
    assert!(s.descendants(ghost).unwrap().is_empty());
    // Same through a snapshot handle.
    let snap = s.snapshot().unwrap();
    assert_eq!(
        names(snap.descendants(id(1, 0)).unwrap()),
        ["T", "t0", "t1"]
    );
    assert!(snap.descendants(id(1, 2)).unwrap().is_empty());
}

/// A file with `n` tokens, all nested in one symbol spanning the whole file
/// (one byte per token so `n` fits comfortably in a `u32` span).
fn many_tokens_ext(n: usize) -> graph_core::Extraction {
    let toks: Vec<(String, u32, u32)> = (0..n)
        .map(|i| (format!("t{i}"), i as u32, i as u32 + 1))
        .collect();
    span_ext(
        &[("S", SymbolKind::Function, 0, n as u32)],
        &toks
            .iter()
            .map(|(t, s, e)| (t.as_str(), *s, *e))
            .collect::<Vec<_>>(),
    )
}

#[test]
fn ancestors_does_not_decode_the_whole_stream() {
    let n = 20 * codec::CHECKPOINT_EVERY;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &many_tokens_ext(n))
        .unwrap();
    let last = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[n - 1].id;

    codec::RECORDS_DECODED.with(|c| c.set(0));
    let anc = s.ancestors(last).unwrap();
    let decoded = codec::RECORDS_DECODED.with(|c| c.get());

    assert_eq!(names(anc), ["S", "x.rs", "r", "o"]);
    // A single `tokens_at` lookup decodes at most `CHECKPOINT_EVERY - 1`
    // token records to reach its target from the nearest checkpoint,
    // regardless of how many tokens the file has (here `n`).
    assert!(
        decoded <= codec::CHECKPOINT_EVERY,
        "decoded {decoded} of {n} token records"
    );
}

#[test]
fn get_does_not_decode_the_whole_stream() {
    let n = 20 * codec::CHECKPOINT_EVERY;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &many_tokens_ext(n))
        .unwrap();
    let last = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[n - 1].id;

    codec::RECORDS_DECODED.with(|c| c.set(0));
    let node = s.get(last).unwrap().unwrap();
    let decoded = codec::RECORDS_DECODED.with(|c| c.get());

    assert_eq!(node.name, format!("t{}", n - 1));
    assert!(
        decoded <= codec::CHECKPOINT_EVERY,
        "decoded {decoded} of {n} token records"
    );
}

/// Issue #35 (mutation testing, PR #34 QA review): `with_lazy`'s
/// `Ok(Some(f(&codec::decode_lazy(v.value())?)?))` must surface a corrupt or
/// truncated stream row as an error through every caller that goes through
/// it -- `get`, `ancestors` and `parent` -- never swallow it into `Ok(None)`
/// or a silently wrong result. No prior test constructed a corrupt stream
/// row and called any of the three through it, so a mutant that changed
/// `with_lazy` to `Err(_) => Ok(None)` went undetected.
#[test]
fn with_lazy_propagates_decode_errors_through_get_ancestors_parent() {
    let n = 2 * codec::CHECKPOINT_EVERY + 3;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &many_tokens_ext(n))
        .unwrap();
    let last_tok = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[n - 1].id;
    // Same `(tag, file, index)` bit layout as `v2::split_id`/`sub_id`
    // (private to that module; reconstructed here, as `ranged_children_file_
    // level::file_id` already does elsewhere in this file).
    let file = (last_tok >> 32) & 0x3fff_ffff;
    let sym_id = (1u64 << 62) | (file << 32); // the file's only symbol, "S"
    let tok_id = (2u64 << 62) | (file << 32); // its first token, "t0"

    let good = {
        let rt = s.db.begin_read().unwrap();
        let t = rt.open_table(crate::v2::STREAMS).unwrap();
        t.get(file).unwrap().unwrap().value().to_vec()
    };
    assert!(!good.is_empty());

    let set_stream = |bytes: &[u8]| {
        let wt = s.db.begin_write().unwrap();
        {
            let mut t = wt.open_table(crate::v2::STREAMS).unwrap();
            t.insert(file, bytes).unwrap();
        }
        wt.commit().unwrap();
    };

    // Every prefix truncation of a genuine, multi-checkpoint stream is
    // corrupt for a query that must decode all the way to its end: reaching
    // the *last* token forces `Lazy::tokens_at` through to the final byte,
    // so any missing suffix -- whether it lands in the header, the
    // checkpoint table or deep in the token payload -- must surface as
    // `StoreError`, never `Ok(None)`/a panic (mirrors codec.rs's own
    // `corrupt_input_is_an_error_not_a_panic` prefix-truncation idiom, but
    // exercised through the store's `get`/`ancestors`/`parent`).
    for cut in 0..good.len() {
        set_stream(&good[..cut]);
        assert!(s.get(last_tok).is_err(), "get(last_tok) at prefix {cut}");
        assert!(
            s.ancestors(last_tok).is_err(),
            "ancestors(last_tok) at prefix {cut}"
        );
        assert!(
            s.parent(last_tok).is_err(),
            "parent(last_tok) at prefix {cut}"
        );
    }

    // Truncated to nothing, and to just the format byte: `decode_lazy`'s own
    // header parse must fail before any symbol- or token-specific logic
    // runs, so every one of get/ancestors/parent errors for *either* kind of
    // id (a symbol and a token needing no more than the header to exist).
    for cut in [0usize, 1] {
        set_stream(&good[..cut]);
        for id in [sym_id, tok_id] {
            assert!(s.get(id).is_err(), "get id={id} at prefix {cut}");
            assert!(
                s.ancestors(id).is_err(),
                "ancestors id={id} at prefix {cut}"
            );
            assert!(s.parent(id).is_err(), "parent id={id} at prefix {cut}");
        }
    }

    // An invalid format byte is rejected outright (mirrors
    // `codec::old_format_2_streams_are_rejected_not_misread`).
    let mut bad_fmt = good.clone();
    bad_fmt[0] = 0xff;
    set_stream(&bad_fmt);
    assert!(s.get(sym_id).is_err());
    assert!(s.ancestors(tok_id).is_err());
    assert!(s.parent(tok_id).is_err());

    // Sanity: restoring the original bytes makes the store healthy again,
    // proving the corruption above -- not some unrelated breakage -- is what
    // drove every assertion above.
    set_stream(&good);
    assert!(s.get(last_tok).unwrap().is_some());
    assert_eq!(
        names(s.ancestors(last_tok).unwrap()),
        ["S", "x.rs", "r", "o"]
    );
}

/// Issue #66 (follow-up to issue #35 / PR #65): `with_lazy` has four more
/// callers beyond `get`/`ancestors`/`parent` -- `children_ranged`,
/// `children_ranged_file`, `descendants_ranged` and `descendants_ranged_file`
/// (the range-based `children`/`descendants` paths, taken whenever
/// `ranges_dense()` is true, which it is for every file built by
/// `many_tokens_ext`: one top-level symbol transitively covering every
/// token). Each shares the exact same
/// `Ok(Some(f(&codec::decode_lazy(v.value())?)?))` pattern as the three
/// callers PR #65 covered, so the same "never swallow the error" invariant
/// applies here too; this test exercises it through `children`/`descendants`
/// called on both a file id (the `*_file` variants) and a symbol id (the
/// non-file variants), the same corrupt-stream construction PR #65 used.
#[test]
fn with_lazy_propagates_decode_errors_through_ranged_children_and_descendants() {
    let n = 2 * codec::CHECKPOINT_EVERY + 3;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &many_tokens_ext(n))
        .unwrap();
    let any_tok = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[0].id;
    let file = (any_tok >> 32) & 0x3fff_ffff;
    let sym_id = (1u64 << 62) | (file << 32); // the file's only symbol, "S"

    // Sanity: both range-based paths are actually reachable on this file
    // before we start corrupting it, so a later `Err` really proves error
    // propagation and not just "the range path was never taken".
    assert!(!s.children(file).unwrap().is_empty());
    assert!(!s.descendants(file).unwrap().is_empty());
    assert!(!s.children(sym_id).unwrap().is_empty());
    assert!(!s.descendants(sym_id).unwrap().is_empty());

    let good = {
        let rt = s.db.begin_read().unwrap();
        let t = rt.open_table(crate::v2::STREAMS).unwrap();
        t.get(file).unwrap().unwrap().value().to_vec()
    };
    assert!(!good.is_empty());

    let set_stream = |bytes: &[u8]| {
        let wt = s.db.begin_write().unwrap();
        {
            let mut t = wt.open_table(crate::v2::STREAMS).unwrap();
            t.insert(file, bytes).unwrap();
        }
        wt.commit().unwrap();
    };

    // `decode_lazy` itself eagerly parses the format byte, header varints,
    // the symbol section's bounds and the *entire* checkpoint table (only
    // symbol/token *records* are decoded lazily -- see `codec::decode_lazy`).
    // So any prefix shorter than that eagerly-parsed portion makes
    // `decode_lazy` itself fail, and `with_lazy` must surface that failure
    // identically through every one of its callers -- this is the shared
    // "never swallow the error" behavior this test targets, independent of
    // which caller-specific bytes (if any) each function goes on to decode
    // lazily afterwards. Find that boundary directly rather than guessing at
    // the header layout.
    let eager_prefix_len = (0..=good.len())
        .find(|&cut| codec::decode_lazy(&good[..cut]).is_ok())
        .expect("the full stream itself must decode_lazy successfully");
    assert!(
        eager_prefix_len > 1,
        "test wants decode_lazy to eagerly validate more than just the format byte"
    );

    // Every prefix shorter than that boundary must surface as an error
    // through all four call sites, never `Ok(vec![])`/a panic -- this is
    // exactly the scenario a `with_lazy` mutant that turned `Err(_)` into
    // `Ok(None)` would hide.
    for cut in 0..eager_prefix_len {
        set_stream(&good[..cut]);
        assert!(s.children(file).is_err(), "children(file) at prefix {cut}");
        assert!(
            s.descendants(file).is_err(),
            "descendants(file) at prefix {cut}"
        );
        assert!(
            s.children(sym_id).is_err(),
            "children(sym_id) at prefix {cut}"
        );
        assert!(
            s.descendants(sym_id).is_err(),
            "descendants(sym_id) at prefix {cut}"
        );
    }

    // Beyond that boundary, `descendants(file)`/`children(sym_id)`/
    // `descendants(sym_id)` still need to decode token records for this
    // file's single, everything-covering symbol `S` (its own range, or its
    // top-level descendants subtree), so truncating anywhere in the token
    // payload must still fail for them. `children(file)` alone needs only
    // the already-validated symbol section for this particular file shape
    // (`S` has no sibling top-level tokens to look up), so it is not
    // expected to fail on every further truncation -- that is a property of
    // *this caller's own logic*, not of `with_lazy`, and is exercised
    // separately by `with_lazy_propagates_decode_errors_through_get_
    // ancestors_parent`'s reasoning for `get`/`ancestors`/`parent`.
    for cut in eager_prefix_len..good.len() {
        set_stream(&good[..cut]);
        assert!(
            s.descendants(file).is_err(),
            "descendants(file) at prefix {cut}"
        );
        assert!(
            s.children(sym_id).is_err(),
            "children(sym_id) at prefix {cut}"
        );
        assert!(
            s.descendants(sym_id).is_err(),
            "descendants(sym_id) at prefix {cut}"
        );
    }

    // An invalid format byte is rejected outright at every call site.
    let mut bad_fmt = good.clone();
    bad_fmt[0] = 0xff;
    set_stream(&bad_fmt);
    assert!(s.children(file).is_err());
    assert!(s.descendants(file).is_err());
    assert!(s.children(sym_id).is_err());
    assert!(s.descendants(sym_id).is_err());

    // Sanity: restoring the original bytes makes the store healthy again.
    set_stream(&good);
    assert!(!s.children(file).unwrap().is_empty());
    assert!(!s.descendants(file).unwrap().is_empty());
    assert!(!s.children(sym_id).unwrap().is_empty());
    assert!(!s.descendants(sym_id).unwrap().is_empty());
}

/// Issue #66: PR #65's corrupt-stream coverage only ever *truncates* a
/// stream (or corrupts its leading format byte), which always shrinks the
/// declared/actual length relationship in a way every decode step notices.
/// This test instead flips individual bytes *within* an otherwise
/// full-length stream -- the checkpoint table and the token-record payload
/// region -- so the byte count stays exactly right and only the content is
/// wrong.
///
/// The codec has **no checksum** (see `codec.rs`'s module doc and
/// `corrupt_input_is_an_error_not_a_panic`/`every_checkpoint_field_is_
/// verified`: correctness rests on structural bounds checks -- varint
/// well-formedness, checkpoint offsets strictly inside the token section,
/// a `debug_assert`-only re-derivation of symbol ranges -- not on any
/// content-integrity check). So a mid-stream bit flip is expected to land
/// in one of three buckets, and this test asserts only what the codec
/// actually promises:
///   1. it still trips a structural bounds check and decoding errors out
///      (e.g. a mangled checkpoint offset or a varint that no longer
///      terminates within the buffer);
///   2. it produces a **different but still successfully decoded** result
///      (no checksum to catch "wrong but plausible" bytes) -- this is an
///      accepted, documented limit of the format, not a bug;
///   3. in a debug build, the independent range-consistency `debug_assert`
///      in `codec::decode` may fire as a panic for a range-shaped
///      corruption -- also documented, so this test runs the corrupted call
///      through `catch_unwind` and treats a caught panic from *that specific,
///      known assertion* as an accepted outcome, while any *other* panic
///      (a real out-of-bounds/UB-shaped bug) fails the test loudly.
///
/// Across every mutation this test tries, no call ever panics for a reason
/// other than that documented debug-only assertion, and no out-of-range
/// index/slice access occurs (Rust's bounds-checked indexing would itself
/// panic on that, so the `catch_unwind` net also covers that case).
#[test]
fn mid_stream_corruption_never_panics_and_documents_no_checksum_guarantee() {
    let n = 3 * codec::CHECKPOINT_EVERY + 7;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &many_tokens_ext(n))
        .unwrap();
    let any_tok = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[0].id;
    let file = (any_tok >> 32) & 0x3fff_ffff;
    let sym_id = (1u64 << 62) | (file << 32);
    let last_tok_id = s.file_tokens("o", "r", "x.rs").unwrap().unwrap()[n - 1].id;

    let good = {
        let rt = s.db.begin_read().unwrap();
        let t = rt.open_table(crate::v2::STREAMS).unwrap();
        t.get(file).unwrap().unwrap().value().to_vec()
    };
    assert!(
        good.len() > 4 * codec::CHECKPOINT_EVERY,
        "test wants a stream with real checkpoint/payload bytes to mutate"
    );

    let set_stream = |bytes: &[u8]| {
        let wt = s.db.begin_write().unwrap();
        {
            let mut t = wt.open_table(crate::v2::STREAMS).unwrap();
            t.insert(file, bytes).unwrap();
        }
        wt.commit().unwrap();
    };

    let mut clean_errors = 0usize;
    let mut wrong_but_no_error = 0usize;
    let mut known_debug_assert_panics = 0usize;

    // Flip one byte (XOR 0xff, keeping the length identical) at a spread of
    // offsets across the whole stream -- header, checkpoint table and deep
    // into the token payload -- rather than only near the start.
    let offsets: Vec<usize> = (0..good.len()).step_by(7).collect();
    for &off in &offsets {
        let mut bytes = good.clone();
        bytes[off] ^= 0xff;
        assert_eq!(bytes.len(), good.len(), "mutation must preserve length");
        set_stream(&bytes);

        // Run every `with_lazy`-backed call site through `catch_unwind` so a
        // genuine out-of-bounds/UB-shaped panic is caught and reported
        // (never silently swallowed), while a normal `Err(_)` or `Ok(_)`
        // result is classified below.
        let outcomes: Vec<(&str, std::thread::Result<Result<usize>>)> = vec![
            (
                "get(last_tok)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.get(last_tok_id).map(|o| o.is_some() as usize)
                })),
            ),
            (
                "ancestors(last_tok)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.ancestors(last_tok_id).map(|v| v.len())
                })),
            ),
            (
                "children(file)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.children(file).map(|v| v.len())
                })),
            ),
            (
                "descendants(file)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.descendants(file).map(|v| v.len())
                })),
            ),
            (
                "children(sym_id)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.children(sym_id).map(|v| v.len())
                })),
            ),
            (
                "descendants(sym_id)",
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.descendants(sym_id).map(|v| v.len())
                })),
            ),
        ];

        for (label, outcome) in outcomes {
            match outcome {
                Ok(Ok(_)) => wrong_but_no_error += 1,
                Ok(Err(_)) => clean_errors += 1,
                Err(payload) => {
                    // Only the documented, debug-only range-consistency
                    // `debug_assert` in `codec::decode` (message contains
                    // "does not match", see `debug_assert_catches_a_
                    // corrupted_dense_range` in codec.rs) is an accepted
                    // panic here. Note the call sites above go through
                    // `decode_lazy`, not `decode`, and only `get`/`ancestors`/
                    // `children`/`descendants`'s *fallback* paths (not
                    // exercised by a dense file like this one) call `decode`
                    // directly -- so in practice this branch should not be
                    // hit for this test's dense file, and any panic here is
                    // reported loudly rather than assumed benign.
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    assert!(
                        msg.contains("does not match"),
                        "UNEXPECTED PANIC at mutated offset {off} (byte {orig:#04x} -> {new:#04x}) \
                         via {label}: {msg}\n\
                         This looks like a real out-of-bounds/UB-shaped bug from mid-stream \
                         corruption, not the documented debug-only range-consistency assert. \
                         Reporting per the issue #66 instructions instead of silently accepting it.",
                        orig = good[off],
                        new = bytes[off],
                    );
                    known_debug_assert_panics += 1;
                }
            }
        }
    }

    // Restore the original bytes: proves nothing else about the store broke,
    // and that every mutation above was genuinely a corruption, not some
    // unrelated failure.
    set_stream(&good);
    assert_eq!(
        names(s.ancestors(last_tok_id).unwrap()),
        ["S", "x.rs", "r", "o"]
    );
    assert!(!s.children(file).unwrap().is_empty());
    assert!(!s.descendants(sym_id).unwrap().is_empty());

    // Document, rather than assert away, what mid-stream corruption actually
    // does on this codec: some mutated offsets are caught as clean decode
    // errors, some silently produce a different-but-plausible (no-checksum)
    // result, and (only in debug builds, if a range field happened to be hit)
    // some trip the documented independent-recomputation debug_assert. All
    // three are legitimate given the codec's no-checksum design; what must
    // never happen -- and did not happen across any of the offsets/call
    // sites tried above -- is an unexplained panic or out-of-bounds read.
    eprintln!(
        "mid_stream_corruption_never_panics_and_documents_no_checksum_guarantee: \
         {} clean errors, {} wrong-but-no-error (no checksum, by design), \
         {} known debug_assert panics, across {} mutated offsets x 6 call sites",
        clean_errors,
        wrong_but_no_error,
        known_debug_assert_panics,
        offsets.len(),
    );
}

#[test]
fn vacuum_removes_only_dead_dictionary_terms() {
    let f = SymbolKind::Function;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("b.redb")).unwrap();
    // Terms: alpha, beta, S1.
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &span_ext(&[("S1", f, 0, 9)], &[("alpha", 1, 2), ("beta", 3, 4)]),
    )
    .unwrap();
    // Nothing is dead yet.
    assert_eq!(
        s.vacuum().unwrap(),
        VacuumStats {
            terms_removed: 0,
            terms_kept: 3
        }
    );
    // Replace: alpha, beta and S1 die; gamma and S2 are new; y.rs keeps beta.
    let dir = Some(ORIGIN_DIRECTORY);
    Store::ingest_file_with_origin(
        &s,
        "o",
        "r",
        "y.rs",
        "rust",
        &span_ext(&[], &[("beta", 0, 1)]),
        dir,
    )
    .unwrap();
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &span_ext(&[("S2", f, 0, 9)], &[("gamma", 1, 2)]),
    )
    .unwrap();
    assert_eq!(
        s.vacuum().unwrap(),
        VacuumStats {
            terms_removed: 2,
            terms_kept: 3
        }
    );
    assert_eq!(s.vacuum().unwrap().terms_removed, 0);
    // Live data is intact and a dead term can come back with a new id.
    assert_eq!(s.search(&Query::new("gamma")).unwrap().len(), 1);
    assert_eq!(s.search(&Query::new("beta")).unwrap().len(), 1);
    assert!(s.search(&Query::new("alpha")).unwrap().is_empty());
    assert_eq!(hit_names(&s, "S2"), ["S2"]);
    Store::ingest_file_with_origin(
        &s,
        "o",
        "r",
        "z.rs",
        "rust",
        &span_ext(&[], &[("alpha", 0, 1)]),
        dir,
    )
    .unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    // Pruning a file frees its terms too.
    let keep: std::collections::HashSet<String> = ["x.rs".to_string()].into();
    s.prune_files("o", "r", &keep, false).unwrap();
    assert_eq!(s.vacuum().unwrap().terms_removed, 2, "alpha and beta");
    assert_eq!(s.search(&Query::new("gamma")).unwrap().len(), 1);
}

/// ADR 0003 story 3, slice 3i: `children(symbol)`/`descendants(symbol)` now
/// read the stored per-symbol transitive token range instead of decoding the
/// whole stream, when the range is exact (`ranges_dense`). This module is
/// the differential proof that the range-based path (what `children`/
/// `descendants` actually call, since generated data here is always dense)
/// and the pre-3i fallback (`children_via_fallback`/`descendants_via_fallback`,
/// literal old code, called directly) agree on every symbol of every
/// generated file.
mod ranged_children {
    use super::*;
    use graph_core::{Span, SymbolDecl, TokenClass, TokenDecl};
    use proptest::prelude::*;

    fn sp(s: u32, e: u32) -> Span {
        Span {
            start: s,
            end: e,
            start_line: 1,
            start_col: s + 1,
            end_line: 1,
            end_col: e + 1,
        }
    }

    /// A nesting spec: `n` tokens (one byte each, in order) and a set of
    /// symbols as `(start_tok, end_tok)` (half-open, in token units),
    /// deduplicated and sorted so overlapping/duplicate spans (which
    /// `ingest_file` rejects, changing nothing) are rare, keeping most cases
    /// dense and worth differentially checking.
    pub(super) fn corpus() -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
        (1usize..60).prop_flat_map(|n| {
            (
                Just(n),
                prop::collection::vec((0usize..=n, 0usize..=n), 0..12),
            )
        })
    }

    pub(super) fn build(n: usize, syms: &[(usize, usize)]) -> graph_core::Extraction {
        let tokens: Vec<TokenDecl> = (0..n)
            .map(|i| TokenDecl {
                text: format!("t{i}"),
                class: TokenClass::Identifier,
                span: sp(i as u32, i as u32 + 1),
            })
            .collect();
        // Sort and nest properly: symbols ordered by (start asc, end desc) so
        // a symbol containing another always precedes it, matching what a
        // real extractor produces and what the codec's parent-index
        // invariant (`parent` refers to an earlier index) requires.
        let mut specs: Vec<(usize, usize)> = syms
            .iter()
            .map(|&(a, b)| (a.min(b), a.max(b)))
            .filter(|(a, b)| a < b)
            .collect();
        specs.sort();
        specs.dedup();
        specs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        // Keep only properly nested/disjoint spans (drop anything that would
        // partially overlap an already-kept span, which `ingest_file`
        // rejects outright): a stack of currently open ancestors, closing
        // any whose end is at or before the candidate's start, then keeping
        // the candidate only if it fits fully inside whatever ancestor (if
        // any) is still open.
        let mut specs2: Vec<(usize, usize)> = Vec::with_capacity(specs.len());
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for (a, b) in specs {
            while let Some(&(_, e)) = stack.last() {
                if e <= a {
                    stack.pop();
                } else {
                    break;
                }
            }
            if let Some(&(_, e)) = stack.last() {
                if b > e {
                    continue;
                }
            }
            specs2.push((a, b));
            stack.push((a, b));
        }
        let specs = specs2;
        let symbols: Vec<SymbolDecl> = specs
            .iter()
            .enumerate()
            .map(|(idx, &(a, b))| SymbolDecl {
                name: format!("S{idx}_{a}_{b}"),
                kind: SymbolKind::Function,
                lang_kind: None,
                span: sp(a as u32, b as u32),
            })
            .collect();
        graph_core::Extraction {
            has_errors: false,
            symbols,
            tokens,
        }
    }

    /// Every symbol id in file `path` of org "o" repo "r": `SymbolHit` carries
    /// no node id (ADR 0003 story 4), so this gets the file's raw entity id
    /// from any of its tokens' ids (masking off the tag/index bits the same
    /// way `traversal_by_symbol_id_and_out_of_range_boundaries` does) and
    /// walks `descendants(file)` -- unaffected by this slice, so a reliable
    /// oracle for "every symbol id" -- filtering to `NodeKind::Symbol`.
    fn symbol_ids(s: &dyn Store, path: &str) -> Vec<NodeId> {
        let Some(toks) = s.file_tokens("o", "r", path).unwrap() else {
            return Vec::new();
        };
        let Some(any) = toks.first().map(|n| n.id) else {
            return Vec::new();
        };
        let file = (any >> 32) & 0x3fff_ffff;
        s.descendants(file)
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == graph_core::NodeKind::Symbol)
            .map(|n| n.id)
            .collect()
    }

    fn assert_same(v: &V2Store, id: NodeId) {
        assert_eq!(
            names(v.children(id).unwrap()),
            names(v.children_via_fallback(id).unwrap()),
            "children names differ for {id}"
        );
        assert_eq!(
            v.children(id).unwrap(),
            v.children_via_fallback(id).unwrap(),
            "children nodes differ for {id}"
        );
        assert_eq!(
            v.descendants(id).unwrap(),
            v.descendants_via_fallback(id).unwrap(),
            "descendants nodes differ for {id}"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
        /// The single most important test in this slice: for every symbol of
        /// a generated file, the range-based `children`/`descendants` and the
        /// literal pre-3i fallback return identical `Vec<Node>` (same ids,
        /// order and spans).
        #[test]
        fn range_based_and_fallback_agree_on_every_symbol((n, syms) in corpus()) {
            let d = tempfile::tempdir().unwrap();
            let v = V2Store::open(d.path().join("r.redb")).unwrap();
            v.ingest_file("o", "r", "x.rs", "rust", &build(n, &syms)).unwrap();
            for id in symbol_ids(&v, "x.rs") {
                assert_same(&v, id);
            }
        }
    }

    /// Confirms the fallback is actually reachable (not dead code): with
    /// out-of-order tokens, `ranges_dense()` is `false`, so `children`/
    /// `descendants` must go through the fallback, and still be correct.
    #[test]
    fn out_of_order_tokens_take_the_fallback_and_are_still_correct() {
        let ex = span_ext(
            &[
                ("Outer", SymbolKind::Type, 0, 30),
                ("Inner", SymbolKind::Function, 5, 20),
            ],
            // t1 (span 12) precedes t0 (span 3) in storage order: breaks
            // contiguity for `Outer`'s range even though both are inside it.
            &[("t1", 12, 13), ("t0", 3, 4), ("t2", 25, 26)],
        );
        let d = tempfile::tempdir().unwrap();
        let v = V2Store::open(d.path().join("nd.redb")).unwrap();
        v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        for id in symbol_ids(&v, "x.rs") {
            assert_same(&v, id);
        }
        // `Outer`'s children in creation (source, not ordinal) order.
        let outer = symbol_ids(&v, "x.rs")
            .into_iter()
            .find(|&id| v.get(id).unwrap().unwrap().name == "Outer")
            .unwrap();
        assert_eq!(names(v.children(outer).unwrap()), ["t0", "Inner", "t2"]);
    }

    /// Locks in the symbol-vs-token tie-break rule ("symbols before tokens
    /// on a tie") for `children_ranged`'s merge loop. The differential
    /// proptest above cannot reach this case: its `corpus()` strategy
    /// filters out zero-width symbol spans, and a *nonzero*-width symbol at
    /// the same start as a token necessarily contains that token as a
    /// descendant rather than sitting beside it as a sibling -- so a
    /// zero-width symbol is the only way to construct a genuine sibling tie.
    /// QA review (PR #37) confirmed this gap by mutation: swapping the
    /// merge loop's `<=` for `<` was not caught by 2000 proptest cases but
    /// is caught here.
    #[test]
    fn a_zero_width_symbol_beats_a_token_at_the_same_start() {
        let ex = span_ext(
            &[
                ("Outer", SymbolKind::Type, 0, 20),
                ("Inner", SymbolKind::Function, 10, 10),
            ],
            &[("t0", 5, 6), ("t1", 10, 11)],
        );
        let d = tempfile::tempdir().unwrap();
        let v = V2Store::open(d.path().join("tie.redb")).unwrap();
        v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        for id in symbol_ids(&v, "x.rs") {
            assert_same(&v, id);
        }
        let outer = symbol_ids(&v, "x.rs")
            .into_iter()
            .find(|&id| v.get(id).unwrap().unwrap().name == "Outer")
            .unwrap();
        // "Inner" and "t1" both start at byte 10: the symbol must win the tie.
        assert_eq!(names(v.children(outer).unwrap()), ["t0", "Inner", "t1"]);
    }

    /// Decode-cost regression (mirrors `ancestors_does_not_decode_the_whole_stream`):
    /// `children` on a small symbol deep in a file with many tokens outside
    /// it decodes far fewer token records than the file's total, bounded by
    /// roughly the symbol's own subtree plus checkpoint overhead, not the
    /// whole file.
    #[test]
    fn children_of_a_small_symbol_does_not_decode_the_whole_file() {
        let n = 20 * codec::CHECKPOINT_EVERY;
        let d = tempfile::tempdir().unwrap();
        let v = V2Store::open(d.path().join("c.redb")).unwrap();
        // One small symbol (3 tokens) near the end of a file with n tokens
        // total, all outside any other symbol.
        let toks: Vec<(String, u32, u32)> = (0..n)
            .map(|i| (format!("t{i}"), i as u32, i as u32 + 1))
            .collect();
        let small_start = n - 3;
        let tok_refs: Vec<(&str, u32, u32)> =
            toks.iter().map(|(t, s, e)| (t.as_str(), *s, *e)).collect();
        let ex = span_ext(
            &[("Small", SymbolKind::Function, small_start as u32, n as u32)],
            &tok_refs,
        );
        v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        let small = symbol_ids(&v, "x.rs")[0];

        codec::RECORDS_DECODED.with(|c| c.set(0));
        let kids = v.children(small).unwrap();
        let decoded = codec::RECORDS_DECODED.with(|c| c.get());

        assert_eq!(kids.len(), 3);
        let ratio = decoded as f64 / n as f64;
        println!(
            "children_of_a_small_symbol_does_not_decode_the_whole_file: \
             decoded {decoded} of {n} token records ({ratio:.5}x)"
        );
        assert!(
            decoded <= 3 + codec::CHECKPOINT_EVERY,
            "decoded {decoded} of {n} token records"
        );
    }
}

/// ADR 0003 story 3, slice 3i real-corpus differential: this repo's own
/// `crates/` tree, indexed with the real `RustExtractor`, checked the same
/// way as the generated proptest corpus above (children/descendants,
/// range-based vs. the literal pre-3i fallback, on every symbol).
#[test]
fn ranged_children_match_fallback_on_this_repos_own_corpus() {
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
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
    let mut files = Vec::new();
    // From `crates/graph-store`, `../../crates` is this repo's `crates/` tree.
    walk(std::path::Path::new("../../crates"), &mut files);
    assert!(
        !files.is_empty(),
        "expected to find this repo's own .rs files"
    );
    files.sort();

    use graph_core::Extractor;
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("corpus.redb")).unwrap();
    let extractor = graph_lang_rust::RustExtractor;
    let mut checked_symbols = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.to_string_lossy().replace('\\', "/");
        let ex = extractor.extract(&src);
        if v.ingest_file("o", "r", &rel, "rust", &ex).is_err() {
            continue;
        }
        // `SymbolHit` carries no node id (ADR 0003 story 4), so get every
        // symbol id for this file the same way `ranged_children::symbol_ids`
        // does: the file's raw entity id from a token id, then
        // `descendants(file)` (unaffected by this slice) filtered to symbols.
        let Some(toks) = v.file_tokens("o", "r", &rel).unwrap() else {
            continue;
        };
        let Some(any) = toks.first().map(|n| n.id) else {
            continue;
        };
        let file = (any >> 32) & 0x3fff_ffff;
        let sym_ids: Vec<NodeId> = v
            .descendants(file)
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == graph_core::NodeKind::Symbol)
            .map(|n| n.id)
            .collect();
        for id in sym_ids {
            assert_eq!(
                v.children(id).unwrap(),
                v.children_via_fallback(id).unwrap(),
                "children differ for {rel} symbol {id}"
            );
            assert_eq!(
                v.descendants(id).unwrap(),
                v.descendants_via_fallback(id).unwrap(),
                "descendants differ for {rel} symbol {id}"
            );
            checked_symbols += 1;
        }
    }
    assert!(
        checked_symbols > 100,
        "expected a substantial number of real symbols checked, got {checked_symbols}"
    );
}

/// ADR 0003 story 3, slice 3j spike measurement (gated on its own number,
/// per the scoping plan): for every file in this repo's own `crates/` tree,
/// compares the token records the pre-3j eager `stream()` decode reads
/// against what the range-based top-level path (`children_ranged_file`,
/// now wired into `children`/`descendants` on a file) actually decodes via
/// `Lazy::tokens_at`. Committed so the number is reproducible, not just
/// quoted in a PR description.
///
/// **Measured 97.78% reduction** (133,772 eager vs. 2,970 ranged token
/// records across 27 files), well past the scoping plan's 30% build gate,
/// so this slice wires the optimization in for real (see
/// `ranged_children_match_fallback_on_this_repos_own_corpus_at_file_level`
/// and `children_of_a_file_does_not_decode_the_whole_file` below).
#[test]
fn file_level_complement_decode_cost_measured_on_this_repos_own_corpus() {
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
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
    let mut files = Vec::new();
    walk(std::path::Path::new("../../crates"), &mut files);
    assert!(
        !files.is_empty(),
        "expected to find this repo's own .rs files"
    );
    files.sort();

    use graph_core::Extractor;
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("corpus.redb")).unwrap();
    let extractor = graph_lang_rust::RustExtractor;
    let (mut total_eager, mut total_complement, mut total_ntok) = (0usize, 0usize, 0usize);
    let mut files_measured = 0usize;
    let mut files_not_dense = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.to_string_lossy().replace('\\', "/");
        let ex = extractor.extract(&src);
        if v.ingest_file("o", "r", &rel, "rust", &ex).is_err() {
            continue;
        }
        let Some(toks) = v.file_tokens("o", "r", &rel).unwrap() else {
            continue;
        };
        let Some(any) = toks.first().map(|n| n.id) else {
            continue;
        };
        let file = (any >> 32) & 0x3fff_ffff;
        match v.measure_file_level_complement(file).unwrap() {
            Some((ntok, eager, complement)) => {
                total_ntok += ntok;
                total_eager += eager;
                total_complement += complement;
                files_measured += 1;
            }
            None => files_not_dense += 1,
        }
    }
    assert!(
        files_measured > 10,
        "expected a substantial number of real files measured, got {files_measured}"
    );
    assert_eq!(
        total_eager, total_ntok,
        "eager children(file)/descendants(file) decodes every token record"
    );
    let reduction_pct = 100.0 * (1.0 - total_complement as f64 / total_eager as f64);
    println!(
        "file_level_complement_decode_cost_measured_on_this_repos_own_corpus: \
         {files_measured} files ({files_not_dense} not dense/skipped), \
         eager {total_eager} vs complement {total_complement} token records \
         decoded ({reduction_pct:.2}% reduction)"
    );
    // The scoping plan's own build gate: only worth wiring in if it beats a
    // 30% reduction in decoded-record count on this repo's own corpus. This
    // assertion is NOT that go/no-go call -- it is a sanity bound on the
    // measurement itself (the complement can never cost more than the
    // eager path, since it decodes a subset of the same records), left here
    // so a future regression in the measurement code fails loudly.
    assert!(
        total_complement <= total_eager,
        "complement decode ({total_complement}) exceeded the eager baseline ({total_eager})"
    );
    // The scoping plan's actual go/no-go gate, now that the optimization is
    // built: fails loudly if a future change to this repo's corpus or the
    // codec regresses the win below the threshold that justified building it.
    assert!(
        reduction_pct > 30.0,
        "file-level complement reduction {reduction_pct:.2}% no longer clears the 30% build gate"
    );
}

/// ADR 0003 story 3, slice 3j real-corpus differential, file-level analog of
/// `ranged_children_match_fallback_on_this_repos_own_corpus`: `children`/
/// `descendants` on every file of this repo's own `crates/` tree, range-based
/// vs. the literal pre-3j fallback.
#[test]
fn ranged_children_match_fallback_on_this_repos_own_corpus_at_file_level() {
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
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
    let mut files = Vec::new();
    walk(std::path::Path::new("../../crates"), &mut files);
    assert!(
        !files.is_empty(),
        "expected to find this repo's own .rs files"
    );
    files.sort();

    use graph_core::Extractor;
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("corpus.redb")).unwrap();
    let extractor = graph_lang_rust::RustExtractor;
    let mut checked_files = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.to_string_lossy().replace('\\', "/");
        let ex = extractor.extract(&src);
        if v.ingest_file("o", "r", &rel, "rust", &ex).is_err() {
            continue;
        }
        let Some(toks) = v.file_tokens("o", "r", &rel).unwrap() else {
            continue;
        };
        let Some(any) = toks.first().map(|n| n.id) else {
            continue;
        };
        let file = (any >> 32) & 0x3fff_ffff;
        assert_eq!(
            v.children(file).unwrap(),
            v.children_via_fallback_file(file).unwrap(),
            "children(file) differ for {rel}"
        );
        assert_eq!(
            v.descendants(file).unwrap(),
            v.descendants_via_fallback_file(file).unwrap(),
            "descendants(file) differ for {rel}"
        );
        checked_files += 1;
    }
    assert!(
        checked_files > 10,
        "expected a substantial number of real files checked, got {checked_files}"
    );
}

/// Decode-cost regression (mirrors `children_of_a_small_symbol_does_not_decode_the_whole_file`):
/// `children(file)`/`descendants(file)` on a file whose tokens are almost
/// entirely covered by one top-level symbol decode far fewer token records
/// than the file's total, bounded by roughly the uncovered gap plus
/// checkpoint overhead, not the whole file.
#[test]
fn children_of_a_file_does_not_decode_the_whole_file() {
    let n = 20 * codec::CHECKPOINT_EVERY;
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("f.redb")).unwrap();
    // One top-level symbol covering all but the last 3 tokens, which sit
    // outside any symbol (the top-level "gap" `children`/`descendants` must
    // still find).
    let toks: Vec<(String, u32, u32)> = (0..n)
        .map(|i| (format!("t{i}"), i as u32, i as u32 + 1))
        .collect();
    let sym_end = n - 3;
    let tok_refs: Vec<(&str, u32, u32)> =
        toks.iter().map(|(t, s, e)| (t.as_str(), *s, *e)).collect();
    let ex = span_ext(
        &[("Big", SymbolKind::Function, 0, sym_end as u32)],
        &tok_refs,
    );
    v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    let toks = v.file_tokens("o", "r", "x.rs").unwrap().unwrap();
    let file = (toks[0].id >> 32) & 0x3fff_ffff;

    codec::RECORDS_DECODED.with(|c| c.set(0));
    let kids = v.children(file).unwrap();
    let decoded = codec::RECORDS_DECODED.with(|c| c.get());

    // One symbol plus the three top-level tokens outside it.
    assert_eq!(kids.len(), 4);
    let ratio = decoded as f64 / n as f64;
    println!(
        "children_of_a_file_does_not_decode_the_whole_file: \
         decoded {decoded} of {n} token records ({ratio:.5}x)"
    );
    assert!(
        decoded <= 3 + codec::CHECKPOINT_EVERY,
        "decoded {decoded} of {n} token records"
    );

    codec::RECORDS_DECODED.with(|c| c.set(0));
    let desc = v.descendants(file).unwrap();
    let desc_decoded = codec::RECORDS_DECODED.with(|c| c.get());
    // Symbol + its n-3 direct tokens + 3 top-level gap tokens = n + 1 nodes,
    // but `descendants_ranged_file` decodes the symbol's own subtree (all
    // n - 3 tokens under it) plus the 3-token gap: bounded by the file's
    // token count here (the symbol covers almost everything), unlike
    // `children`, which only needs the gap.
    assert_eq!(desc.len(), n + 1);
    assert!(
        desc_decoded <= n + codec::CHECKPOINT_EVERY,
        "decoded {desc_decoded} of {n} token records"
    );
}

/// File-level analog of `a_zero_width_symbol_beats_a_token_at_the_same_start`:
/// locks in the symbol-vs-token tie-break rule ("symbols before tokens on a
/// tie") for `children_ranged_file`'s merge loop specifically. QA review (PR
/// #38) found this exact bug class was only caught incidentally by an
/// unrelated fixture (`run_differential`'s `eq.rs`), which would
/// silently stop covering it if that fixture is ever edited -- a dedicated,
/// intention-revealing test closes that gap, matching slice 3i's precedent.
#[test]
fn a_zero_width_top_level_symbol_beats_a_top_level_token_at_the_same_start() {
    let ex = span_ext(
        &[("Z", SymbolKind::Type, 10, 10)],
        &[("t0", 5, 6), ("t1", 10, 11)],
    );
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("tie.redb")).unwrap();
    v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    let toks = v.file_tokens("o", "r", "x.rs").unwrap().unwrap();
    let file = (toks[0].id >> 32) & 0x3fff_ffff;
    // "Z" and "t1" both start at byte 10: the symbol must win the tie.
    assert_eq!(names(v.children(file).unwrap()), ["t0", "Z", "t1"]);
    assert_eq!(
        v.children(file).unwrap(),
        v.children_via_fallback_file(file).unwrap()
    );
}

/// Issue #39: slice 3j (file-level `children`/`descendants`) relied solely
/// on this repo's own 27-file corpus (`ranged_children_match_fallback_on_this_repos_own_corpus_at_file_level`)
/// for its differential coverage, unlike slice 3i's symbol-level equivalent
/// (`ranged_children::range_based_and_fallback_agree_on_every_symbol`),
/// which additionally runs a 200-case generated-corpus proptest. This is the
/// file-level analog of that proptest: generates the same kind of
/// nested/disjoint symbol+token corpora (via `ranged_children`'s own
/// `corpus`/`build` helpers, so both levels are checked against exactly the
/// same distribution of shapes) and compares `children(file)`/
/// `descendants(file)` (the range-based `children_ranged_file`/
/// `descendants_ranged_file` path) against the literal pre-3j fallback.
mod ranged_children_file_level {
    use super::*;
    use proptest::prelude::*;

    fn file_id(v: &V2Store, path: &str) -> Option<NodeId> {
        let toks = v.file_tokens("o", "r", path).unwrap()?;
        let any = toks.first()?.id;
        Some((any >> 32) & 0x3fff_ffff)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
        /// For every generated file, the range-based `children(file)`/
        /// `descendants(file)` and the literal pre-3j fallback return
        /// identical `Vec<Node>` (same ids, order and spans) -- including
        /// when the file has no top-level symbols at all (an all-tokens
        /// file, or an empty one), which exercises the pure-gap path.
        #[test]
        fn range_based_and_fallback_agree_on_every_file(
            (n, syms) in super::ranged_children::corpus()
        ) {
            let d = tempfile::tempdir().unwrap();
            let v = V2Store::open(d.path().join("r.redb")).unwrap();
            let ex = super::ranged_children::build(n, &syms);
            v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
            if let Some(file) = file_id(&v, "x.rs") {
                prop_assert_eq!(
                    v.children(file).unwrap(),
                    v.children_via_fallback_file(file).unwrap()
                );
                prop_assert_eq!(
                    v.descendants(file).unwrap(),
                    v.descendants_via_fallback_file(file).unwrap()
                );
            }
        }
    }
}

#[test]
fn vacuum_keeps_a_symbol_only_lang_kind_term() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("k.redb")).unwrap();
    let mut ex = span_ext(&[("Sym", SymbolKind::Type, 0, 9)], &[]);
    // The kind text is referenced by the symbol record only: no posting, no name.
    ex.symbols[0].lang_kind = Some("struct_item".into());
    s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    assert_eq!(
        s.vacuum().unwrap(),
        VacuumStats {
            terms_removed: 0,
            terms_kept: 2
        }
    );
    let hits = s.search_symbols(&SymbolQuery::new("Sym")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].lang_kind.as_deref(), Some("struct_item"));
}

// --- ADR 0003 story 11: paging (Store::children_page/descendants_page, Query/SymbolQuery::offset) ---

/// Paging through a result set larger than one page (`children_page` on a
/// fallback file with many direct token children -- the epic's story 12
/// "fallback files" case: a File with no language extractor, so no Symbol
/// nodes, lists every Token as a direct child) returns every item exactly
/// once, in the same order `children` returns them, across every page, in
/// both configurations.
#[test]
fn paging_children_covers_every_item_once_in_order() {
    let (_d, a, b) = two_configs();
    // "text" has no registered extractor: a fallback file, tokens only.
    let src: String = (0..250).map(|i| format!("t{i} ")).collect();
    let words = tokenize_words(&src);
    for s in [&a, &b] {
        s.ingest_file("o", "r", "big.txt", "text", &span_ext(&[], &words))
            .unwrap();
    }
    for s in [&a, &b] {
        let org = s.roots().unwrap()[0].id;
        let repo = s.children(org).unwrap()[0].id;
        let file = s.children(repo).unwrap()[0].id;
        let full = s.children(file).unwrap();
        assert_eq!(
            full.len(),
            250,
            "every token is a direct child of a fallback file"
        );

        let mut paged = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = s.children_page(file, offset, 32).unwrap();
            assert!(page.items.len() <= 32);
            let more = page.has_more;
            let got = page.items.len();
            paged.extend(page.items);
            if !more {
                break;
            }
            offset += got;
        }
        assert_eq!(
            paged, full,
            "paged union equals the unpaged list, same order"
        );
        let ids: std::collections::HashSet<_> = paged.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), paged.len(), "no id twice across pages");

        // Past the end: empty, has_more false.
        let past = s.children_page(file, 10_000, 10).unwrap();
        assert!(past.items.is_empty() && !past.has_more);

        // limit:0 mid-list: empty items, but has_more is true -- it still
        // reflects whether more data exists behind this (empty) page, not
        // whether this call returned anything.
        let zero = s.children_page(file, 0, 0).unwrap();
        assert!(zero.items.is_empty() && zero.has_more);
        // limit:0 past the end: empty items, has_more false (no data left).
        let zero_past = s.children_page(file, 10_000, 0).unwrap();
        assert!(zero_past.items.is_empty() && !zero_past.has_more);
    }
}

/// Same coverage guarantee for `descendants_page` over a whole org (mixed
/// repo/file/symbol/token levels), on both configurations.
#[test]
fn paging_descendants_covers_every_item_once_in_order() {
    let (_d, a, b) = two_configs();
    for s in [&a, &b] {
        for i in 0..8 {
            s.ingest_file(
                "o",
                "r",
                &format!("f{i}.rs"),
                "rust",
                &span_ext(
                    &[("S", SymbolKind::Function, 0, 4)],
                    &[("alpha", 0, 4), ("beta", 5, 9)],
                ),
            )
            .unwrap();
        }
    }
    for s in [&a, &b] {
        let org = s.roots().unwrap()[0].id;
        let full = s.descendants(org).unwrap();
        assert!(full.len() > 10, "enough nodes to span multiple pages");

        let mut paged = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = s.descendants_page(org, offset, 7).unwrap();
            let more = page.has_more;
            let got = page.items.len();
            paged.extend(page.items);
            if !more {
                break;
            }
            offset += got.max(1);
        }
        assert_eq!(paged, full);
    }
}

/// Paged `search` (`Query::offset` + `Query::limit`) reconstructs exactly the
/// same rows, in the same order, as one unpaged call, on both configurations.
#[test]
fn paging_search_offset_limit_matches_full_results() {
    let (_d, a, b) = two_configs();
    for s in [&a, &b] {
        for i in 0..12 {
            s.ingest_file(
                "o",
                "r",
                &format!("f{i}.rs"),
                "rust",
                &span_ext(&[], &[("needle", 0, 6)]),
            )
            .unwrap();
        }
    }
    for s in [&a, &b] {
        let full = s.search(&Query::new("needle")).unwrap();
        assert_eq!(full.len(), 12);
        let mut paged = Vec::new();
        let mut offset = 0usize;
        loop {
            let mut q = Query::new("needle");
            q.offset = Some(offset);
            q.limit = Some(5);
            let page = s.search(&q).unwrap();
            let got = page.len();
            paged.extend(page);
            if got < 5 {
                break;
            }
            offset += got;
        }
        assert_eq!(paged, full, "paged search equals the unpaged list");
    }
}

/// Paged `search_symbols` (`SymbolQuery::offset` + `limit`) likewise
/// reconstructs the unpaged result, on both configurations.
#[test]
fn paging_search_symbols_offset_limit_matches_full_results() {
    let (_d, a, b) = two_configs();
    for s in [&a, &b] {
        for i in 0..9 {
            s.ingest_file(
                "o",
                "r",
                &format!("f{i}.rs"),
                "rust",
                &span_ext(&[("S", SymbolKind::Function, 0, 4)], &[]),
            )
            .unwrap();
        }
    }
    for s in [&a, &b] {
        let full = s.search_symbols(&SymbolQuery::new("S")).unwrap();
        assert_eq!(full.len(), 9);
        let mut paged = Vec::new();
        let mut offset = 0usize;
        loop {
            let mut q = SymbolQuery::new("S");
            q.offset = Some(offset);
            q.limit = Some(4);
            let page = s.search_symbols(&q).unwrap();
            let got = page.len();
            paged.extend(page);
            if got < 4 {
                break;
            }
            offset += got;
        }
        assert_eq!(paged, full);
    }
}

/// Paging is snapshot-consistent: pages already fetched, and pages fetched
/// later in the same sequence, come from the one frozen read the snapshot
/// took, not from a concurrent writer's changes -- in both configurations.
#[test]
fn paging_is_snapshot_consistent_across_concurrent_writes() {
    let (_d, a, b) = two_configs();
    for s in [&a, &b] {
        for i in 0..6 {
            s.ingest_file(
                "o",
                "r",
                &format!("f{i}.rs"),
                "rust",
                &span_ext(&[], &[("orig", 0, 4)]),
            )
            .unwrap();
        }
        let snap = s.snapshot().unwrap();
        let org = snap.roots().unwrap()[0].id;
        let repo = snap.children(org).unwrap()[0].id;

        // First page from the snapshot.
        let page1 = snap.children_page(repo, 0, 3).unwrap();
        assert_eq!(page1.items.len(), 3);
        assert!(page1.has_more);

        // Mutate the live store: ingest a new file, replace an existing one.
        s.ingest_file(
            "o",
            "r",
            "new.rs",
            "rust",
            &span_ext(&[], &[("orig", 0, 4)]),
        )
        .unwrap();
        s.ingest_file(
            "o",
            "r",
            "f0.rs",
            "rust",
            &span_ext(&[], &[("changed", 0, 7)]),
        )
        .unwrap();

        // Second page, fetched AFTER the mutation, still comes from the
        // frozen snapshot: total children of the repo is still 6, not 7,
        // and the two pages together equal the pre-mutation full list.
        let page2 = snap.children_page(repo, 3, 3).unwrap();
        assert_eq!(page2.items.len(), 3);
        assert!(
            !page2.has_more,
            "still 6 files, not 7, through the frozen snapshot"
        );
        let mut all: Vec<_> = page1
            .items
            .iter()
            .chain(&page2.items)
            .map(|n| n.name.clone())
            .collect();
        all.sort();
        let mut want: Vec<String> = (0..6).map(|i| format!("f{i}.rs")).collect();
        want.sort();
        assert_eq!(
            all, want,
            "snapshot paging sees none of the concurrent writes"
        );

        // A fresh snapshot after the writes does see them.
        let fresh = s.snapshot().unwrap();
        let fresh_repo = fresh.children(fresh.roots().unwrap()[0].id).unwrap()[0].id;
        assert_eq!(fresh.children(fresh_repo).unwrap().len(), 7);
    }
}

fn tokenize_words(src: &str) -> Vec<(&str, u32, u32)> {
    let mut out = Vec::new();
    let mut pos = 0u32;
    for w in src.split_whitespace() {
        let start = src[pos as usize..].find(w).unwrap() as u32 + pos;
        out.push((w, start, start + w.len() as u32));
        pos = start + w.len() as u32;
    }
    out
}

// --- issue #40: descendants_ranged_file quadratic symbol-map-rebuild cost ---

/// Builds a synthetic single-file extraction with `n` disjoint top-level
/// symbols, each covering one token, so every top-level symbol's own subtree
/// walk is O(1) work and the only thing that can scale worse than linearly
/// in `n` is the symbol-to-children map itself.
fn many_top_level_symbols_ext(n: usize) -> graph_core::Extraction {
    let syms: Vec<(String, SymbolKind, u32, u32)> = (0..n)
        .map(|i| {
            let base = (i * 10) as u32;
            (format!("Sym{i}"), SymbolKind::Function, base, base + 1)
        })
        .collect();
    let sym_refs: Vec<(&str, SymbolKind, u32, u32)> = syms
        .iter()
        .map(|(n, k, s, e)| (n.as_str(), *k, *s, *e))
        .collect();
    let toks: Vec<(String, u32, u32)> = (0..n)
        .map(|i| {
            let base = (i * 10) as u32;
            (format!("t{i}"), base, base + 1)
        })
        .collect();
    let tok_refs: Vec<(&str, u32, u32)> =
        toks.iter().map(|(t, s, e)| (t.as_str(), *s, *e)).collect();
    span_ext(&sym_refs, &tok_refs)
}

/// Differential: `descendants_ranged_file`'s output for a file with many
/// disjoint top-level symbols is identical to the pre-optimization eager
/// fallback walk (`descendants_via_fallback_file`), both before and after
/// sharing the symbol-to-children map across top-level symbols (issue #40).
/// This is the correctness half of the fix -- the perf half is
/// `descendants_ranged_file_symbol_decode_cost_is_linear_not_quadratic`
/// below.
#[test]
fn descendants_ranged_file_matches_fallback_for_many_top_level_symbols() {
    let d = tempfile::tempdir().unwrap();
    let v = V2Store::open(d.path().join("many.redb")).unwrap();
    let ex = many_top_level_symbols_ext(60);
    v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    let toks = v.file_tokens("o", "r", "x.rs").unwrap().unwrap();
    let file = (toks[0].id >> 32) & 0x3fff_ffff;

    let ranged = v.descendants(file).unwrap();
    let fallback = v.descendants_via_fallback_file(file).unwrap();
    assert_eq!(
        ranged, fallback,
        "descendants_ranged_file diverges from the eager fallback for a \
         file with many top-level symbols"
    );
    // 60 symbols + 60 tokens, one token directly under each symbol.
    assert_eq!(ranged.len(), 120);
}

/// Performance regression guard for issue #40: before the fix,
/// `descendants_ranged_file` rebuilt the whole file's symbol table (via
/// `Lazy::symbols()`) once per top-level symbol, so the number of symbol
/// records decoded scaled with `n^2` for a file with `n` top-level symbols
/// (`n` rebuilds x `n` symbols each). After the fix it is built once per
/// call to `descendants_ranged_file`, so the count scales with `n` (one
/// rebuild x `n` symbols).
///
/// This asserts the *ratio* of decoded symbol records between a 3x-larger
/// file and a smaller one stays close to linear (~3x), not the ~9x a
/// quadratic rebuild would produce.
#[test]
fn descendants_ranged_file_symbol_decode_cost_is_linear_not_quadratic() {
    fn decoded_symbol_records(n: usize) -> usize {
        let d = tempfile::tempdir().unwrap();
        let v = V2Store::open(d.path().join("n.redb")).unwrap();
        let ex = many_top_level_symbols_ext(n);
        v.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
        let toks = v.file_tokens("o", "r", "x.rs").unwrap().unwrap();
        let file = (toks[0].id >> 32) & 0x3fff_ffff;

        codec::SYM_RECORDS_DECODED.with(|c| c.set(0));
        let out = v.descendants(file).unwrap();
        assert_eq!(out.len(), 2 * n);
        codec::SYM_RECORDS_DECODED.with(|c| c.get())
    }

    let small = decoded_symbol_records(25);
    let large = decoded_symbol_records(75);
    let ratio = large as f64 / small as f64;
    println!(
        "descendants_ranged_file_symbol_decode_cost_is_linear_not_quadratic: \
         n=25 -> {small} symbol records decoded, n=75 -> {large} \
         ({ratio:.2}x for a 3x larger file)"
    );
    // Linear scaling (one symbol-table decode per call, `nsym` records each)
    // gives ~3x. Quadratic (pre-fix: one decode per top-level symbol) would
    // give ~9x. 5x is a generous cutoff that still clearly separates the two.
    assert!(
        ratio <= 5.0,
        "decoded symbol record count scaled {ratio:.2}x for a 3x larger \
         file (small={small}, large={large}); expected close to linear \
         (~3x), not quadratic (~9x) -- issue #40 regressed"
    );
}

/// v2 chunk accounting through `index_prepared` (shared with `index_batch`):
/// only stored files count towards `chunk_bytes`, and a chunk commits once
/// it reaches the cap, so a later whole-batch error keeps earlier chunks.
#[test]
fn prepared_chunks_count_only_stored_files() {
    let d = tempfile::tempdir().unwrap();
    let mut s = V2Store::open(d.path().join("g")).unwrap();
    let f = |p, b: &'static [u8], l| BatchFile {
        path: p,
        bytes: b,
        language: Some(l),
        origin: None,
    };
    let opts = IndexOptions::default();
    // A big unchanged file must not count: with cap 6, only "small" (5
    // bytes) is stored before the bad file, so no chunk commits.
    s.index_batch("o", "r", &[f("big.txt", b"0123456789", "text")], opts)
        .unwrap();
    s.set_chunk_bytes(6);
    let files = [
        f("big.txt", b"0123456789", "text"),
        f("small.txt", b"abcde", "text"),
        f("bad.txt", b"x", "a\0b"),
    ];
    let p: Vec<_> = files
        .iter()
        .map(|x| Store::prepare(&s, "o", "r", x, opts).unwrap())
        .collect();
    assert!(Store::index_prepared(&s, "o", "r", p, opts).is_err());
    assert!(
        s.file_tokens("o", "r", "small.txt").unwrap().is_none(),
        "skipped file counted towards the chunk"
    );
    // Stored files do count: two 5-byte files reach cap 6 after the second
    // only if both count; with cap 5 the first commits alone.
    s.set_chunk_bytes(5);
    let files = [f("small.txt", b"abcde", "text"), f("bad.txt", b"x", "a\0b")];
    let p: Vec<_> = files
        .iter()
        .map(|x| Store::prepare(&s, "o", "r", x, opts).unwrap())
        .collect();
    assert!(Store::index_prepared(&s, "o", "r", p, opts).is_err());
    assert!(
        s.file_tokens("o", "r", "small.txt").unwrap().is_some(),
        "a full chunk commits before the failing file"
    );
}
