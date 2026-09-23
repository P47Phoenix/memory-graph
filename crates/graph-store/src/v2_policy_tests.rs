//! ADR 0003 story 3 leftovers on v2: no-op vacuum, term-length policy,
//! chunked commits and the consistency proptest.
use super::*;
use crate::v2::{content_id, hashed_key, MAX_INLINE_TERM, OPEN_BATCH, R};
use crate::v2_tests::{both_backends, span_ext};
use redb::{ReadableTableMetadata, TableDefinition};

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

/// Reads the open-batch marker directly out of the raw `meta`/`open_batch`
/// tables, bypassing any store API, so the test observes exactly what a
/// slice-3o reader would see on disk (`None` when fully cleared).
fn open_batch_marker(s: &V2Store) -> Option<(u64, String, String)> {
    let rt = s.db.begin_read().unwrap();
    let meta = rt.open_table(META).unwrap();
    let id = meta.get("open_batch_id").unwrap().map(|v| v.value());
    let ob = rt.open_table(OPEN_BATCH).unwrap();
    let org = ob.get("org").unwrap().map(|v| v.value().to_string());
    let repo = ob.get("repo").unwrap().map(|v| v.value().to_string());
    match (id, org, repo) {
        (Some(id), Some(org), Some(repo)) => Some((id, org, repo)),
        (None, None, None) => None,
        other => panic!("open-batch marker partially set: {other:?}"),
    }
}

