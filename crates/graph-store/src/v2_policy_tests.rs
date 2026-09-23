//! ADR 0003 story 3 leftovers on v2: no-op vacuum, term-length policy,
//! chunked commits and the consistency proptest.
use super::*;
use crate::v2::{hashed_key, MAX_INLINE_TERM, R};
use crate::v2_tests::{both_backends, span_ext};

fn sha(p: &std::path::Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(std::fs::read(p).unwrap()).to_vec()
}

/// `open_with_cache_bytes` is `open` with an explicit redb cache size; a
/// tiny cache must not change what is stored or read back, and `None`
/// (redb's default) must behave exactly like `open`.
#[test]
fn open_with_cache_bytes_does_not_change_results() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open_with_cache_bytes(&p, Some(1)).unwrap();
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &span_ext(&[("S", SymbolKind::Function, 0, 9)], &[("alpha", 1, 2)]),
    )
    .unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    drop(s);

    // Reopening with a different (or no) cache size sees the same data.
    let s = V2Store::open_with_cache_bytes(&p, Some(1024 * 1024)).unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    drop(s);

    let s = V2Store::open_with_cache_bytes(&p, None).unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
}

#[test]
fn a_no_op_vacuum_leaves_the_file_byte_identical() {
    // The file is hashed only while no `V2Store` handle is open on it: on
    // Windows, redb holds a lock on the file for the life of the handle,
    // and a bare `fs::read` while that handle is live hits a sharing
    // violation (the store itself has no such restriction otherwise).
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &span_ext(&[("S", SymbolKind::Function, 0, 9)], &[("alpha", 1, 2)]),
    )
    .unwrap();
    drop(s);
    let before = sha(&p);

    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.vacuum().unwrap().terms_removed, 0);
    drop(s);
    assert_eq!(sha(&p), before, "no-op vacuum must not write");

    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.vacuum().unwrap().terms_removed, 0);
    drop(s);
    assert_eq!(sha(&p), before);

    // Sanity: a vacuum that does remove something does write.
    let s = V2Store::open(&p).unwrap();
    s.ingest_file("o", "r", "x.rs", "rust", &span_ext(&[], &[("beta", 1, 2)]))
        .unwrap();
    drop(s);
    let mid = sha(&p);

    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.vacuum().unwrap().terms_removed, 2, "S and alpha");
    drop(s);
    assert_ne!(sha(&p), mid);

    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.search(&Query::new("beta")).unwrap().len(), 1);
}

/// Token texts of every interesting length.
fn long_term_texts() -> Vec<String> {
    let long = "L".repeat(MAX_INLINE_TERM + 44);
    vec![
        "a".repeat(MAX_INLINE_TERM - 1),
        "b".repeat(MAX_INLINE_TERM),
        "c".repeat(MAX_INLINE_TERM + 1),
        long.clone(),
        "d".repeat(100_000),
        "\0nul".to_string(),
        // A short term shaped exactly like a hashed key of a long one.
        hashed_key(&long, 0),
        "plain".to_string(),
        // Multi-byte: over the cap in bytes, under it in characters.
        "é".repeat(MAX_INLINE_TERM),
    ]
}

fn long_term_extraction(texts: &[String], sym: &str) -> graph_core::Extraction {
    let mut toks: Vec<(&str, u32, u32)> = Vec::new();
    let mut at = 0u32;
    for t in texts {
        toks.push((t.as_str(), at, at + t.len() as u32));
        at += t.len() as u32 + 1;
    }
    span_ext(&[(sym, SymbolKind::Function, 0, at)], &toks)
}

#[test]
fn very_long_terms_search_exactly_like_v1() {
    let (_d, a, b) = both_backends();
    let texts = long_term_texts();
    let sym = "S".repeat(MAX_INLINE_TERM * 4);
    let ex = long_term_extraction(&texts, &sym);
    for s in [&a, &b] {
        s.ingest_file("o", "r", "big.txt", "text", &ex).unwrap();
        // The same terms again in a second file (interned once).
        s.ingest_file("o", "r", "again.txt", "text", &ex).unwrap();
    }
    for t in texts.iter().map(String::as_str).chain(["missing"]) {
        for grain in [Grain::Token, Grain::Symbol, Grain::File, Grain::Repo] {
            let mut q = Query::new(t);
            q.grain = grain;
            let (ha, hb) = (a.search(&q).unwrap(), b.search(&q).unwrap());
            assert_eq!(ha, hb, "len {} {grain:?}", t.len());
            if t != "missing" && grain == Grain::Token {
                assert_eq!(hb.len(), 2, "len {}", t.len());
            }
        }
    }
    for pat in [sym.as_str(), "SSS*", "*"] {
        let q = SymbolQuery::new(pat);
        assert_eq!(a.search_symbols(&q).unwrap(), b.search_symbols(&q).unwrap());
    }
    assert_eq!(
        a.describe(None, None).unwrap(),
        b.describe(None, None).unwrap()
    );
    for f in ["big.txt", "again.txt"] {
        let text = |s: &dyn Store| -> Vec<_> {
            s.file_tokens("o", "r", f)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|n| (n.name, n.span))
                .collect()
        };
        assert_eq!(text(&*a), text(&*b), "exact texts and spans");
    }
    crate::conformance::run_differential(&*a, &*b);
}

