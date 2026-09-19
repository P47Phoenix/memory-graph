use super::*;
use graph_core::tokenizer::tokenize;
use graph_core::SymbolDecl;

fn span_of(src: &str, needle: &str, nth: usize) -> Span {
    let start = src.match_indices(needle).nth(nth).unwrap().0;
    let end = start + needle.len();
    let pos = |o: usize| {
        let b = &src[..o];
        (
            1 + b.matches('\n').count() as u32,
            1 + b.rsplit('\n').next().unwrap().chars().count() as u32,
        )
    };
    let (sl, sc) = pos(start);
    let (el, ec) = pos(end);
    Span {
        start: start as u32,
        end: end as u32,
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn sym(name: &str, kind: SymbolKind, span: Span) -> SymbolDecl {
    SymbolDecl {
        name: name.into(),
        kind,
        lang_kind: None,
        span,
    }
}

const RUST: &str = "impl S {\n    fn a() { foo(); foo(); }\n    fn b() { foo(); }\n}\n";
const ZIG: &str = "pub fn main() void { foo(); }\n";

fn setup(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("g.redb")).unwrap();
    let ex = Extraction {
        has_errors: false,
        symbols: vec![
            sym("S", SymbolKind::Type, span_of(RUST, RUST.trim_end(), 0)),
            sym(
                "a",
                SymbolKind::Method,
                span_of(RUST, "fn a() { foo(); foo(); }", 0),
            ),
            sym(
                "b",
                SymbolKind::Method,
                span_of(RUST, "fn b() { foo(); }", 0),
            ),
        ],
        tokens: tokenize(RUST),
    };
    s.ingest_file("o1", "r1", "lib.rs", "rust", &ex).unwrap();
    let z = Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize(ZIG),
    };
    s.ingest_file("o2", "r2", "main.zig", "zig", &z).unwrap();
    s
}

#[test]
fn reopen_and_language_filter() {
    let d = tempfile::tempdir().unwrap();
    drop(setup(d.path()));
    let s = Store::open(d.path().join("g.redb")).unwrap();
    let mut q = Query::new("foo");
    assert_eq!(s.search(&q).unwrap().len(), 4);
    q.language = Some("rust".into());
    let hits = s.search(&q).unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits
        .windows(2)
        .all(|w| w[0].span.unwrap().start < w[1].span.unwrap().start));
    let h = &hits[0];
    assert_eq!(h.symbol.as_deref(), Some("S::a"));
    assert_eq!(
        &RUST[h.span.unwrap().start as usize..h.span.unwrap().end as usize],
        "foo"
    );
}

#[test]
fn grains() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let mut q = Query::new("foo");
    q.grain = Grain::Symbol;
    q.symbol_kind = Some(SymbolKind::Method);
    let h = s.search(&q).unwrap();
    // a (2 hits), b (1 hit), plus the zig file rolled up with no_symbols.
    let counts: Vec<_> = h
        .iter()
        .map(|x| (x.symbol.clone(), x.count, x.no_symbols))
        .collect();
    assert_eq!(
        counts,
        [
            (Some("S::a".into()), 2, false),
            (Some("S::b".into()), 1, false),
            (None, 1, true)
        ]
    );
    q.symbol_kind = None;
    q.grain = Grain::File;
    assert_eq!(
        s.search(&q)
            .unwrap()
            .iter()
            .map(|x| x.count)
            .collect::<Vec<_>>(),
        [3, 1]
    );
    q.grain = Grain::Repo;
    assert_eq!(s.search(&q).unwrap().len(), 2);
    q.grain = Grain::Org;
    q.language = Some("zig".into());
    let o = s.search(&q).unwrap();
    assert_eq!((o.len(), o[0].org.as_str(), o[0].count), (1, "o2", 1));
}

#[test]
fn reindex_no_duplicates() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let before = (
        s.count_nodes(NodeKind::Token).unwrap(),
        s.count_nodes(NodeKind::Symbol).unwrap(),
    );
    let z = Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize(ZIG),
    };
    let st = s.ingest_file("o2", "r2", "main.zig", "zig", &z).unwrap();
    assert!(st.replaced);
    assert_eq!(
        before,
        (
            s.count_nodes(NodeKind::Token).unwrap(),
            s.count_nodes(NodeKind::Symbol).unwrap()
        )
    );
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 4);
    // Shrinking the file removes stale tokens.
    let e = Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize("bar"),
    };
    s.ingest_file("o2", "r2", "main.zig", "zig", &e).unwrap();
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 3);
}