/// After a completed `index_batch` -- chunked (a small chunk cap forces
/// several chunk commits) or unchunked (the default cap, one transaction) --
/// the open-batch marker (ADR 0003 story 3, decision D3) must be absent: the
/// final chunk's transaction cleared it.
#[test]
fn open_batch_marker_is_cleared_after_a_completed_batch() {
    let d = tempfile::tempdir().unwrap();
    let srcs: Vec<String> = (0..6).map(|i| format!("aaa{i}")).collect();
    let ps = paths(6, "p");

    let mut chunked = V2Store::open(d.path().join("chunked.redb")).unwrap();
    chunked.set_chunk_bytes(5); // several chunk commits for 6 tiny files
    V2Store::index_batch(
        &chunked,
        "o",
        "r",
        &batch(&srcs, &ps),
        IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(
        open_batch_marker(&chunked),
        None,
        "chunked batch clears the marker"
    );

    let unchunked = V2Store::open(d.path().join("unchunked.redb")).unwrap();
    V2Store::index_batch(
        &unchunked,
        "o",
        "r",
        &batch(&srcs, &ps),
        IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(
        open_batch_marker(&unchunked),
        None,
        "unchunked batch clears the marker"
    );

    // Single-file ingest paths never touch chunking, so the marker never
    // appears on them at all.
    let single = V2Store::open(d.path().join("single.redb")).unwrap();
    single
        .index_bytes("o", "r", "x.p", b"aaa", Some("poison"))
        .unwrap();
    assert_eq!(
        open_batch_marker(&single),
        None,
        "single-file ingest never sets the marker"
    );
}

/// Ingest-cost sanity check for the open-batch marker (ADR 0003 story 3,
/// slice 3n): the marker adds one small `meta`/`open_batch` write per chunk
/// commit, so unlike slice 3h/3l's byte-growth gates there is no separate
/// "before" binary to diff against in this same test process -- instead this
/// asserts a generous wall-clock ceiling on indexing this repo's own
/// `crates/` tree (27 files) chunked finely enough that nearly every file is
/// its own chunk (worst case for marker-write overhead), so a real
/// regression in the marker bookkeeping would blow well past it.
#[test]
fn chunked_ingest_with_the_open_batch_marker_completes_promptly_on_this_repos_own_corpus() {
    let files = this_repos_rust_corpus();
    assert!(
        files.len() > 10,
        "expected this repo's own .rs corpus, found {}",
        files.len()
    );
    let srcs: Vec<String> = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    let ps: Vec<String> = files
        .iter()
        .take(srcs.len())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let bf: Vec<BatchFile<'_>> = srcs
        .iter()
        .zip(&ps)
        .map(|(s, p)| BatchFile {
            path: p,
            bytes: s.as_bytes(),
            language: Some("rust"),
            origin: None,
        })
        .collect();

    let d = tempfile::tempdir().unwrap();
    let mut s = V2Store::open(d.path().join("v.redb")).unwrap();
    s.register(Box::new(graph_lang_rust::RustExtractor));
    s.set_chunk_bytes(1); // every file its own chunk: worst case for marker overhead
    let t = std::time::Instant::now();
    let results = V2Store::index_batch(&s, "o", "r", &bf, IndexOptions::default()).unwrap();
    let elapsed = t.elapsed();
    assert!(results.iter().all(|r| r.is_ok()));
    eprintln!(
        "chunked ingest with open-batch marker, {} files, one chunk each: {:.1} ms total",
        bf.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "indexing this repo's own corpus, one file per chunk, took {:.1} ms; expected well under \
         5 s even with the added marker write per chunk",
        elapsed.as_secs_f64() * 1000.0
    );
}

/// A batch that dies before its final chunk commits (mirroring
/// `chunked_batches_are_atomic_per_chunk`'s technique: an extractor that
/// poisons one file's chunk) leaves the marker present after reopening, and
/// it correctly names the batch id, org and repo of the batch that never
/// finished.
#[test]
fn open_batch_marker_survives_a_mid_batch_abort() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("s.redb");
    // f2 is poisoned; cap of 20 bytes commits {f0, f1} as chunk 0, then f2's
    // own chunk fails and is never committed, so the batch never reaches its
    // final, marker-clearing transaction.
    let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
        .map(String::from)
        .to_vec();
    let ps = paths(4, "p");
    let mut s = V2Store::open(&p).unwrap();
    s.register(Box::new(Poison));
    s.set_chunk_bytes(20);
    assert!(
        V2Store::index_batch(&s, "o", "r", &batch(&srcs, &ps), IndexOptions::default()).is_err()
    );
    drop(s);

    let reopened = V2Store::open(&p).unwrap();
    let marker = open_batch_marker(&reopened);
    assert_eq!(
        marker,
        Some((0, "o".to_string(), "r".to_string())),
        "the marker names the batch that never finalized"
    );
}

/// Slice 3o: `describe`'s `RepoInfo::open_batch` reader surface for the D3
/// marker. A batch killed mid-way (same poisoned-extractor technique as
/// `open_batch_marker_survives_a_mid_batch_abort`) makes `describe` report
/// `open_batch: true` for that org/repo, and self-heals -- a normal
/// completing batch for the same org/repo afterward clears it again, with no
/// repair command needed, matching the ADR's "self-healing" description.
#[test]
fn describe_reports_open_batch_after_a_crash_and_clears_it_after_a_completed_batch() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("s.redb");
    let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
        .map(String::from)
        .to_vec();
    let ps = paths(4, "p");
    let mut s = V2Store::open(&p).unwrap();
    s.register(Box::new(Poison));
    s.set_chunk_bytes(20);
    assert!(
        V2Store::index_batch(&s, "o", "r", &batch(&srcs, &ps), IndexOptions::default()).is_err()
    );
    drop(s);

    let reopened = V2Store::open(&p).unwrap();
    let infos = reopened.describe(None, None).unwrap();
    let r = infos.iter().find(|i| i.org == "o" && i.repo == "r");
    assert!(
        r.is_some_and(|r| r.open_batch),
        "describe should report the crashed batch's repo as open_batch: true"
    );

    // Self-healing: a normal batch for the SAME org/repo (not a repair
    // command) clears the marker on its own completion.
    drop(reopened);
    let mut healer = V2Store::open(&p).unwrap();
    healer.register(Box::new(Poison));
    V2Store::index_batch(
        &healer,
        "o",
        "r",
        &batch(&[srcs[0].clone()], &[ps[0].clone()]),
        IndexOptions::default(),
    )
    .unwrap()
    .into_iter()
    .for_each(|r| {
        r.unwrap();
    });
    let infos = healer.describe(None, None).unwrap();
    let r = infos.iter().find(|i| i.org == "o" && i.repo == "r");
    assert!(
        r.is_some_and(|r| !r.open_batch),
        "a completed batch for the same org/repo self-heals the marker"
    );
}

