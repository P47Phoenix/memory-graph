//! Store-level tests: opening, the conformance suite, configuration
//! equivalence (`run_differential` across settings) and the refusal of files
//! in another layout. Behaviour shared by every configuration belongs in
//! `conformance.rs`; v2-internal cases live in `v2_tests.rs` and
//! `v2_policy_tests.rs`.
use super::*;
use crate::v2_tests::two_configs;
use sha2::{Digest, Sha256};

fn digest(p: &std::path::Path) -> Vec<u8> {
    Sha256::digest(std::fs::read(p).unwrap()).to_vec()
}

/// A redb file whose meta table holds `schema_version` = `version`, as the
/// retired v1 layout (or an unknown future layout) would stamp it.
pub(crate) fn stamped_file(p: &std::path::Path, version: u64) {
    let db = redb::Database::create(p).unwrap();
    let wt = db.begin_write().unwrap();
    {
        let mut m = wt.open_table(META).unwrap();
        m.insert("schema_version", version).unwrap();
        m.insert("next_id", 1).unwrap();
        wt.open_table(NODES).unwrap();
        wt.open_table(NAMES).unwrap();
        wt.open_table(CATALOG).unwrap();
    }
    wt.commit().unwrap();
}

#[test]
fn open_in_missing_directory_names_the_path() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nope").join("g.redb");
    let e = V2Store::open(&p).err().unwrap().to_string();
    assert!(e.contains("nope"), "{e}");
}

#[test]
fn opening_a_directory_has_no_read_only_hint() {
    let d = tempfile::tempdir().unwrap();
    let msg = V2Store::open(d.path())
        .err()
        .expect("must fail")
        .to_string();
    assert!(msg.contains("cannot open database"), "{msg}");
    assert!(!msg.contains("writable"), "{msg}");
}

#[cfg(unix)]
#[test]
fn read_only_database_gives_clear_error() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    drop(V2Store::open(&p).unwrap());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().write(true).open(&p).is_ok() {
        // Root ignores file modes; the hint mapping is covered deterministically
        // by `common::tests::open_failed_hint_only_for_permission_errors`.
        eprintln!("SKIPPED read_only_database_gives_clear_error: running with write access to 0444 files (root)");
        return;
    }
    let msg = V2Store::open(&p).err().expect("must fail").to_string();
    assert!(
        msg.contains("cannot open database") && msg.contains("must be writable"),
        "{msg}"
    );
}

#[test]
fn store_passes_conformance_suite() {
    conformance::run_all(&|| {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("g2.redb");
        conformance::Harness {
            open: Box::new(move |ex| open_store(&path, ex)),
            exclusive: true,
            guard: Some(Box::new(d)),
        }
    });
}

/// Configuration equivalence: chunk size and cache size never change what a
/// query returns.
#[test]
fn configurations_are_query_equivalent() {
    let (_d, a, b) = two_configs();
    conformance::run_differential(&*a, &*b);
}

/// A batch that crashes mid-way (storage error in one chunk) and is re-run
/// ends up identical to a fresh index, whatever the chunking.
#[test]
fn crash_mid_batch_then_rerun_matches_fresh_index() {
    let (_d, fresh, crashed) = two_configs();
    conformance::run_crash_rerun_differential(&*fresh, &*crashed);
    let (_d, fresh, crashed) = two_configs();
    conformance::run_crash_rerun_differential(&*crashed, &*fresh);
}