#[test]
fn long_terms_are_hashed_keys_and_survive_replace_and_vacuum() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("l.redb")).unwrap();
    let texts = long_term_texts();
    s.ingest_file(
        "o",
        "r",
        "x.txt",
        "text",
        &long_term_extraction(&texts, "s"),
    )
    .unwrap();
    s.check_consistency(false);
    {
        let rt = s.db.begin_read().unwrap();
        let r = R::new(&rt).unwrap();
        for t in &texts {
            let hashed = t.len() > MAX_INLINE_TERM || t.starts_with('\0');
            // Inline terms are their own key; hashed ones never are.
            // (The forged-key text is itself a key: the long term's.)
            let is_a_key = texts.iter().any(|o| hashed_key(o, 0) == *t);
            assert_eq!(
                r.dict.get(t.as_str()).unwrap().is_some(),
                !hashed || is_a_key,
                "len {}",
                t.len()
            );
            if hashed {
                assert!(r.dict.get(hashed_key(t, 0).as_str()).unwrap().is_some());
            }
        }
        // No key is longer than the inline cap: that is the point.
        for row in r.dict.iter().unwrap() {
            assert!(row.unwrap().0.value().len() <= MAX_INLINE_TERM);
        }
    }
    // Replace with only a short term: every long term dies, vacuum drops them.
    s.ingest_file(
        "o",
        "r",
        "x.txt",
        "text",
        &span_ext(&[], &[("plain", 0, 5)]),
    )
    .unwrap();
    s.check_consistency(false);
    let st = s.vacuum().unwrap();
    assert_eq!(
        st.terms_removed,
        texts.len(),
        "all but plain, plus symbol s"
    );
    s.check_consistency(true);
    for t in &texts {
        assert_eq!(
            s.search(&Query::new(t)).unwrap().len(),
            usize::from(t == "plain")
        );
    }
    // A dead long term can be interned again.
    s.ingest_file(
        "o",
        "r",
        "y.txt",
        "text",
        &long_term_extraction(&texts, "s"),
    )
    .unwrap();
    s.check_consistency(false);
    assert_eq!(s.search(&Query::new(&texts[4])).unwrap().len(), 1);
}

#[test]
fn a_digest_collision_probes_to_the_next_key() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("c.redb")).unwrap();
    s.ingest_file("o", "r", "a.txt", "text", &span_ext(&[], &[("zzz", 0, 3)]))
        .unwrap();
    let long = "Q".repeat(MAX_INLINE_TERM + 10);
    // Forge a collision: the long term's first key already names "zzz".
    {
        let wt = s.db.begin_write().unwrap();
        {
            let mut dict = wt.open_table(crate::v2::DICT).unwrap();
            let id = dict.get("zzz").unwrap().unwrap().value();
            dict.insert(hashed_key(&long, 0).as_str(), id).unwrap();
        }
        wt.commit().unwrap();
    }
    s.ingest_file(
        "o",
        "r",
        "b.txt",
        "text",
        &span_ext(&[], &[(long.as_str(), 0, 5), ("zzz", 6, 9)]),
    )
    .unwrap();
    assert_eq!(s.search(&Query::new(&long)).unwrap().len(), 1);
    assert_eq!(s.search(&Query::new("zzz")).unwrap().len(), 2);
    let rt = s.db.begin_read().unwrap();
    let r = R::new(&rt).unwrap();
    assert!(r.dict.get(hashed_key(&long, 1).as_str()).unwrap().is_some());
}

/// Extractor that makes the store fail the whole call (not just one file)
/// when the source contains `BAD`: a NUL in a symbol kind is rejected inside
/// the write transaction.
struct Poison;
impl graph_core::Extractor for Poison {
    fn language(&self) -> &str {
        "poison"
    }
    fn extract(&self, source: &str) -> graph_core::Extraction {
        let mut ex = span_ext(&[("S", SymbolKind::Function, 0, source.len() as u32)], &[]);
        if source.contains("BAD") {
            ex.symbols[0].lang_kind = Some("nul\0".into());
        }
        ex
    }
}