/// v1 has no chunked-batch marker at all: `describe` must unconditionally
/// report `open_batch: false`, never error looking for a table that does not
/// exist in the v1 layout.
#[test]
fn v1_describe_always_reports_open_batch_false() {
    let d = tempfile::tempdir().unwrap();
    let s = crate::RedbStore::open(d.path().join("v1.redb")).unwrap();
    s.index_bytes("o", "r", "x.p", b"aaa", None).unwrap();
    let infos = s.describe(None, None).unwrap();
    assert_eq!(infos.len(), 1);
    assert!(!infos[0].open_batch, "v1 never reports an open batch");
}

/// The marker names exactly one org/repo (redb allows only one writer
/// transaction at a time, so only one batch can ever be open). A crashed
/// batch for `o/r` must not leak `open_batch: true` onto an unrelated,
/// cleanly-indexed `o2/r2` in the same store, in either direction: neither an
/// unfiltered `describe` nor a `describe` filtered to the other repo.
#[test]
fn open_batch_does_not_leak_across_repos() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("s.redb");
    let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
        .map(String::from)
        .to_vec();
    let ps = paths(4, "p");
    let mut s = V2Store::open(&p).unwrap();
    s.register(Box::new(Poison));
    s.set_chunk_bytes(20);
    assert!(
        V2Store::index_batch(&s, "o", "r", &batch(&srcs, &ps), IndexOptions::default()).is_err()
    );
    s.index_bytes("o2", "r2", "clean.p", b"aaa", Some("poison"))
        .unwrap();

    let by_org_repo = |infos: &[crate::RepoInfo], org: &str, repo: &str| -> bool {
        infos
            .iter()
            .find(|i| i.org == org && i.repo == repo)
            .is_some_and(|i| i.open_batch)
    };

    let all = s.describe(None, None).unwrap();
    assert!(by_org_repo(&all, "o", "r"), "crashed repo reports open");
    assert!(
        !by_org_repo(&all, "o2", "r2"),
        "unrelated repo must not leak open_batch: true"
    );

    let scoped_clean = s.describe(Some("o2"), Some("r2")).unwrap();
    assert!(
        scoped_clean.iter().all(|i| !i.open_batch),
        "filtering to the clean repo must not surface the marker"
    );
    let scoped_crashed = s.describe(Some("o"), Some("r")).unwrap();
    assert!(
        scoped_crashed.iter().all(|i| i.open_batch),
        "filtering to the crashed repo must still surface the marker"
    );

    // The reference scan (`R::describe_by_scan`, used by `check_consistency`
    // and the conformance suite) must agree with `describe` while the batch
    // is genuinely open, not just after it clears.
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap(),
        "describe and describe_by_scan must agree while a batch is open"
    );
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

    /// The refcount invariant (ADR 0003 story 3, slice 3m), checked
    /// independently of `check_consistency`'s own refs/content_files
    /// recomputation so this test carries its own signal: for every content
    /// id, `refs[cid]` equals the number of `content_files[cid]` entries,
    /// which equals the number of live file rows whose content is `cid`; and
    /// no stream or posting exists for a `cid` with refcount zero (i.e.
    /// every `cid` that owns a stream or posting appears in `refs` with a
    /// nonzero count).
    fn assert_refcount_invariant(s: &V2Store) {
        let rt = s.db.begin_read().unwrap();
        let r = R::new(&rt).unwrap();

        let mut refs: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        for row in r.refs.iter().unwrap() {
            let (k, v) = row.unwrap();
            refs.insert(k.value(), v.value());
        }
        let mut content_files: std::collections::BTreeMap<u64, usize> =
            std::collections::BTreeMap::new();
        let mut live_files_by_cid: std::collections::BTreeMap<u64, usize> =
            std::collections::BTreeMap::new();
        for row in r.content_files.iter().unwrap() {
            let (k, vals) = row.unwrap();
            let k = k.value();
            let mut n = 0;
            for v in vals {
                let file = v.unwrap().value();
                n += 1;
                // A `content_files` entry for a file that no longer has a
                // stream would be an orphan (dangling pointer to dead
                // content); every entry's file must be live.
                assert!(
                    r.streams.get(file).unwrap().is_some(),
                    "content_files[{k}] points at file {file}, which has no live stream"
                );
            }
            content_files.insert(k, n);
        }
        let mut cids_with_data: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for row in r.streams.iter().unwrap() {
            let (file, _) = row.unwrap();
            *live_files_by_cid
                .entry(content_id(file.value()))
                .or_default() += 1;
            cids_with_data.insert(content_id(file.value()));
        }
        for row in r.post.iter().unwrap() {
            let (k, _) = row.unwrap();
            let (_, file) = k.value();
            cids_with_data.insert(content_id(file));
        }

        for (&cid, &want) in &live_files_by_cid {
            assert_eq!(
                refs.get(&cid).copied().unwrap_or(0),
                want as u64,
                "refs[{cid}] must equal the live file count for that content id"
            );
            assert_eq!(
                content_files.get(&cid).copied().unwrap_or(0),
                want,
                "content_files[{cid}] must have one entry per live file of that content id"
            );
        }
        for &cid in refs.keys() {
            assert!(
                live_files_by_cid.contains_key(&cid),
                "refs has an entry for content id {cid} with no live file"
            );
        }
        for cid in cids_with_data {
            assert!(
                refs.get(&cid).copied().unwrap_or(0) > 0,
                "content id {cid} has a stream or posting but refcount zero (or missing)"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
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
                assert_refcount_invariant(&s);
            }
            s.vacuum().unwrap();
            s.check_consistency(true);
            assert_refcount_invariant(&s);
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

// --- ADR 0003 story 3, slice 3m: rebuild_refs ------------------------------

fn walk_rs_files(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(p) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if !path.ends_with("target") && !path.ends_with(".git") {
                walk_rs_files(&path, out);
            }
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// This repo's own `crates/` tree (27 Rust files at the time this slice was
/// written), the same corpus the story-3 size and decode-cost gates already
/// measure against (slices 3h, 3j, 3k, 3l).
fn this_repos_rust_corpus() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    walk_rs_files(std::path::Path::new("../../crates"), &mut files);
    files.sort();
    files
}

/// Corrupts `refs`/`content_files` directly (bypassing ingest/replace/prune
/// entirely, the way `inject_extra_content_ref` bypasses it for slice 3l's
/// test) and confirms `rebuild_refs` restores them to exactly what a fresh
/// ingest of the same corpus would produce.
#[test]
fn rebuild_refs_restores_refs_and_content_files_after_direct_corruption() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();
    compact_fixture(&s);
    // A second store over the identical corpus is the "what a fresh ingest
    // would produce" oracle this test checks `rebuild_refs`'s output against.
    let fresh = V2Store::open(d.path().join("fresh.redb")).unwrap();
    compact_fixture(&fresh);
    let want = refs_snapshot(&fresh);

    {
        let wt = s.db.begin_write().unwrap();
        {
            let mut refs = wt.open_table(crate::v2::REFS).unwrap();
            let keys: Vec<u64> = refs.iter().unwrap().map(|r| r.unwrap().0.value()).collect();
            // Drop one live entry and plant a bogus one -- both a missing row
            // and an orphaned row are corruption `rebuild_refs` must fix.
            if let Some(&k) = keys.first() {
                refs.remove(k).unwrap();
            }
            refs.insert(999_999_999u64, 7u64).unwrap();
        }
        {
            let mut cf = wt.open_multimap_table(crate::v2::CONTENT_FILES).unwrap();
            cf.insert(999_999_999u64, 424_242u64).unwrap();
        }
        wt.commit().unwrap();
    }
    let corrupted = refs_snapshot(&s);
    assert_ne!(
        corrupted, want,
        "the direct corruption above must actually change the stored tables"
    );

    s.rebuild_refs().unwrap();
    let rebuilt = refs_snapshot(&s);
    assert_eq!(
        rebuilt, want,
        "rebuild_refs must restore refs/content_files to exactly what a fresh ingest of the \
         same corpus produces"
    );
    s.check_consistency(false);
}

/// Wall-clock cost of `rebuild_refs` (ADR 0003 story 3, slice 3m gate):
/// under 50 ms on this repo's own `crates/` tree, and the cost ratio between
/// a 2x-corpus run (the same files ingested twice, under a second org) and
/// the 1x run stays under 2.5x -- not e.g. 4x+, which would indicate
/// quadratic behavior.
#[test]
fn rebuild_refs_cost_is_bounded_and_near_linear_on_this_repos_own_corpus() {
    use graph_core::Extractor;
    let files = this_repos_rust_corpus();
    assert!(
        files.len() > 10,
        "expected this repo's own .rs corpus, found {}",
        files.len()
    );
    let extractor = graph_lang_rust::RustExtractor;

    let d = tempfile::tempdir().unwrap();
    let s1 = V2Store::open(d.path().join("1x.redb")).unwrap();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.to_string_lossy().replace('\\', "/");
        let ex = extractor.extract(&src);
        let _ = s1.ingest_file("o", "r", &rel, "rust", &ex);
    }

    let s2 = V2Store::open(d.path().join("2x.redb")).unwrap();
    for org in ["o1", "o2"] {
        for path in &files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = path.to_string_lossy().replace('\\', "/");
            let ex = extractor.extract(&src);
            let _ = s2.ingest_file(org, "r", &rel, "rust", &ex);
        }
    }

    // Averages several reps (with one untimed warm-up call first, so the
    // first-call allocator/page-cache cost does not skew a single sample) to
    // damp scheduler noise on a shared CI box.
    fn avg_ms(s: &V2Store, reps: usize) -> f64 {
        s.rebuild_refs().unwrap();
        let t = std::time::Instant::now();
        for _ in 0..reps {
            s.rebuild_refs().unwrap();
        }
        t.elapsed().as_secs_f64() * 1000.0 / reps as f64
    }

    let reps = 10;
    let ms1 = avg_ms(&s1, reps);
    let ms2 = avg_ms(&s2, reps);
    let ratio = ms2 / ms1.max(0.001);
    eprintln!(
        "rebuild_refs cost: 1x corpus ({} files) {ms1:.3} ms/call, 2x corpus {ms2:.3} ms/call, \
         ratio {ratio:.2}x",
        files.len()
    );
    assert!(
        ms1 < 50.0,
        "rebuild_refs on the 1x corpus took {ms1:.3} ms, expected < 50 ms"
    );
    assert!(
        ratio < 2.5,
        "rebuild_refs cost ratio (2x/1x) was {ratio:.2}x, expected < 2.5x (a quadratic-behavior \
         gate, not a tight bound)"
    );

    s1.check_consistency(false);
    s2.check_consistency(false);
}

