//! ADR 0003 story 3 leftovers on v2: no-op vacuum, term-length policy,
//! chunked commits and the consistency proptest.
use super::*;
use crate::v2::{hashed_key, MAX_INLINE_TERM, R};
use crate::v2_tests::{both_backends, span_ext};

fn sha(p: &std::path::Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(std::fs::read(p).unwrap()).to_vec()
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