fn batch<'a>(srcs: &'a [String], paths: &'a [String]) -> Vec<BatchFile<'a>> {
    srcs.iter()
        .zip(paths)
        .map(|(s, p)| BatchFile {
            path: p,
            bytes: s.as_bytes(),
            language: Some("poison"),
            origin: None,
        })
        .collect()
}

fn paths(n: usize, ext: &str) -> Vec<String> {
    (0..n).map(|i| format!("f{i}.{ext}")).collect()
}

fn file_count(s: &V2Store) -> usize {
    s.count_nodes(NodeKind::File).unwrap()
}

#[test]
fn chunked_batches_are_atomic_per_chunk() {
    let d = tempfile::tempdir().unwrap();
    // Each source is 10 bytes; f2 is poisoned.
    let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
        .map(String::from)
        .to_vec();
    let ps = paths(4, "p");
    let opts = IndexOptions::default();

    // Cap of 20 bytes: {f0, f1} commit, then f2 fails and only its own chunk
    // {f2} is lost; f3 is never reached.
    let mut small = V2Store::open(d.path().join("s.redb")).unwrap();
    small.register(Box::new(Poison));
    small.set_chunk_bytes(20);
    assert!(V2Store::index_batch(&small, "o", "r", &batch(&srcs, &ps), opts).is_err());
    assert_eq!(file_count(&small), 2, "earlier chunks stay committed");
    small.check_consistency(false);
    // Re-running without the bad file skips what is stored and stores the rest.
    let ok = [srcs[0].clone(), srcs[1].clone(), srcs[3].clone()];
    let ok_paths = [ps[0].clone(), ps[1].clone(), ps[3].clone()];
    let res = V2Store::index_batch(&small, "o", "r", &batch(&ok, &ok_paths), opts).unwrap();
    let unchanged: Vec<bool> = res.iter().map(|r| r.as_ref().unwrap().unchanged).collect();
    assert_eq!(unchanged, [true, true, false]);
    assert_eq!(file_count(&small), 3);
    small.check_consistency(false);

    // Default cap: the same batch is one transaction, all or nothing.
    let mut big = V2Store::open(d.path().join("b.redb")).unwrap();
    big.register(Box::new(Poison));
    assert!(V2Store::index_batch(&big, "o", "r", &batch(&srcs, &ps), opts).is_err());
    assert_eq!(file_count(&big), 0, "one chunk: nothing stored");

    // A chunk that holds the failure loses everything in it: cap 40 puts
    // f0, f1, f2 and f3 in one chunk.
    let mut mid = V2Store::open(d.path().join("m.redb")).unwrap();
    mid.register(Box::new(Poison));
    mid.set_chunk_bytes(40);
    assert!(V2Store::index_batch(&mid, "o", "r", &batch(&srcs, &ps), opts).is_err());
    assert_eq!(file_count(&mid), 0);
}

#[test]
fn chunked_and_unchunked_batches_store_the_same_data() {
    let d = tempfile::tempdir().unwrap();
    let srcs: Vec<String> = (0..30)
        .map(|i| format!("fn f{i}() {{ let x{} = {i}; }}\n", i % 5))
        .collect();
    let ps = paths(30, "rs");
    let files: Vec<BatchFile<'_>> = srcs
        .iter()
        .zip(&ps)
        .map(|(s, p)| BatchFile {
            path: p,
            bytes: s.as_bytes(),
            language: Some("rust"),
            origin: None,
        })
        .collect();
    let one = V2Store::open(d.path().join("one.redb")).unwrap();
    let mut many = V2Store::open(d.path().join("many.redb")).unwrap();
    many.set_chunk_bytes(1); // every file is its own chunk
    let r1 = V2Store::index_batch(&one, "o", "r", &files, IndexOptions::default()).unwrap();
    let r2 = V2Store::index_batch(&many, "o", "r", &files, IndexOptions::default()).unwrap();
    let toks = |r: &[Result<IngestStats>]| -> Vec<usize> {
        r.iter().map(|r| r.as_ref().unwrap().tokens).collect()
    };
    assert_eq!(toks(&r1), toks(&r2));
    crate::conformance::run_differential(&one, &many);
    many.check_consistency(false);
}

mod consistency {
    use super::*;
    use graph_core::{Span, SymbolDecl, TokenClass, TokenDecl};
    use proptest::prelude::*;

    fn vocab() -> Vec<String> {
        vec![
            "a".into(),
            "b".into(),
            "L".repeat(MAX_INLINE_TERM + 5),
            "\0z".into(),
            "M".repeat(MAX_INLINE_TERM),
        ]
    }

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