/// A single term occurring far more than `codec::POSTING_BLOCK` times in one
/// file, so its `POST` value spans several block-encoded blocks (story 6,
/// ADR 0003, D1). Search results must be byte-for-byte identical to v1,
/// which has no block concept at all.
#[test]
fn a_term_repeated_across_many_posting_blocks_matches_v1() {
    let (_d, a, b) = both_backends();
    let n = crate::codec::POSTING_BLOCK * 3 + 7;
    let mut toks: Vec<(String, u32, u32)> = Vec::with_capacity(n);
    let mut at = 0u32;
    for _ in 0..n {
        toks.push(("hot".to_string(), at, at + 3));
        at += 4;
    }
    let tok_refs: Vec<(&str, u32, u32)> =
        toks.iter().map(|(s, a, b)| (s.as_str(), *a, *b)).collect();
    let ex = span_ext(&[("S", SymbolKind::Function, 0, at)], &tok_refs);
    for s in [&a, &b] {
        s.ingest_file("o", "r", "hot.rs", "rust", &ex).unwrap();
    }
    let mut q = Query::new("hot");
    q.grain = Grain::Token;
    let (ha, hb) = (a.search(&q).unwrap(), b.search(&q).unwrap());
    assert_eq!(ha.len(), n, "expected {n} occurrences");
    assert_eq!(ha, hb);
    crate::conformance::run_differential(&*a, &*b);
}