/// Zero-length and equal-span symbols: identical across configurations.
#[test]
fn zero_length_and_equal_span_symbols_are_configuration_independent() {
    use graph_core::{Extraction, Span, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
    let sp = |s: u32, e: u32| Span {
        start: s,
        end: e,
        start_line: 1,
        start_col: s + 1,
        end_line: 1,
        end_col: e + 1,
    };
    let sym = |n: &str, k, s, e| SymbolDecl {
        name: n.into(),
        kind: k,
        lang_kind: None,
        span: sp(s, e),
    };
    let tok = |t: &str, s, e| TokenDecl {
        text: t.into(),
        class: TokenClass::Identifier,
        span: sp(s, e),
    };
    let ex = Extraction {
        has_errors: false,
        symbols: vec![
            sym("z", SymbolKind::Function, 4, 4),
            sym("a", SymbolKind::Type, 0, 10),
            sym("b", SymbolKind::Method, 0, 10),
            sym("c", SymbolKind::Method, 4, 8),
            sym("d", SymbolKind::Variable, 4, 8),
        ],
        tokens: vec![
            tok("foo", 0, 3),
            tok("foo", 4, 7),
            tok("foo", 4, 4),
            tok("foo", 8, 10),
        ],
    };
    let (_d, a, b) = two_configs();
    for s in [&a, &b] {
        s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    }
    for grain in [Grain::Token, Grain::Symbol, Grain::File] {
        for kind in [None, Some("method"), Some("function")] {
            let mut q = Query::new("foo");
            q.grain = grain;
            q.symbol_kind = kind.map(Into::into);
            let hits = a.search(&q).unwrap();
            assert_eq!(hits, b.search(&q).unwrap(), "{grain:?} {kind:?}");
            assert!(!hits.is_empty(), "{grain:?} {kind:?}");
        }
    }
    let q = SymbolQuery::new("*");
    assert_eq!(a.search_symbols(&q).unwrap(), b.search_symbols(&q).unwrap());
    assert_eq!(a.search_symbols(&q).unwrap().len(), 5);
    assert_eq!(
        a.describe(None, None).unwrap(),
        b.describe(None, None).unwrap()
    );
    let toks = |s: &dyn Store| {
        s.file_tokens("o", "r", "x.rs")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|n| (n.name, n.span))
            .collect::<Vec<_>>()
    };
    assert_eq!(toks(&*a), toks(&*b));
    assert_eq!(toks(&*a).len(), 4);
}

fn v2_ext(tokens: &[&str], syms: &[(&str, Option<&str>)]) -> graph_core::Extraction {
    use graph_core::{Extraction, Span, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
    let sp = |s: u32, e: u32| Span {
        start: s,
        end: e,
        start_line: 1,
        start_col: s + 1,
        end_line: 1,
        end_col: e + 1,
    };
    let n = tokens.len() as u32;
    Extraction {
        has_errors: false,
        symbols: syms
            .iter()
            .map(|(name, lk)| SymbolDecl {
                name: (*name).into(),
                kind: SymbolKind::Type,
                lang_kind: lk.map(Into::into),
                span: sp(0, n * 2),
            })
            .collect(),
        tokens: tokens
            .iter()
            .enumerate()
            .map(|(i, t)| TokenDecl {
                text: (*t).into(),
                class: TokenClass::Identifier,
                span: sp(i as u32 * 2, i as u32 * 2 + 1),
            })
            .collect(),
    }
}

#[test]
fn v2_get_bounds_and_tags() {
    let d = tempfile::tempdir().unwrap();
    let s = open_store(&d.path().join("b.redb"), vec![]).unwrap();
    // 1 symbol, 4 tokens.
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &v2_ext(&["a", "b", "c", "d"], &[("S", None)]),
    )
    .unwrap();
    let toks = s.file_tokens("o", "r", "x.rs").unwrap().unwrap();
    let sym = s.parent(toks[0].id).unwrap().unwrap();
    assert_eq!(sym.kind, NodeKind::Symbol);
    assert_eq!(s.get(sym.id).unwrap().unwrap(), sym);
    assert_eq!(s.parent(sym.id).unwrap().unwrap().kind, NodeKind::File);
    let tag = |id: u64, t: u64, idx: u64| (id & 0x3fff_ffff_0000_0000) | (t << 62) | idx;
    let any = toks[0].id;
    // One past the last symbol / token, and a symbol index that is a valid
    // token index, are all absent.
    assert!(s.get(tag(any, 1, 1)).unwrap().is_none());
    assert!(s.get(tag(any, 1, 3)).unwrap().is_none());
    assert!(s.get(tag(any, 2, 4)).unwrap().is_none());
    assert!(s.get(tag(any, 2, 3)).unwrap().is_some());
    assert!(s.get(tag(any, 3, 0)).unwrap().is_none());
    assert!(s.get(tag(any, 1, 0)).unwrap().is_some());
    // A token index that is a valid symbol index but wrong tag stays a token.
    assert_eq!(
        s.get(tag(any, 2, 0)).unwrap().unwrap().kind,
        NodeKind::Token
    );
}