#[test]
fn lock_and_schema_mismatch() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    let s = Store::open(&p).unwrap();
    assert!(matches!(Store::open(&p), Err(StoreError::Locked(_))));
    drop(s);
    {
        let db = Database::create(&p).unwrap();
        let wt = db.begin_write().unwrap();
        wt.open_table(META)
            .unwrap()
            .insert("schema_version", 99)
            .unwrap();
        wt.commit().unwrap();
    }
    assert!(matches!(
        Store::open(&p),
        Err(StoreError::SchemaMismatch { found: 99 })
    ));
}

#[test]
fn parent_lookup() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let h = s.search(&Query::new("main")).unwrap();
    assert_eq!(h.len(), 1);
    let f = s
        .get(
            s.ingest_file("o2", "r2", "main.zig", "zig", &Extraction::default())
                .unwrap()
                .file_id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(s.parent(f.id).unwrap().unwrap().kind, NodeKind::Repo);
}

#[test]
fn partial_overlap_and_bad_spans_rejected_atomically() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let before = s.search(&Query::new("foo")).unwrap().len();
    let src = "aaaa bbbb cccc";
    let mk = |a: &str, b: &str| (span_of(src, a, 0), span_of(src, b, 0));
    let (a, b) = mk("aaaa bbbb", "bbbb cccc"); // b starts inside a, ends after
    let ex = Extraction {
        has_errors: false,
        symbols: vec![sym("A", SymbolKind::Type, a), sym("B", SymbolKind::Type, b)],
        tokens: tokenize(src),
    };
    let err = s
        .ingest_file("o2", "r2", "main.zig", "zig", &ex)
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSpan(_)), "{err}");
    // Old version of the file is intact (transaction aborted).
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), before);
    let mut bad = span_of(src, "aaaa", 0);
    bad.end = 0;
    bad.start = 3;
    let ex = Extraction {
        has_errors: false,
        symbols: vec![sym("A", SymbolKind::Type, bad)],
        tokens: vec![],
    };
    assert!(matches!(
        s.ingest_file("o", "r", "x", "l", &ex),
        Err(StoreError::InvalidSpan(_))
    ));
}

#[test]
fn no_matching_symbol_is_distinct_from_no_symbols() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let mut q = Query::new("impl");
    q.grain = Grain::Symbol;
    q.symbol_kind = Some(SymbolKind::Method);
    // `impl` sits inside type S but not inside a method.
    let h = s.search(&q).unwrap();
    assert_eq!(
        (h.len(), h[0].no_symbols, h[0].no_matching_symbol),
        (1, false, true)
    );
}

#[test]
fn index_bytes_normalizes() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path().join("g.redb")).unwrap();
    s.index_bytes("o", "r", "./a.rs", b"\xEF\xBB\xBFfoo bar", None)
        .unwrap();
    let st = s
        .index_bytes("o", "r", "x/../a.rs", b"foo", Some("Rust"))
        .unwrap();
    assert!(st.replaced);
    assert_eq!((st.path.as_str(), st.language.as_str()), ("a.rs", "rust"));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    let mut q = Query::new("foo");
    q.language = Some("RUST".into());
    assert_eq!(s.search(&q).unwrap().len(), 1);
    // BOM is not a token and does not shift columns beyond its bytes.
    s.index_bytes("o", "r", "b.rs", b"\xEF\xBB\xBFfoo", None)
        .unwrap();
    let h = s.search(&Query::new("foo")).unwrap();
    assert_eq!(h[1].span.unwrap().start_col, 1);
    assert!(matches!(
        s.index_bytes("o", "r", "c.bin", &[0xff, 0xfe], None),
        Err(StoreError::NotUtf8(_))
    ));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
}

#[test]
fn prune_removes_unlisted_files_only() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    s.index_bytes("o2", "r2", "extra.zig", b"foo", None)
        .unwrap();
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 5);
    let keep = ["main.zig".to_string()].into();
    let gone = s.prune_files("o2", "r2", &keep).unwrap();
    assert_eq!(gone, ["extra.zig"]);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    // Other repos untouched; pruned file's tokens are gone from the index.
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 4);
    assert!(s.prune_files("nope", "r", &keep).unwrap().is_empty());
    // Re-adding works (name entry was cleaned).
    s.index_bytes("o2", "r2", "extra.zig", b"foo", None)
        .unwrap();
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
}

#[test]
fn empty_org_or_repo_rejected() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path().join("g.redb")).unwrap();
    assert!(matches!(
        s.index_bytes("", "r", "a", b"x", None),
        Err(StoreError::Rejected(_))
    ));
}