// --- ADR 0003 story 9: generic derived_version + refs/content_files
// rebuild-on-open ---

/// Directly clears the stored `derived_version_refs_content_files` marker
/// (the same direct-table-write technique as slice 3l/3m's
/// `inject_extra_content_ref`/direct-corruption tests) and corrupts
/// `refs`/`content_files` to something a fresh ingest would never produce.
/// A plain `V2Store::open` (no manual `rebuild_refs()` call) must detect the
/// stale/missing marker and self-heal both tables back to what a fresh
/// ingest of the same corpus produces.
#[test]
fn a_missing_derived_version_self_heals_refs_on_plain_open() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    compact_fixture(&s);
    drop(s);

    // Oracle: what a fresh ingest of the identical corpus produces.
    let fresh = V2Store::open(d.path().join("fresh.redb")).unwrap();
    compact_fixture(&fresh);
    let want = refs_snapshot(&fresh);

    // Reopen, corrupt refs/content_files and clear the derived_version
    // marker directly, then close the handle so the next `open` starts
    // fresh (as a real "written by an older build" file would).
    {
        let s = V2Store::open(&p).unwrap();
        let wt = s.db.begin_write().unwrap();
        {
            let mut meta = wt.open_table(crate::META).unwrap();
            meta.remove(crate::v2::DERIVED_VERSION_REFS_KEY).unwrap();
        }
        {
            let mut refs = wt.open_table(crate::v2::REFS).unwrap();
            let keys: Vec<u64> = refs.iter().unwrap().map(|r| r.unwrap().0.value()).collect();
            if let Some(&k) = keys.first() {
                refs.remove(k).unwrap();
            }
            refs.insert(999_999_999u64, 7u64).unwrap();
        }
        {
            let mut cf = wt.open_multimap_table(crate::v2::CONTENT_FILES).unwrap();
            cf.insert(999_999_999u64, 424_242u64).unwrap();
        }
        wt.commit().unwrap();
        drop(s);
    }

    // Plain open -- no manual rebuild_refs() call anywhere in this test.
    let s = V2Store::open(&p).unwrap();
    let healed = refs_snapshot(&s);
    assert_eq!(
        healed, want,
        "a plain open() must self-heal refs/content_files when derived_version is missing"
    );
    s.check_consistency(false);

    // The marker itself must now read as current.
    let rt = s.db.begin_read().unwrap();
    let stamped = rt
        .open_table(crate::META)
        .unwrap()
        .get(crate::v2::DERIVED_VERSION_REFS_KEY)
        .unwrap()
        .map(|v| v.value());
    assert_eq!(stamped, Some(crate::v2::REFS_DERIVED_VERSION));
}

