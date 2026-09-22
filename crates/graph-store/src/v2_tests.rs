//! v2 test gaps (issue #19 QA comment) and a randomized v1-vs-v2 differential.
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

pub(crate) fn both_backends() -> (tempfile::TempDir, Box<dyn Store>, Box<dyn Store>) {
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
    fn corpus() -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
        (1usize..60).prop_flat_map(|n| {
            (
                Just(n),
                prop::collection::vec((0usize..=n, 0usize..=n), 0..12),
            )
        })
    }

    fn build(n: usize, syms: &[(usize, usize)]) -> graph_core::Extraction {
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