    type Spec = (Vec<usize>, Vec<(usize, usize, usize)>);

    fn build(spec: &Spec) -> graph_core::Extraction {
        let v = vocab();
        let n = spec.0.len() as u32 + 1;
        graph_core::Extraction {
            has_errors: false,
            tokens: spec
                .0
                .iter()
                .enumerate()
                .map(|(i, &t)| TokenDecl {
                    text: v[t % v.len()].clone(),
                    class: TokenClass::Identifier,
                    span: sp(i as u32 * 4, i as u32 * 4 + 2),
                })
                .collect(),
            symbols: spec
                .1
                .iter()
                .map(|&(nm, a, b)| {
                    let (a, b) = (a as u32 % n, b as u32 % n);
                    SymbolDecl {
                        name: v[nm % v.len()].clone(),
                        kind: SymbolKind::Function,
                        lang_kind: (nm % 2 == 0).then(|| v[(nm + 1) % v.len()].clone()),
                        span: sp(a.min(b) * 4, a.max(b) * 4 + 3),
                    }
                })
                .collect(),
        }
    }

    fn spec() -> impl Strategy<Value = Spec> {
        (
            prop::collection::vec(0usize..5, 0..90),
            prop::collection::vec((0usize..5, 0usize..100, 0usize..100), 0..3),
        )
    }

    #[derive(Debug, Clone)]
    enum Op {
        Ingest(usize, Spec),
        Prune(Vec<bool>),
        Vacuum,
        Batch(Vec<(usize, u8)>),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0usize..4, spec()).prop_map(|(f, s)| Op::Ingest(f, s)),
            1 => prop::collection::vec(any::<bool>(), 4).prop_map(Op::Prune),
            1 => Just(Op::Vacuum),
            1 => prop::collection::vec((0usize..4, any::<u8>()), 1..5).prop_map(Op::Batch),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(40))]
        /// After any sequence of ingest, replace, prune, chunked batch and
        /// vacuum, every derived table equals what the streams imply, and a
        /// vacuum leaves no dead dictionary term. The oracle recomputes
        /// derived tables from the decoded streams; it does not check the
        /// streams against the input spec or against v1 (the differential
        /// tests cover that), and `describe_by_scan` is itself code under test.
        #[test]
        fn derived_tables_always_match_the_streams(ops in prop::collection::vec(op(), 1..10)) {
            let d = tempfile::tempdir().unwrap();
            let mut s = V2Store::open(d.path().join("p.redb")).unwrap();
            s.set_chunk_bytes(30);
            for op in ops {
                let mut vacuumed = false;
                match op {
                    Op::Ingest(f, spec) => {
                        // Overlapping symbols are rejected and change nothing.
                        let _ = s.ingest_file("o", "r", &format!("f{f}.x"), "text", &build(&spec));
                    }
                    Op::Prune(keep) => {
                        let set = (0..4).filter(|&i| keep[i]).map(|i| format!("f{i}.x")).collect();
                        s.prune_files("o", "r", &set, false).unwrap();
                    }
                    Op::Vacuum => {
                        s.vacuum().unwrap();
                        vacuumed = true;
                    }
                    Op::Batch(items) => {
                        let srcs: Vec<String> = items
                            .iter()
                            .map(|(_, b)| format!("word{} other{}", b % 3, b % 2))
                            .collect();
                        let ps: Vec<String> = items.iter().map(|(f, _)| format!("f{f}.x")).collect();
                        let files: Vec<BatchFile<'_>> = srcs
                            .iter()
                            .zip(&ps)
                            .map(|(src, p)| BatchFile {
                                path: p,
                                bytes: src.as_bytes(),
                                language: Some("text"),
                                origin: None,
                            })
                            .collect();
                        V2Store::index_batch(&s, "o", "r", &files, IndexOptions { reindex: true }).unwrap();
                    }
                }
                s.check_consistency(vacuumed);
            }
            s.vacuum().unwrap();
            s.check_consistency(true);
        }
    }
}