/// Same self-heal, but the marker is stamped to an explicit stale value
/// (rather than missing entirely) -- both "absent" and "lower than current"
/// must be treated as stale.
#[test]
fn a_stale_derived_version_self_heals_refs_on_plain_open() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    let s = V2Store::open(&p).unwrap();
    compact_fixture(&s);

    let wt = s.db.begin_write().unwrap();
    {
        let mut meta = wt.open_table(crate::META).unwrap();
        meta.insert(crate::v2::DERIVED_VERSION_REFS_KEY, 0u64)
            .unwrap();
        let mut refs = wt.open_table(crate::v2::REFS).unwrap();
        refs.insert(999_999_999u64, 7u64).unwrap();
    }
    wt.commit().unwrap();
    drop(s);

    let s = V2Store::open(&p).unwrap();
    let rt = s.db.begin_read().unwrap();
    let stamped = rt
        .open_table(crate::META)
        .unwrap()
        .get(crate::v2::DERIVED_VERSION_REFS_KEY)
        .unwrap()
        .map(|v| v.value());
    assert_eq!(stamped, Some(crate::v2::REFS_DERIVED_VERSION));
    let refs = rt.open_table(crate::v2::REFS).unwrap();
    assert!(
        refs.get(999_999_999u64).unwrap().is_none(),
        "the bogus refs row must be gone after self-heal"
    );
    drop(rt);
    s.check_consistency(false);
}

/// A store already stamped at the current `REFS_DERIVED_VERSION` must not
/// write anything on a plain reopen -- the file stays byte-for-byte
/// identical, same convention as `a_no_op_vacuum_leaves_the_file_byte_identical`.
#[test]
fn a_current_derived_version_does_not_rewrite_the_file_on_reopen() {
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

    // Reopening at the already-current derived_version must not write.
    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    drop(s);
    assert_eq!(
        sha(&p),
        before,
        "reopening a store already at the current derived_version must not write"
    );

    // And again, for good measure.
    let s = V2Store::open(&p).unwrap();
    drop(s);
    assert_eq!(sha(&p), before);
}