#[test]
fn v2_symbol_filters_and_stale_index() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("b.redb");
    let st = V2Store::open(&path).unwrap();
    let ex = v2_ext(&["a", "b"], &[("S", None)]);
    st.ingest_file("o", "r", "one.rs", "rust", &ex).unwrap();
    st.ingest_file("o", "r", "two.rs", "rust", &ex).unwrap();
    let mut q = SymbolQuery::new("S");
    assert_eq!(st.search_symbols(&q).unwrap().len(), 2);
    q.file = Some("two.rs".into());
    let hits = st.search_symbols(&q).unwrap();
    assert_eq!((hits.len(), hits[0].file.as_str()), (1, "two.rs"));
    q.file = Some("none.rs".into());
    assert!(st.search_symbols(&q).unwrap().is_empty());
    // Stale index entries are skipped: index == symbol count, a token id,
    // a missing file.
    let sym = st
        .file_tokens("o", "r", "one.rs")
        .unwrap()
        .and_then(|t| st.parent(t[0].id).unwrap())
        .unwrap();
    let base = sym.id & 0x3fff_ffff_0000_0000;
    for id in [
        (1u64 << 62) | base | 1,
        (2u64 << 62) | base,
        (1u64 << 62) | (99 << 32),
    ] {
        st.inject_symbol_index("S", id);
    }
    assert_eq!(st.search_symbols(&SymbolQuery::new("S")).unwrap().len(), 2);
}

#[test]
fn v2_replace_and_prune_clean_symbol_index_and_catalog() {
    let d = tempfile::tempdir().unwrap();
    let s = open_store(&d.path().join("b.redb"), vec![]).unwrap();
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &v2_ext(&["a"], &[("foo", Some("struct"))]),
    )
    .unwrap();
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "rust",
        &v2_ext(&["a"], &[("bar", Some("enum"))]),
    )
    .unwrap();
    // The old name must be gone from the index, not resolve to the new symbol.
    assert!(s
        .search_symbols(&SymbolQuery::new("foo"))
        .unwrap()
        .is_empty());
    assert_eq!(s.search_symbols(&SymbolQuery::new("bar")).unwrap().len(), 1);
    let d1 = s.describe(None, None).unwrap();
    assert_eq!(d1, s.describe_by_scan(None, None).unwrap());
    let kinds = &d1[0].languages["rust"].symbol_kinds;
    assert_eq!(kinds.len(), 1);
    assert!(kinds.contains_key("type/enum"), "{kinds:?}");
}

/// A file in the retired v1 layout is refused with `LegacyFormat` (naming
/// the path, the version and the migration path) and its bytes are never
/// touched, through every open path.
#[test]
fn legacy_v1_file_is_refused_with_a_migration_hint_and_untouched() {
    let d = tempfile::tempdir().unwrap();
    for version in LEGACY_SCHEMA_VERSIONS {
        let p = d.path().join(format!("v{version}.redb"));
        stamped_file(&p, version);
        let before = digest(&p);
        for attempt in 0..2 {
            let err = match attempt {
                0 => V2Store::open(&p).err().expect("refused"),
                _ => open_store(&p, vec![]).err().expect("refused"),
            };
            match &err {
                StoreError::LegacyFormat { path, version: v } => {
                    assert!(path.contains(&format!("v{version}.redb")), "{path}");
                    assert_eq!(*v, version);
                }
                other => panic!("wrong error: {other:?}"),
            }
            let msg = err.to_string();
            for needle in [
                "retired v1 format",
                &format!("schema version {version}"),
                "Re-index",
                "v1-last",
                "memory-graph migrate",
            ] {
                assert!(msg.contains(needle), "{needle}: {msg}");
            }
            assert_eq!(digest(&p), before, "v{version} bytes unchanged");
        }
        assert!(matches!(
            detect_format(&p),
            Err(StoreError::LegacyFormat { version: v, .. }) if v == version
        ));
        assert_eq!(digest(&p), before, "v{version} bytes unchanged by detect");
    }
    // An unknown (future) layout is a schema mismatch, also untouched.
    let p = d.path().join("future.redb");
    stamped_file(&p, V2_SCHEMA_VERSION + 1);
    let before = digest(&p);
    assert!(matches!(
        V2Store::open(&p),
        Err(StoreError::SchemaMismatch { found }) if found == V2_SCHEMA_VERSION + 1
    ));
    assert_eq!(digest(&p), before);
}

#[test]
fn differential_against_itself() {
    let mk = || {
        let d = tempfile::tempdir().unwrap();
        let s = open_store(&d.path().join("g.redb"), vec![]).unwrap();
        (d, s)
    };
    let ((_da, a), (_db, b)) = (mk(), mk());
    conformance::run_differential(&*a, &*b);
}

/// The trait must stay object-safe and shareable across threads.
#[test]
fn store_trait_is_object_safe_send_sync() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn Store>();
    assert_send_sync::<V2Store>();
    fn assert_send<T: Send + ?Sized>() {}
    assert_send::<dyn StoreRead + Send>();
}