/// A rich fixture: several files across two repos, symbols, and one file
/// using every interesting term length (short, at/over the inline cap,
/// NUL-leading, multi-byte), so `compact`'s table-by-table copy is exercised
/// against both the small inline dictionary keys and the hashed ones.
fn compact_fixture(s: &V2Store) {
    s.ingest_file(
        "o",
        "r1",
        "a.rs",
        "rust",
        &span_ext(
            &[
                ("Outer", SymbolKind::Type, 0, 40),
                ("inner", SymbolKind::Method, 5, 20),
            ],
            &[("alpha", 21, 26), ("beta", 27, 31)],
        ),
    )
    .unwrap();
    s.ingest_file(
        "o",
        "r1",
        "b.rs",
        "rust",
        &span_ext(
            &[("Gamma", SymbolKind::Function, 0, 10)],
            &[("gamma", 1, 6)],
        ),
    )
    .unwrap();
    let texts = long_term_texts();
    let sym = "S".repeat(MAX_INLINE_TERM * 2);
    s.ingest_file(
        "o2",
        "r2",
        "long.rs",
        "rust",
        &long_term_extraction(&texts, &sym),
    )
    .unwrap();
}

/// Every query surface (`search`, `search_symbols`, `describe`, `file_tokens`)
/// returns exactly what it returned before compaction.
#[test]
fn compact_does_not_change_query_results() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    compact_fixture(&s);

    let before_search = s.search(&Query::new("alpha")).unwrap();
    let before_long = s.search(&Query::new("d".repeat(100_000))).unwrap();
    let before_symbols = s.search_symbols(&SymbolQuery::new("*")).unwrap();
    let before_describe = s.describe(None, None).unwrap();
    let before_tokens = s.file_tokens("o", "r1", "a.rs").unwrap();
    s.check_consistency(false);

    let (s, stats) = s.compact().unwrap();
    assert!(stats.before_bytes > 0);
    assert!(stats.after_bytes > 0);

    assert_eq!(s.search(&Query::new("alpha")).unwrap(), before_search);
    assert_eq!(
        s.search(&Query::new("d".repeat(100_000))).unwrap(),
        before_long
    );
    assert_eq!(
        s.search_symbols(&SymbolQuery::new("*")).unwrap(),
        before_symbols
    );
    assert_eq!(s.describe(None, None).unwrap(), before_describe);
    assert_eq!(s.file_tokens("o", "r1", "a.rs").unwrap(), before_tokens);
    s.check_consistency(false);
}

/// The core story-3 gate: after pruning most of a store down to one file and
/// vacuuming (which drops the dead dictionary terms but, per the `churn` and
/// `prune_churn` measurements, never shrinks the file on its own), `compact`
/// actually shrinks it.
#[test]
fn compact_after_vacuum_on_a_pruned_store_shrinks_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    for i in 0..200 {
        // Enough distinct, sizeable tokens per file that pruning almost all
        // of them frees whole pages, not just a handful of table rows (a
        // handful of dead rows can still fit in already-allocated pages, so
        // the file would not visibly shrink even though `compact` worked).
        let toks: Vec<(String, u32, u32)> = (0..40)
            .map(|j| {
                let text = format!("token_{i}_{j}_{}", "x".repeat(20));
                let start = j * 30;
                (text, start, start + 25)
            })
            .collect();
        let tok_refs: Vec<(&str, u32, u32)> =
            toks.iter().map(|(t, s, e)| (t.as_str(), *s, *e)).collect();
        s.ingest_file_with_origin(
            "o",
            "r",
            &format!("f{i}.rs"),
            "rust",
            &span_ext(
                &[(&format!("Sym{i}"), SymbolKind::Function, 0, 1200)],
                &tok_refs,
            ),
            Some(ORIGIN_DIRECTORY),
        )
        .unwrap();
    }
    let keep: std::collections::HashSet<String> = ["f0.rs".to_string()].into_iter().collect();
    s.prune_files("o", "r", &keep, false).unwrap();
    s.vacuum().unwrap();
    drop(s);
    let before_bytes = std::fs::metadata(&p).unwrap().len();

    let s = V2Store::open(&p).unwrap();
    let (s, stats) = s.compact().unwrap();
    assert_eq!(stats.before_bytes, before_bytes);
    assert!(
        stats.after_bytes < stats.before_bytes,
        "compact must shrink a heavily pruned, vacuumed file: {} -> {}",
        stats.before_bytes,
        stats.after_bytes
    );
    let after_bytes = sha_len(&p);
    assert_eq!(after_bytes, stats.after_bytes);

    // Compaction did not lose the surviving file's data.
    assert_eq!(
        s.search(&Query::new(format!("token_0_0_{}", "x".repeat(20))))
            .unwrap()
            .len(),
        1
    );
    s.check_consistency(true);
}

fn sha_len(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).unwrap().len()
}

/// `compact` reopens the store internally; `chunk_bytes` and `cache_bytes`
/// must survive that reopen unchanged, not silently reset to defaults.
#[test]
fn compact_preserves_chunk_and_cache_bytes() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let mut s = V2Store::open_with_cache_bytes(&p, Some(123_456)).unwrap();
    s.set_chunk_bytes(789);
    compact_fixture(&s);

    let (s, _) = s.compact().unwrap();
    assert_eq!(s.chunk_bytes, 789);
    assert_eq!(s.cache_bytes, Some(123_456));
}