/// `V2_SCHEMA_VERSION` stays a hard gate: a mismatched layout is still
/// refused (unmodified) rather than silently self-healed. No regression
/// from the soft `derived_version` mechanism added above.
#[test]
fn schema_version_mismatch_still_hard_refuses() {
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
    let wt = s.db.begin_write().unwrap();
    {
        let mut meta = wt.open_table(crate::META).unwrap();
        meta.insert("schema_version", crate::v2::V2_SCHEMA_VERSION - 1)
            .unwrap();
    }
    wt.commit().unwrap();
    drop(s);
    let before = sha(&p);

    match V2Store::open(&p) {
        Err(StoreError::Rejected(_)) => {}
        Err(other) => panic!("expected StoreError::Rejected, got a different error: {other}"),
        Ok(_) => panic!("expected a hard refusal, got Ok"),
    }
    assert_eq!(
        sha(&p),
        before,
        "a schema mismatch must leave the file untouched, not attempt any self-heal"
    );
}

// --- ADR 0003 story 5: packed single sorted dictionary (D1) ---

/// Interning more than a few [`crate::codec::DICT_BLOCK`]-sized worths of
/// distinct terms packs them into far fewer `dict_rev` rows than terms
/// (one row per up to `DICT_BLOCK` terms, not one row per term), and every
/// one of them is still findable by [`crate::v2::R::text`]/`lookup` via
/// `search`.
#[test]
fn dict_rev_packs_many_terms_into_few_blocks() {
    use crate::v2::DICT_REV;
    let n = crate::codec::DICT_BLOCK * 3 + 5;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();
    let owned: Vec<String> = (0..n).map(|i| format!("term{i}")).collect();
    let toks: Vec<(&str, u32, u32)> = owned
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i as u32, i as u32 + 1))
        .collect();
    s.ingest_file("o", "r", "x.txt", "text", &span_ext(&[], &toks))
        .unwrap();
    s.check_consistency(false);

    let rt = s.db.begin_read().unwrap();
    let blocks = rt.open_table(DICT_REV).unwrap().len().unwrap();
    let want_blocks = (n as u64).div_ceil(crate::codec::DICT_BLOCK as u64);
    assert_eq!(
        blocks,
        want_blocks,
        "{n} terms should pack into {want_blocks} rows of up to {} each, not one row per term",
        crate::codec::DICT_BLOCK
    );
    assert!(
        blocks < n as u64,
        "packing must use far fewer rows than terms"
    );
    for t in &owned {
        assert_eq!(s.search(&Query::new(t)).unwrap().len(), 1, "term {t}");
    }
}

/// `vacuum` repacks `dict_rev` densely (ADR 0003 story 5): after removing
/// terms scattered across several blocks, block boundaries no longer line up
/// with `id / DICT_BLOCK` (dead ids leave gaps), and lookups must still find
/// every surviving term by the binary-search-over-blocks path, not the
/// (no longer valid) dense addressing.
#[test]
fn vacuum_repacks_dict_rev_blocks_and_lookups_stay_correct() {
    use crate::v2::DICT_REV;
    let n = crate::codec::DICT_BLOCK * 2 + 10;
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();
    let owned: Vec<String> = (0..n).map(|i| format!("term{i}")).collect();
    let toks: Vec<(&str, u32, u32)> = owned
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i as u32, i as u32 + 1))
        .collect();
    s.ingest_file("o", "r", "x.txt", "text", &span_ext(&[], &toks))
        .unwrap();
    s.check_consistency(false);
    let before_blocks = {
        let rt = s.db.begin_read().unwrap();
        rt.open_table(DICT_REV).unwrap().len().unwrap()
    };

    // Replace the file with only every third term: two-thirds of the
    // original terms die, scattered across every original block.
    let kept: Vec<String> = owned.iter().step_by(3).cloned().collect();
    let kept_toks: Vec<(&str, u32, u32)> = kept
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i as u32, i as u32 + 1))
        .collect();
    s.ingest_file("o", "r", "x.txt", "text", &span_ext(&[], &kept_toks))
        .unwrap();
    s.check_consistency(false);
    let stats = s.vacuum().unwrap();
    assert_eq!(stats.terms_removed, n - kept.len());
    s.check_consistency(true);

    let after_blocks = {
        let rt = s.db.begin_read().unwrap();
        rt.open_table(DICT_REV).unwrap().len().unwrap()
    };
    assert!(
        after_blocks <= before_blocks,
        "repacking after vacuum must not use more blocks"
    );
    for t in &kept {
        assert_eq!(
            s.search(&Query::new(t.as_str())).unwrap().len(),
            1,
            "term {t}"
        );
    }
    for (i, t) in owned.iter().enumerate() {
        if i % 3 != 0 {
            assert_eq!(s.search(&Query::new(t)).unwrap().len(), 0, "dead term {t}");
        }
    }
}

