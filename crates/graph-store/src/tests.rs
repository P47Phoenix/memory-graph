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