// --- ADR 0003 story 3, slice 3l: refs/content_files refcount bookkeeping --

fn refs_snapshot(
    s: &V2Store,
) -> (
    std::collections::BTreeMap<u64, u64>,
    std::collections::BTreeSet<(u64, u64)>,
) {
    let rt = s.db.begin_read().unwrap();
    let mut refs = std::collections::BTreeMap::new();
    for row in rt.open_table(crate::v2::REFS).unwrap().iter().unwrap() {
        let (k, v) = row.unwrap();
        refs.insert(k.value(), v.value());
    }
    let mut content_files = std::collections::BTreeSet::new();
    for row in rt
        .open_multimap_table(crate::v2::CONTENT_FILES)
        .unwrap()
        .iter()
        .unwrap()
    {
        let (k, vals) = row.unwrap();
        for v in vals {
            content_files.insert((k.value(), v.unwrap().value()));
        }
    }
    (refs, content_files)
}

/// The core refcount invariant (ADR 0003 story 3, Q2 / slice 3l): after
/// ingest, replace, prune and re-ingest of a small corpus, `refs` and
/// `content_files` exactly match the live file set -- every live file has
/// `refs[content_id(file)] == 1` and a matching `content_files` entry, and
/// there is no entry left over for a deleted file. Content sharing is off
/// today (`content_id` is the identity), so this is a 1:1:1 correspondence,
/// not a real fan-out test; that is exactly the seam story 18 will exercise.
#[test]
fn refs_and_content_files_match_the_live_file_set_after_ingest_replace_prune_and_reingest() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();

    // `prune_files` only prunes files ingested with `ORIGIN_DIRECTORY`
    // (see its origin check), so this fixture uses that origin throughout.
    s.ingest_file_with_origin(
        "o",
        "r",
        "a.rs",
        "rust",
        &span_ext(&[], &[("alpha", 0, 5)]),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    s.ingest_file_with_origin(
        "o",
        "r",
        "b.rs",
        "rust",
        &span_ext(&[], &[("beta", 0, 4)]),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    s.ingest_file_with_origin(
        "o",
        "r",
        "c.rs",
        "rust",
        &span_ext(&[], &[("gamma", 0, 5)]),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    // Replace: same path, different content -- old content's rows must go.
    s.ingest_file_with_origin(
        "o",
        "r",
        "b.rs",
        "rust",
        &span_ext(&[], &[("delta", 0, 5)]),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    // Prune: drop c.rs entirely.
    let keep: std::collections::HashSet<String> = ["a.rs".to_string(), "b.rs".to_string()]
        .into_iter()
        .collect();
    s.prune_files("o", "r", &keep, false).unwrap();
    // Re-ingest a fresh file.
    s.ingest_file(
        "o",
        "r",
        "d.rs",
        "rust",
        &span_ext(&[], &[("epsilon", 0, 7)]),
    )
    .unwrap();

    // No orphaned entries and no missing ones: refs/content_files as seen
    // through the store equal exactly what `check_consistency` (extended for
    // this slice) already recomputes from the streams -- assert both here,
    // directly, so this test carries its own signal independent of that helper.
    let (refs, content_files) = refs_snapshot(&s);
    assert_eq!(refs.len(), 3, "one live content id per live file: {refs:?}");
    assert_eq!(
        content_files.len(),
        3,
        "one content_files entry per live file: {content_files:?}"
    );
    for &v in refs.values() {
        assert_eq!(v, 1, "refcount is always 1 while content sharing is off");
    }
    // Every content_files value is a file that is still searchable.
    for &(cid, file) in &content_files {
        assert_eq!(cid, file, "content_id is the identity while sharing is off");
    }
    assert_eq!(
        s.search(&Query::new("delta")).unwrap().len(),
        1,
        "b.rs's replacement content must be live"
    );
    assert!(
        s.search(&Query::new("beta")).unwrap().is_empty(),
        "b.rs's replaced content must be gone"
    );
    assert!(
        s.search(&Query::new("gamma")).unwrap().is_empty(),
        "c.rs's content must be gone after prune"
    );
    assert_eq!(s.search(&Query::new("epsilon")).unwrap().len(), 1);

    s.check_consistency(false);
}

/// Exercises `remove_content`'s refcount-gated delete branch (decrement,
/// don't delete, unless the count reaches zero) -- otherwise unreachable
/// before story 18's real content-sharing fan-out exists, since
/// `content_id(file) == file` always makes every real refcount exactly 1,
/// making a gated delete indistinguishable from an unconditional one (found
/// by QA review, PR #42: an "always delete" mutation was not caught by any
/// other test). Simulates two files sharing one content id via the
/// `inject_extra_content_ref` test hook, without a real story-18 dedup path.
#[test]
fn remove_content_only_deletes_at_a_zero_refcount() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();
    s.ingest_file_with_origin(
        "o",
        "r",
        "shared.rs",
        "rust",
        &span_ext(&[], &[("onlyhere", 0, 8)]),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    let toks = s.file_tokens("o", "r", "shared.rs").unwrap().unwrap();
    let shared_file = (toks[0].id >> 32) & 0x3fff_ffff;
    // No file "999" is ever really ingested under this content id; the hook
    // only adds the bookkeeping a real second reference would leave, so
    // `refs[content_id(shared.rs)]` goes from 1 to 2.
    s.inject_extra_content_ref(999, shared_file);

    // Pruning "shared.rs" alone must decrement, not delete: the content is
    // still referenced (by the injected extra reference). An "always
    // delete" bug (QA's mutation, PR #42) would delete the stream row here
    // regardless. (Not checked via `search`: prune legitimately removes
    // "shared.rs"'s own file entity row regardless of content refcount --
    // resolving a query hit through a *different*, still-live referencing
    // file's entity is real content-sharing fan-out, story 18's job, not
    // this bookkeeping-only slice's. The stream row itself is the thing
    // `remove_content` decides whether to delete, so check that directly.)
    let keep = std::collections::HashSet::new();
    s.prune_files("o", "r", &keep, false).unwrap();
    let rt = s.db.begin_read().unwrap();
    assert!(
        rt.open_table(crate::v2::STREAMS)
            .unwrap()
            .get(shared_file)
            .unwrap()
            .is_some(),
        "stream row must survive while a second reference exists"
    );
    let refs = rt.open_table(crate::v2::REFS).unwrap();
    assert_eq!(
        refs.get(shared_file).unwrap().map(|v| v.value()),
        Some(1),
        "refcount must have decremented from 2 to 1, not stayed at 2 or hit 0"
    );
    // Reaching zero and actually deleting is already covered extensively by
    // every other test in this file (every real refcount is 1, so an
    // ordinary prune/replace already exercises "decrement to zero, delete"
    // end to end); this test's job is only the previously-uncovered
    // "decrement, don't delete" branch above.
}

/// A skip-unchanged (fingerprint-matched) file must not touch `refs` or
/// `content_files` at all -- the skip check runs before any write, so the
/// whole file is byte-identical, not just those two tables (same style as
/// `a_no_op_vacuum_leaves_the_file_byte_identical`: hashed only while no
/// handle is open, since redb holds a Windows file lock for the handle's life).
#[test]
fn skip_unchanged_file_leaves_refs_and_content_files_untouched() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    s.index_bytes("o", "r", "a.rs", b"fn f() { f(); }\n", None)
        .unwrap();
    drop(s);
    let before = std::fs::read(&p).unwrap();

    let s = V2Store::open(&p).unwrap();
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn f() { f(); }\n", None)
        .unwrap();
    assert!(
        st.unchanged,
        "second ingest of identical bytes must be a skip"
    );
    drop(s);
    let after = std::fs::read(&p).unwrap();
    assert_eq!(before, after, "a skipped file must write nothing at all");
}

/// `compact` round-trips `refs` and `content_files` (ADR 0003 story 3, slice
/// 3l): indexing a corpus, pruning some files so both tables have real
/// content, compacting, and comparing table contents before/after --
/// mirrors `compact_does_not_change_query_results`' method but is a
/// dedicated check because `compact`'s table copy list is hand-maintained
/// (the single highest-risk detail in this slice: forgetting to add these
/// two tables there would silently drop refcounts on every compaction).
#[test]
fn compact_round_trips_refs_and_content_files() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    compact_fixture(&s);
    // Prune one file so refs/content_files reflect real churn, not just a
    // freshly-ingested 1:1:1 set.
    let keep: std::collections::HashSet<String> = ["a.rs".to_string()].into_iter().collect();
    s.prune_files("o", "r1", &keep, false).unwrap();

    let before = refs_snapshot(&s);
    assert!(
        !before.0.is_empty(),
        "fixture must leave live refs to round-trip"
    );

    let (s, _) = s.compact().unwrap();
    let after = refs_snapshot(&s);
    assert_eq!(
        before, after,
        "compact must copy refs/content_files verbatim"
    );
    s.check_consistency(false);
}

/// Pins the chunked-batch case: a chunked `index_batch` (several commits, not
/// one) must leave the same refcount invariant as an unchunked run -- every
/// live file has refcount exactly 1 with a matching `content_files` entry.
#[test]
fn chunked_batch_ingest_holds_the_refcount_invariant() {
    let d = tempfile::tempdir().unwrap();
    let mut s = V2Store::open(d.path().join("v.redb")).unwrap();
    s.set_chunk_bytes(1); // every file is its own commit chunk

    let files: Vec<BatchFile<'_>> = (0..12)
        .map(|i| BatchFile {
            path: Box::leak(format!("f{i}.txt").into_boxed_str()),
            bytes: Box::leak(format!("word{i} other{}", i % 3).into_boxed_str().into()),
            language: Some("text"),
            origin: None,
        })
        .collect();
    V2Store::index_batch(&s, "o", "r", &files, IndexOptions { reindex: false }).unwrap();

    let (refs, content_files) = refs_snapshot(&s);
    assert_eq!(refs.len(), 12);
    assert_eq!(content_files.len(), 12);
    for &v in refs.values() {
        assert_eq!(v, 1);
    }
    s.check_consistency(false);
}

/// <0.5% storage-growth gate for this slice's two new tables (following the
/// ADR 0003 story 3 board's slice-3h precedent of a real, measured CI gate
/// rather than a claimed number). Two identically-indexed stores over this
/// repo's own `crates/` tree, compacted so both sizes reflect only live
/// data: one keeps its real `refs`/`content_files` rows, the other has them
/// cleared (but the tables still exist, so table-creation overhead is
/// common to both and only the row data differs) before compacting --
/// isolating exactly the bytes this slice's bookkeeping adds.
#[test]
fn refs_and_content_files_grow_store_size_by_under_half_a_percent_on_this_repos_corpus() {
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
    let mut files = Vec::new();
    walk(std::path::Path::new("../../crates"), &mut files);
    assert!(
        files.len() > 10,
        "expected this repo's own .rs corpus, found {}",
        files.len()
    );
    files.sort();

    use graph_core::Extractor;
    let extractor = graph_lang_rust::RustExtractor;

    fn build_store(path: &std::path::Path, files: &[std::path::PathBuf]) -> V2Store {
        let s = V2Store::open(path).unwrap();
        let extractor = graph_lang_rust::RustExtractor;
        for path in files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = path.to_string_lossy().replace('\\', "/");
            let ex = extractor.extract(&src);
            let _ = s.ingest_file("o", "r", &rel, "rust", &ex);
        }
        s
    }
    let _ = &extractor; // silence unused-in-outer-scope lint; used inside build_store

    let d = tempfile::tempdir().unwrap();
    let new_path = d.path().join("new.redb");
    let s = build_store(&new_path, &files);
    let (s, _) = s.compact().unwrap();
    drop(s);
    let new_bytes = std::fs::metadata(&new_path).unwrap().len();

    let old_path = d.path().join("old.redb");
    let s = build_store(&old_path, &files);
    {
        let wt = s.db.begin_write().unwrap();
        {
            let mut refs = wt.open_table(crate::v2::REFS).unwrap();
            let keys: Vec<u64> = refs.iter().unwrap().map(|r| r.unwrap().0.value()).collect();
            for k in keys {
                refs.remove(k).unwrap();
            }
        }
        {
            let mut cf = wt.open_multimap_table(crate::v2::CONTENT_FILES).unwrap();
            let pairs: Vec<(u64, u64)> = cf
                .iter()
                .unwrap()
                .flat_map(|r| {
                    let (k, vals) = r.unwrap();
                    let k = k.value();
                    vals.map(move |v| (k, v.unwrap().value()))
                        .collect::<Vec<_>>()
                })
                .collect();
            for (k, v) in pairs {
                cf.remove(k, v).unwrap();
            }
        }
        wt.commit().unwrap();
    }
    let (s, _) = s.compact().unwrap();
    drop(s);
    let old_bytes = std::fs::metadata(&old_path).unwrap().len();

    let delta_pct = (new_bytes as f64 - old_bytes as f64) / old_bytes as f64 * 100.0;
    println!(
        "refs/content_files growth over {} files: without {old_bytes} B, with {new_bytes} B, \
         delta {delta_pct:.4}%",
        files.len()
    );
    assert!(
        delta_pct < 0.5,
        "refs/content_files grew store size {delta_pct:.4}% (without {old_bytes} B -> with \
         {new_bytes} B over {} files); must stay under 0.5% (slice 3l gate)",
        files.len()
    );
}