/// Size gate (ADR 0003 story 5, decision D1: "Dictionary <= 15% of pages at
/// 9.9 M; lookups unchanged"): on a term set derived from this repo's own
/// source (`v2.rs`/`codec.rs`, tokenized crudely by splitting on
/// non-identifier bytes, deduplicated), the packed `dict_rev` table's total
/// on-disk bytes must not exceed the pre-story-5 one-row-per-term layout's,
/// measured empirically (two real redb files, `Database::compact`ed so
/// redb's own free-page slack does not swamp the row-format difference) --
/// not estimated. Fast (runs in the default `cargo test`, not `--release`
/// only): the term set here is a few thousand terms, not the ADR spike's
/// synthetic 9.9 M-token corpus (that scale is a `--release` `cargo run
/// --example`, matching story 6's `block_postings_100m` precedent, not a
/// unit test).
#[test]
fn dict_rev_packed_bytes_do_not_exceed_pre_story5_layout_on_this_repos_own_corpus() {
    use crate::v2::DICT_REV;
    let mut terms: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for src in [
        include_str!("v2.rs"),
        include_str!("codec.rs"),
        include_str!("v2_policy_tests.rs"),
    ] {
        for word in src.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.len() >= 2 {
                terms.insert(word.to_string());
            }
        }
    }
    let terms: Vec<String> = terms.into_iter().collect();
    assert!(
        terms.len() > 4 * crate::codec::DICT_BLOCK,
        "sanity: {}",
        terms.len()
    );

    let d = tempfile::tempdir().unwrap();

    // Pre-story-5 layout: one row per term, id -> text.
    let old_path = d.path().join("old.redb");
    {
        const OLD: TableDefinition<u64, &str> = TableDefinition::new("dict_rev");
        let mut db = redb::Database::create(&old_path).unwrap();
        let wt = db.begin_write().unwrap();
        {
            let mut t = wt.open_table(OLD).unwrap();
            for (id, text) in terms.iter().enumerate() {
                t.insert(id as u64, text.as_str()).unwrap();
            }
        }
        wt.commit().unwrap();
        db.compact().unwrap();
    }

    // Story-5 packed layout: block index -> encode_dict_block bytes.
    let new_path = d.path().join("new.redb");
    {
        let mut db = redb::Database::create(&new_path).unwrap();
        let wt = db.begin_write().unwrap();
        {
            let mut t = wt.open_table(DICT_REV).unwrap();
            for (i, chunk) in terms.chunks(crate::codec::DICT_BLOCK).enumerate() {
                let refs: Vec<(u64, &str)> = chunk
                    .iter()
                    .enumerate()
                    .map(|(j, text)| ((i * crate::codec::DICT_BLOCK + j) as u64, text.as_str()))
                    .collect();
                t.insert(i as u64, crate::codec::encode_dict_block(&refs).as_slice())
                    .unwrap();
            }
        }
        wt.commit().unwrap();
        db.compact().unwrap();
    }

    let old_bytes = std::fs::metadata(&old_path).unwrap().len();
    let new_bytes = std::fs::metadata(&new_path).unwrap().len();
    assert!(
        new_bytes <= old_bytes,
        "packed dict_rev ({new_bytes} B) must not exceed the pre-story-5 one-row-per-term \
         layout ({old_bytes} B) on {} terms from this repo's own source",
        terms.len()
    );
}
