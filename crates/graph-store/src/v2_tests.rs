//! v2 test gaps (issue #19 QA comment) and a randomized v1-vs-v2 differential.
//! Each named test kills the mutant it is named for.
use super::*;

/// Build an extraction from `(name, kind, start, end)` symbols and
/// `(text, start, end)` tokens (one line, so columns follow bytes).
fn span_ext(
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

fn both_backends() -> (tempfile::TempDir, Box<dyn Store>, Box<dyn Store>) {
    let d = tempfile::tempdir().unwrap();
    let a = open_store(Backend::Redb, &d.path().join("a.redb"), vec![]).unwrap();
    let b = open_store(Backend::RedbV2, &d.path().join("b.redb"), vec![]).unwrap();
    (d, a, b)
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
    let (_d, a, b) = both_backends();
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
    let (_d, a, b) = both_backends();
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
    let (_d, a, b) = both_backends();
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
    let s = open_store(Backend::RedbV2, &d.path().join("b.redb"), vec![]).unwrap();
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

mod random {
    use super::*;
    use proptest::prelude::*;

    const VOCAB: [&str; 4] = ["new", "a", "b", "("];
    const KINDS: [SymbolKind; 3] = [SymbolKind::Function, SymbolKind::Method, SymbolKind::Type];
    const NAMES: [&str; 4] = ["new", "a", "ab", "x"];

    type Sym = (usize, usize, usize, usize);
    type FileSpec = (usize, usize, Vec<usize>, Vec<Sym>);

    fn file_spec() -> impl Strategy<Value = FileSpec> {
        (
            0usize..2, // repo
            0usize..2, // org
            prop::collection::vec(0usize..VOCAB.len(), 0..10),
            prop::collection::vec(
                (0usize..NAMES.len(), 0usize..3, 0usize..11, 0usize..11),
                0..4,
            ),
        )
    }

    fn extraction(toks: &[usize], syms: &[Sym]) -> graph_core::Extraction {
        let t: Vec<(&str, u32, u32)> = toks
            .iter()
            .enumerate()
            .map(|(i, &v)| (VOCAB[v], i as u32 * 4, i as u32 * 4 + 2))
            .collect();
        let s: Vec<(&str, SymbolKind, u32, u32)> = syms
            .iter()
            .map(|&(n, k, a, b)| {
                (
                    NAMES[n],
                    KINDS[k],
                    a.min(b) as u32 * 4,
                    a.max(b) as u32 * 4 + 3,
                )
            })
            .collect();
        span_ext(&s, &t)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(40))]
        #[test]
        fn v1_and_v2_agree_on_random_corpora(files in prop::collection::vec(file_spec(), 1..5)) {
            let (_d, a, b) = both_backends();
            let mut ingested = 0;
            for (i, (repo, org, toks, syms)) in files.iter().enumerate() {
                let ex = extraction(toks, syms);
                let (o, r, p) = (format!("o{org}"), format!("r{repo}"), format!("f{i}.rs"));
                // Partially overlapping symbols are rejected by both.
                if a.ingest_file(&o, &r, &p, "rust", &ex).is_ok() {
                    b.ingest_file(&o, &r, &p, "rust", &ex).unwrap();
                    ingested += 1;
                } else {
                    prop_assert!(b.ingest_file(&o, &r, &p, "rust", &ex).is_err());
                }
            }
            prop_assume!(ingested > 0);
            for text in VOCAB {
                for grain in [Grain::Token, Grain::Symbol, Grain::File, Grain::Repo, Grain::Org] {
                    for limit in [None, Some(0), Some(1), Some(2), Some(5)] {
                        for (org, repo) in [(None, None), (Some("o0"), None), (Some("o1"), Some("r0"))] {
                            for kind in [None, Some("method")] {
                                let mut q = Query::new(text);
                                q.grain = grain;
                                q.limit = limit;
                                q.org = org.map(Into::into);
                                q.repo = repo.map(Into::into);
                                q.symbol_kind = kind.map(Into::into);
                                prop_assert_eq!(a.search(&q).unwrap(), b.search(&q).unwrap(), "{:?}", q);
                            }
                        }
                    }
                }
            }
            for pat in ["*", "new", "a*", "ab", "x*", "a\\*", "nope"] {
                for limit in [None, Some(1), Some(3)] {
                    for (repo, kind) in [(None, None), (Some("r1"), None), (None, Some("type"))] {
                        let mut q = SymbolQuery::new(pat);
                        q.limit = limit;
                        q.repo = repo.map(Into::into);
                        q.kind = kind.map(Into::into);
                        prop_assert_eq!(a.search_symbols(&q).unwrap(), b.search_symbols(&q).unwrap(), "{:?}", q);
                    }
                }
            }
            prop_assert_eq!(a.describe(None, None).unwrap(), b.describe(None, None).unwrap());
        }
    }
}
