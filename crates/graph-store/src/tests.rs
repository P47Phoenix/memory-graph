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

fn setup(dir: &std::path::Path) -> RedbStore {
    let s = RedbStore::open(dir.join("g.redb")).unwrap();
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
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
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
    q.symbol_kind = Some("method".into());
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
    let s = RedbStore::open(&p).unwrap();
    assert!(matches!(RedbStore::open(&p), Err(StoreError::Locked(_))));
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
        RedbStore::open(&p),
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
    q.symbol_kind = Some("method".into());
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
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
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
    s.index_bytes_with_origin(
        "o2",
        "r2",
        "extra.zig",
        b"foo",
        None,
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 5);
    let keep = ["main.zig".to_string()].into();
    let gone = s.prune_files("o2", "r2", &keep, false).unwrap();
    assert_eq!(gone, ["extra.zig"]);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    // Other repos untouched; pruned file's tokens are gone from the index.
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 4);
    assert!(s.prune_files("nope", "r", &keep, false).unwrap().is_empty());
    // Re-adding works (name entry was cleaned).
    s.index_bytes("o2", "r2", "extra.zig", b"foo", None)
        .unwrap();
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
}

#[test]
fn empty_org_or_repo_rejected() {
    let d = tempfile::tempdir().unwrap();
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    assert!(matches!(
        s.index_bytes("", "r", "a", b"x", None),
        Err(StoreError::Rejected(_))
    ));
}

fn origin_of(s: &RedbStore, org: &str, repo: &str, file: &str) -> Option<String> {
    let rt = s.db.begin_read().unwrap();
    let names = rt.open_table(NAMES).unwrap();
    let o = names
        .get(name_key(None, NodeKind::Org, org).as_str())
        .unwrap()
        .unwrap()
        .value();
    let r = names
        .get(name_key(Some(o), NodeKind::Repo, repo).as_str())
        .unwrap()
        .unwrap()
        .value();
    let f = names
        .get(name_key(Some(r), NodeKind::File, file).as_str())
        .unwrap()
        .unwrap()
        .value();
    s.get(f).unwrap().unwrap().origin
}

#[test]
fn origin_marker_last_ingest_wins_and_gates_prune() {
    let d = tempfile::tempdir().unwrap();
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    let keep = std::collections::HashSet::new();
    s.index_bytes("o", "r", "a.txt", b"x", None).unwrap();
    assert_eq!(origin_of(&s, "o", "r", "a.txt"), None);
    // Unmarked files are never pruned.
    assert!(s.prune_files("o", "r", &keep, false).unwrap().is_empty());
    s.index_bytes_with_origin("o", "r", "a.txt", b"x", None, Some(ORIGIN_DIRECTORY))
        .unwrap();
    assert_eq!(
        origin_of(&s, "o", "r", "a.txt").as_deref(),
        Some(ORIGIN_DIRECTORY)
    );
    // Dry run reports but keeps.
    assert_eq!(s.prune_files("o", "r", &keep, true).unwrap(), ["a.txt"]);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    // A later plain ingest clears the mark.
    s.index_bytes("o", "r", "a.txt", b"x", None).unwrap();
    assert_eq!(origin_of(&s, "o", "r", "a.txt"), None);
    assert!(s.prune_files("o", "r", &keep, false).unwrap().is_empty());
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
}

#[test]
fn nodes_without_origin_field_still_deserialize() {
    let old = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"span":null}"#;
    assert_eq!(dec(old).unwrap().origin, None);
    let older = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"span":null}"#;
    assert_eq!(dec(older).unwrap().origin, None);
}

#[test]
fn open_in_missing_directory_names_the_path() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nope").join("g.redb");
    let e = RedbStore::open(&p).err().unwrap().to_string();
    assert!(e.contains("nope"), "{e}");
}

fn symbol_fixture(dir: &std::path::Path) -> RedbStore {
    let s = RedbStore::open(dir.join("g.redb")).unwrap();
    let src = "fn parse() {}\nstruct S;\nimpl S { fn parse(&self) {} fn parser(&self) {} fn other(&self) {} }\n";
    let ex = Extraction {
        has_errors: false,
        symbols: vec![
            SymbolDecl {
                name: "parse".into(),
                kind: SymbolKind::Function,
                lang_kind: Some("fn".into()),
                span: span_of(src, "fn parse() {}", 0),
            },
            SymbolDecl {
                name: "S".into(),
                kind: SymbolKind::Other,
                lang_kind: Some("impl".into()),
                span: span_of(
                    src,
                    "impl S { fn parse(&self) {} fn parser(&self) {} fn other(&self) {} }",
                    0,
                ),
            },
            SymbolDecl {
                name: "parse".into(),
                kind: SymbolKind::Method,
                lang_kind: Some("fn".into()),
                span: span_of(src, "fn parse(&self) {}", 0),
            },
            SymbolDecl {
                name: "parser".into(),
                kind: SymbolKind::Method,
                lang_kind: Some("fn".into()),
                span: span_of(src, "fn parser(&self) {}", 0),
            },
            SymbolDecl {
                name: "other".into(),
                kind: SymbolKind::Method,
                lang_kind: Some("fn".into()),
                span: span_of(src, "fn other(&self) {}", 0),
            },
        ],
        tokens: tokenize(src),
    };
    s.ingest_file("o1", "r1", "a.rs", "rust", &ex).unwrap();
    s.ingest_file("o1", "r2", "b.rs", "rust", &ex).unwrap();
    // A Python file with a `parse` function (language filter).
    let py = Extraction {
        has_errors: false,
        symbols: vec![SymbolDecl {
            name: "parse".into(),
            kind: SymbolKind::Function,
            lang_kind: Some("def".into()),
            span: span_of("def parse(): pass", "def parse(): pass", 0),
        }],
        tokens: tokenize("def parse(): pass"),
    };
    s.ingest_file("o2", "r3", "p.py", "python", &py).unwrap();
    s
}

#[test]
fn symbol_search_name_kind_language_scope_prefix() {
    let d = tempfile::tempdir().unwrap();
    let s = symbol_fixture(d.path());
    let names = |q: &SymbolQuery| -> Vec<String> {
        s.search_symbols(q)
            .unwrap()
            .iter()
            .map(|h| format!("{}/{}:{}", h.org, h.repo, h.qualified))
            .collect()
    };
    // Exact name: 2 rust (function + method) x 2 repos + python.
    assert_eq!(names(&SymbolQuery::new("parse")).len(), 5);
    let mut q = SymbolQuery::new("parse");
    q.kind = Some("method".into());
    assert_eq!(names(&q), ["o1/r1:S::parse", "o1/r2:S::parse"]);
    let mut q = SymbolQuery::new("parse");
    q.language = Some("Python".into());
    assert_eq!(names(&q), ["o2/r3:parse"]);
    let mut q = SymbolQuery::new("parse");
    q.repo = Some("r2".into());
    assert_eq!(names(&q), ["o1/r2:parse", "o1/r2:S::parse"]);
    q.file = Some("./b.rs".into());
    assert_eq!(names(&q).len(), 2);
    q.file = Some("a.rs".into());
    assert!(names(&q).is_empty());
    // Prefix: parse, parse, parser per rust repo (+ python parse), not `other`.
    let mut q = SymbolQuery::new("pars*");
    q.org = Some("o1".into());
    assert_eq!(names(&q).len(), 6);
    assert!(names(&SymbolQuery::new("nomatch*")).is_empty());
    assert!(names(&SymbolQuery::new("Parse")).is_empty()); // case-sensitive
                                                           // Path and kind strings.
    let h = &s.search_symbols(&SymbolQuery::new("parser")).unwrap()[0];
    assert_eq!(
        (
            h.qualified.as_str(),
            h.lang_kind.as_deref(),
            h.file.as_str()
        ),
        ("S::parser", Some("fn"), "a.rs")
    );
    assert_eq!(h.span.unwrap().start_line, 3);
}

#[test]
fn symbol_index_follows_reindex_and_prune() {
    let d = tempfile::tempdir().unwrap();
    let s = symbol_fixture(d.path());
    // Re-index a.rs without symbols: its symbols leave the index.
    s.ingest_file(
        "o1",
        "r1",
        "a.rs",
        "rust",
        &Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: tokenize("x"),
        },
    )
    .unwrap();
    let mut q = SymbolQuery::new("parse");
    q.repo = Some("r1".into());
    assert!(s.search_symbols(&q).unwrap().is_empty());
    // Pruned directory-origin file leaves the index too.
    s.ingest_file_with_origin(
        "o1",
        "r2",
        "b.rs",
        "rust",
        &Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: tokenize("x"),
        },
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    s.prune_files("o1", "r2", &Default::default(), false)
        .unwrap();
    assert_eq!(
        s.search_symbols(&SymbolQuery::new("parse")).unwrap().len(),
        1
    ); // python only
}

#[test]
fn symbol_index_backfilled_for_older_databases() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    drop(symbol_fixture(d.path()));
    {
        // Simulate a database from before the symbol index existed.
        let db = Database::create(&p).unwrap();
        let wt = db.begin_write().unwrap();
        wt.delete_multimap_table(SYMBOLS).unwrap();
        wt.commit().unwrap();
    }
    let s = RedbStore::open(&p).unwrap();
    assert_eq!(
        s.search_symbols(&SymbolQuery::new("parse")).unwrap().len(),
        5
    );
}

#[test]
fn describe_polyglot_repo() {
    let d = tempfile::tempdir().unwrap();
    let s = symbol_fixture(d.path());
    let all = s.describe(None, None).unwrap();
    assert_eq!(all.len(), 3);
    let r1 = &s.describe(Some("o1"), Some("r1")).unwrap()[0];
    let rust = &r1.languages["rust"];
    assert_eq!((r1.files, rust.files, rust.symbols), (1, 1, 5));
    assert_eq!(rust.symbol_kinds["method/fn"], 3);
    assert_eq!(rust.symbol_kinds["other/impl"], 1);
    assert!(r1.kind_names(Some("RUST")).contains("impl"));
    assert!(r1.kind_names(Some("python")).is_empty());
    assert!(s.describe(Some("nope"), None).unwrap().is_empty());
    // Matching a language-specific kind name.
    let mut q = SymbolQuery::new("S");
    q.kind = Some("impl".into());
    assert_eq!(s.search_symbols(&q).unwrap().len(), 2);
}

fn rust_store(dir: &std::path::Path) -> RedbStore {
    let mut s = RedbStore::open(dir.join("g.redb")).unwrap();
    s.register(Box::new(graph_lang_rust::RustExtractor));
    s
}

#[test]
fn stale_symbol_index_is_rebuilt_on_version_change() {
    let d = tempfile::tempdir().unwrap();
    {
        let s = rust_store(d.path());
        s.index_bytes("o", "r", "a.rs", b"fn foo() {}\nfn bar() {}\n", None)
            .unwrap();
        // Corrupt the derived index and mark it as an older version.
        let wt = s.db.begin_write().unwrap();
        {
            wt.delete_multimap_table(SYMBOLS).unwrap();
            let mut idx = wt.open_multimap_table(SYMBOLS).unwrap();
            idx.insert("stale", 999_999u64).unwrap();
            wt.open_table(META)
                .unwrap()
                .insert("symbol_index_version", SYMBOL_INDEX_VERSION - 1)
                .unwrap();
        }
        wt.commit().unwrap();
    }
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    assert_eq!(s.search_symbols(&SymbolQuery::new("foo")).unwrap().len(), 1);
    assert!(s
        .search_symbols(&SymbolQuery::new("stale"))
        .unwrap()
        .is_empty());
    let rt = s.db.begin_read().unwrap();
    let v = rt
        .open_table(META)
        .unwrap()
        .get("symbol_index_version")
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(v, SYMBOL_INDEX_VERSION);
}

#[test]
fn search_symbols_skips_dangling_ids() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"fn foo() {}\n", None)
        .unwrap();
    let wt = s.db.begin_write().unwrap();
    wt.open_multimap_table(SYMBOLS)
        .unwrap()
        .insert("foo", 999_999u64)
        .unwrap();
    wt.commit().unwrap();
    assert_eq!(s.search_symbols(&SymbolQuery::new("foo")).unwrap().len(), 1);
}

#[test]
fn open_failed_hint_only_for_permission_errors() {
    let path = std::path::Path::new("x.redb");
    let io = |k: std::io::ErrorKind| {
        DatabaseError::Storage(redb::StorageError::Io(std::io::Error::from(k)))
    };
    let denied = open_failed(path, &io(std::io::ErrorKind::PermissionDenied)).to_string();
    assert!(denied.contains("cannot open database") && denied.contains("must be writable"));
    let ro = open_failed(path, &io(std::io::ErrorKind::ReadOnlyFilesystem)).to_string();
    assert!(ro.contains("must be writable"), "{ro}");
    let other = open_failed(path, &io(std::io::ErrorKind::NotFound)).to_string();
    assert!(!other.contains("writable"), "{other}");
}

#[test]
fn opening_a_directory_has_no_read_only_hint() {
    let d = tempfile::tempdir().unwrap();
    let msg = RedbStore::open(d.path())
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
    drop(RedbStore::open(&p).unwrap());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().write(true).open(&p).is_ok() {
        // Root ignores file modes; the hint mapping is covered deterministically
        // by `open_failed_hint_only_for_permission_errors`.
        eprintln!("SKIPPED read_only_database_gives_clear_error: running with write access to 0444 files (root)");
        return;
    }
    let msg = RedbStore::open(&p).err().expect("must fail").to_string();
    assert!(
        msg.contains("cannot open database") && msg.contains("must be writable"),
        "{msg}"
    );
}

fn set_meta(s: &RedbStore, key: &str, v: Option<u64>) {
    let wt = s.db.begin_write().unwrap();
    {
        let mut m = wt.open_table(META).unwrap();
        match v {
            Some(v) => m.insert(key, v).unwrap(),
            None => m.remove(key).unwrap(),
        };
    }
    wt.commit().unwrap();
}

fn meta(s: &RedbStore, key: &str) -> Option<u64> {
    let rt = s.db.begin_read().unwrap();
    let t = rt.open_table(META).unwrap();
    let v = t.get(key).unwrap().map(|v| v.value());
    v
}

#[test]
fn newer_symbol_index_is_refused_and_untouched() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    {
        let s = rust_store(d.path());
        s.index_bytes("o", "r", "a.rs", b"fn foo() {}\n", None)
            .unwrap();
        set_meta(&s, "symbol_index_version", Some(SYMBOL_INDEX_VERSION + 1));
    }
    let err = RedbStore::open(&p).err().expect("must refuse");
    assert!(
        matches!(err, StoreError::IndexTooNew { found } if found == SYMBOL_INDEX_VERSION + 1),
        "{err}"
    );
    assert!(err.to_string().contains("newer"), "{err}");
    // Nothing was rewritten: the stamp is unchanged and the index still there.
    let db = Database::create(&p).unwrap();
    let rt = db.begin_read().unwrap();
    let v = rt
        .open_table(META)
        .unwrap()
        .get("symbol_index_version")
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(v, SYMBOL_INDEX_VERSION + 1);
    assert_eq!(
        rt.open_multimap_table(SYMBOLS)
            .unwrap()
            .get("foo")
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn missing_version_key_with_existing_table_is_rebuilt() {
    let d = tempfile::tempdir().unwrap();
    {
        let s = rust_store(d.path());
        s.index_bytes("o", "r", "a.rs", b"fn foo() {}\n", None)
            .unwrap();
        set_meta(&s, "symbol_index_version", None);
        let wt = s.db.begin_write().unwrap();
        {
            let mut idx = wt.open_multimap_table(SYMBOLS).unwrap();
            idx.insert("stale", 999_999u64).unwrap();
        }
        wt.commit().unwrap();
        assert_eq!(meta(&s, "symbol_index_version"), None);
    }
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    assert_eq!(meta(&s, "symbol_index_version"), Some(SYMBOL_INDEX_VERSION));
    assert_eq!(s.search_symbols(&SymbolQuery::new("foo")).unwrap().len(), 1);
    assert!(s
        .search_symbols(&SymbolQuery::new("stale"))
        .unwrap()
        .is_empty());
}

#[test]
fn search_symbols_skips_ids_of_non_symbol_nodes() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn foo() {}\n", None)
        .unwrap();
    let wt = s.db.begin_write().unwrap();
    wt.open_multimap_table(SYMBOLS)
        .unwrap()
        .insert("foo", st.file_id)
        .unwrap();
    wt.commit().unwrap();
    assert_eq!(s.search_symbols(&SymbolQuery::new("foo")).unwrap().len(), 1);
}

#[test]
fn same_file_and_offset_ties_break_by_node_id() {
    let src = "abcdef\n";
    let zero = span_of(src, "c", 0);
    let zero = Span {
        end: zero.start,
        end_col: zero.start_col,
        ..zero
    };
    let order = |first: &str, second: &str| {
        let d = tempfile::tempdir().unwrap();
        let s = RedbStore::open(d.path().join("g.redb")).unwrap();
        let mk = |k: &str| SymbolDecl {
            lang_kind: Some(k.into()),
            ..sym("dup", SymbolKind::Function, zero)
        };
        let ex = Extraction {
            has_errors: false,
            symbols: vec![mk(first), mk(second)],
            tokens: vec![],
        };
        s.ingest_file("o", "r", "a.txt", "text", &ex).unwrap();
        s.search_symbols(&SymbolQuery::new("dup"))
            .unwrap()
            .into_iter()
            .map(|h| h.lang_kind.unwrap())
            .collect::<Vec<_>>()
    };
    let ab = order("a", "b");
    assert_eq!(ab.len(), 2);
    assert_eq!(ab, order("a", "b"));
    let ba = order("b", "a");
    assert_eq!(ba.len(), 2);
    assert_ne!(ab, ba, "ties follow insertion (node id) order");
}

struct CountingExtractor(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Extractor for CountingExtractor {
    fn language(&self) -> &str {
        "count"
    }
    fn extract(&self, source: &str) -> Extraction {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut bad = span_of("xxxx", "xx", 0);
        if source.starts_with("bad") {
            bad.start = 3;
            bad.end = 1;
        }
        Extraction {
            has_errors: false,
            symbols: vec![sym("s", SymbolKind::Function, bad)],
            tokens: vec![],
        }
    }
}

#[test]
fn batch_invalid_span_fails_only_that_file() {
    let d = tempfile::tempdir().unwrap();
    let mut s = RedbStore::open(d.path().join("g.redb")).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    s.register(Box::new(CountingExtractor(calls.clone())));
    let f = |p, b: &'static [u8]| BatchFile {
        path: p,
        bytes: b,
        language: Some("count"),
        origin: None,
    };
    // A previously stored version of the file that will now fail.
    s.index_bytes("o", "r", "old.c", b"xxxx", Some("count"))
        .unwrap();
    let files = [
        f("ok1.c", b"xxxx"),
        f("bad.c", b"bad-span"),
        f("old.c", b"bad-again"),
        f("ok2.c", b"xxxx"),
    ];
    let out = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert_eq!(out.len(), 4);
    assert!(out[0].is_ok() && out[3].is_ok());
    for i in [1, 2] {
        match &out[i] {
            Err(StoreError::InvalidSpan(m)) => {
                assert!(m.contains(files[i].path), "message names the path: {m}")
            }
            other => panic!("expected InvalidSpan, got {other:?}"),
        }
    }
    // Every file was extracted (plus the priming call): no early abort.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert!(s.file_tokens("o", "r", "ok1.c").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "ok2.c").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "bad.c").unwrap().is_none());
    // The failing re-index left the old stored version intact.
    let old = s.file_tokens("o", "r", "old.c").unwrap().unwrap();
    assert!(old.is_empty());
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    assert_catalog_matches_scan(&s, "after per-file failure");
}

#[test]
fn batch_hard_error_still_aborts_and_rolls_back() {
    let d = tempfile::tempdir().unwrap();
    let mut s = RedbStore::open(d.path().join("g.redb")).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    s.register(Box::new(CountingExtractor(calls.clone())));
    let f = |p, l| BatchFile {
        path: p,
        bytes: b"xxxx",
        language: Some(l),
        origin: None,
    };
    // A NUL in the language is rejected as a whole-batch error.
    let files = [
        f("ok1.c", "count"),
        f("nul.c", "a\0b"),
        f("never.c", "count"),
    ];
    let err = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap_err();
    assert!(matches!(err, StoreError::Rejected(_)), "{err}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert_eq!(s.count_nodes(NodeKind::Org).unwrap(), 0);
}

#[test]
fn raw_string_with_inner_quote_indexes_through_rust_extractor() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let src = "const A: &str = r#\"a\"b\"#;\n";
    let st = s
        .index_bytes("o", "r", "a.rs", src.as_bytes(), None)
        .unwrap();
    assert_eq!(st.symbols, 1);
    let toks = s.file_tokens("o", "r", "a.rs").unwrap().unwrap();
    assert!(!toks.is_empty());
    for t in &toks {
        let sp = t.span.unwrap();
        assert_eq!(&src[sp.start as usize..sp.end as usize], t.name);
    }
    assert!(toks.iter().any(|t| t.name == "r#\"a\"b\"#"));
}

struct OldTokenizerRust;
impl Extractor for OldTokenizerRust {
    fn language(&self) -> &str {
        "rust"
    }
    fn version(&self) -> String {
        // What the Rust extractor reported before its raw-string tokens (#16).
        "rust-syn-1+tok1".to_string()
    }
    fn extract(&self, source: &str) -> Extraction {
        Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: tokenize(source),
        }
    }
}

#[test]
fn files_indexed_with_previous_tokenizer_version_reindex() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("g.redb");
    let src = b"const A: &str = r#\"a\"b\"#;\n";
    {
        let mut old = RedbStore::open(&path).unwrap();
        old.register(Box::new(OldTokenizerRust));
        assert!(
            !old.index_bytes("o", "r", "a.rs", src, None)
                .unwrap()
                .unchanged
        );
        assert!(
            old.index_bytes("o", "r", "a.rs", src, None)
                .unwrap()
                .unchanged
        );
    }
    let mut s = RedbStore::open(&path).unwrap();
    s.register(Box::new(graph_lang_rust::RustExtractor));
    let st = s.index_bytes("o", "r", "a.rs", src, None).unwrap();
    assert!(st.replaced && !st.unchanged && st.symbols == 1);
}

#[test]
fn batch_duplicate_paths_collapse_to_one_file() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let f = |p, b: &'static [u8]| BatchFile {
        path: p,
        bytes: b,
        language: None,
        origin: Some(ORIGIN_DIRECTORY),
    };
    let out = s
        .index_batch(
            "o",
            "r",
            &[f("./a.rs", b"fn one() {}\n"), f("a.rs", b"fn two() {}\n")],
            IndexOptions::default(),
        )
        .unwrap();
    assert!(!out[0].as_ref().unwrap().replaced);
    assert!(out[1].as_ref().unwrap().replaced);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert!(s
        .search_symbols(&SymbolQuery::new("one"))
        .unwrap()
        .is_empty());
    assert_eq!(s.search_symbols(&SymbolQuery::new("two")).unwrap().len(), 1);
}

#[test]
fn kind_filter_is_case_insensitive() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"struct Foo;\nfn foo() {}\n", None)
        .unwrap();
    for k in ["struct", "STRUCT", "Struct", "TYPE", "Function"] {
        let mut q = SymbolQuery::new("*");
        q.kind = Some(k.into());
        assert!(!s.search_symbols(&q).unwrap().is_empty(), "{k}");
    }
}

#[test]
fn limit_is_deterministic_for_search_and_symbols() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    for f in ["b.rs", "a.rs", "c.rs"] {
        s.index_bytes("o", "r", f, b"fn foo() { foo(); }\n", None)
            .unwrap();
    }
    let all = s.search_symbols(&SymbolQuery::new("foo")).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].file, "a.rs");
    let mut q = SymbolQuery::new("foo");
    q.limit = Some(2);
    assert_eq!(s.search_symbols(&q).unwrap(), all[..2]);
    let mut q = Query::new("foo");
    let full = s.search(&q).unwrap();
    q.limit = Some(4);
    assert_eq!(s.search(&q).unwrap(), full[..4]);
    assert_eq!(full[0].file.as_deref(), Some("a.rs"));
}

#[test]
fn symbol_pattern_edge_cases() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    for bad in ["", "**", "a**"] {
        assert!(
            matches!(
                s.search_symbols(&SymbolQuery::new(bad)),
                Err(StoreError::Rejected(_))
            ),
            "{bad:?}"
        );
    }
    let mut q = SymbolQuery::new("S");
    q.org = Some(String::new());
    assert!(matches!(s.search_symbols(&q), Err(StoreError::Rejected(_))));
    // `*` lists everything.
    assert_eq!(s.search_symbols(&SymbolQuery::new("*")).unwrap().len(), 3);
    // A symbol whose name is a literal `*` is reachable with `\*`.
    let src = "fn f() {}\n";
    let ex = Extraction {
        has_errors: false,
        symbols: vec![sym("*", SymbolKind::Function, span_of(src, "fn f() {}", 0))],
        tokens: tokenize(src),
    };
    s.ingest_file("o3", "r3", "star.rs", "rust", &ex).unwrap();
    let hits = s.search_symbols(&SymbolQuery::new("\\*")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "*");
}

#[test]
fn ingest_file_lowercases_language() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let ex = Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize("x"),
    };
    let st = s.ingest_file("o", "r", "x.txt", "RuSt", &ex).unwrap();
    assert_eq!(st.language, "rust");
    let mut q = Query::new("x");
    q.language = Some("rust".into());
    assert_eq!(s.search(&q).unwrap().len(), 1);
}

#[test]
fn kind_other_matches_uncategorised_symbols() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path());
    let src = "thing\n";
    let ex = Extraction {
        has_errors: false,
        symbols: vec![sym("thing", SymbolKind::Other, span_of(src, "thing", 0))],
        tokens: tokenize(src),
    };
    s.ingest_file("o", "r", "t.zig", "zig", &ex).unwrap();
    let mut q = SymbolQuery::new("thing");
    q.kind = Some("other".into());
    assert_eq!(s.search_symbols(&q).unwrap().len(), 1);
    let mut sq = Query::new("thing");
    (sq.grain, sq.symbol_kind) = (Grain::Symbol, Some("other".into()));
    assert_eq!(sq_symbol(&s, &sq), Some("thing".into()));
    let info = &s.describe(Some("o"), Some("r")).unwrap()[0];
    assert!(info.kind_names(None).contains("other"));
}

fn sq_symbol(s: &RedbStore, q: &Query) -> Option<String> {
    s.search(q).unwrap()[0].symbol.clone()
}

#[test]
fn batch_matches_individual_ingest() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let files = [
        BatchFile {
            path: "./a.rs",
            bytes: b"fn foo() {}\n",
            language: None,
            origin: Some(ORIGIN_DIRECTORY),
        },
        BatchFile {
            path: "bad.txt",
            bytes: &[0xff, 0xfe],
            language: None,
            origin: None,
        },
        BatchFile {
            path: "b.py",
            bytes: b"x = 1\n",
            language: Some("PYTHON"),
            origin: None,
        },
    ];
    let out = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert_eq!(out.len(), 3);
    let a = out[0].as_ref().unwrap();
    assert_eq!(
        (a.path.as_str(), a.language.as_str(), a.symbols),
        ("a.rs", "rust", 1)
    );
    assert!(matches!(out[1], Err(StoreError::NotUtf8(_))));
    assert_eq!(out[2].as_ref().unwrap().language, "python");
    assert_eq!(s.file_tokens("o", "r", "b.py").unwrap().unwrap().len(), 3);
    assert!(s.file_tokens("o", "r", "bad.txt").unwrap().is_none());
    // Re-running skips unchanged files; forcing replaces, never duplicates.
    let again = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert!(again[0].as_ref().unwrap().unchanged);
    let again = s
        .index_batch("o", "r", &files, IndexOptions { reindex: true })
        .unwrap();
    assert!(again[0].as_ref().unwrap().replaced);
    assert_eq!(s.search_symbols(&SymbolQuery::new("foo")).unwrap().len(), 1);
}

// --- skip unchanged files -------------------------------------------------

fn file_token_ids(s: &RedbStore, path: &str) -> Vec<NodeId> {
    s.file_tokens("o", "r", path)
        .unwrap()
        .unwrap()
        .iter()
        .map(|n| n.id)
        .collect()
}

fn file_fingerprint(s: &RedbStore, path: &str) -> Option<String> {
    let rt = s.db.begin_read().unwrap();
    let names = rt.open_table(NAMES).unwrap();
    let o = names
        .get(name_key(None, NodeKind::Org, "o").as_str())
        .unwrap()
        .unwrap()
        .value();
    let r = names
        .get(name_key(Some(o), NodeKind::Repo, "r").as_str())
        .unwrap()
        .unwrap()
        .value();
    let f = names
        .get(name_key(Some(r), NodeKind::File, path).as_str())
        .unwrap()
        .unwrap()
        .value();
    s.get(f).unwrap().unwrap().fingerprint
}

#[test]
fn unchanged_file_is_skipped_without_writes() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let src = b"fn foo() { foo(); }\n";
    let first = s.index_bytes("o", "r", "a.rs", src, None).unwrap();
    assert!(!first.unchanged && !first.replaced);
    let ids = file_token_ids(&s, "a.rs");
    let next = meta(&s, "next_id");
    let nodes = s.count_nodes(NodeKind::Token).unwrap();
    let second = s.index_bytes("o", "r", "a.rs", src, None).unwrap();
    assert!(second.unchanged && !second.replaced);
    assert_eq!((second.symbols, second.tokens), (0, 0));
    assert_eq!(second.file_id, first.file_id);
    assert_eq!(second.language, "rust");
    assert_eq!(file_token_ids(&s, "a.rs"), ids);
    assert_eq!(meta(&s, "next_id"), next, "no ids allocated");
    assert_eq!(s.count_nodes(NodeKind::Token).unwrap(), nodes);
    assert!(file_fingerprint(&s, "a.rs").unwrap().starts_with("sha256:"));
}

#[test]
fn unchanged_file_still_updates_origin() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes_with_origin("o", "r", "a.rs", b"fn f() {}\n", None, None)
        .unwrap();
    let st = s
        .index_bytes_with_origin(
            "o",
            "r",
            "a.rs",
            b"fn f() {}\n",
            None,
            Some(ORIGIN_DIRECTORY),
        )
        .unwrap();
    assert!(st.unchanged);
    assert_eq!(
        origin_of(&s, "o", "r", "a.rs").as_deref(),
        Some(ORIGIN_DIRECTORY)
    );
    let st = s
        .index_bytes_with_origin("o", "r", "a.rs", b"fn f() {}\n", None, None)
        .unwrap();
    assert!(st.unchanged);
    assert_eq!(origin_of(&s, "o", "r", "a.rs"), None);
}

#[test]
fn changed_content_or_language_reindexes_without_duplicates() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn g() {}\nfn h() {}\n", None)
        .unwrap();
    assert!(st.replaced && !st.unchanged);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert_eq!(s.count_nodes(NodeKind::Symbol).unwrap(), 2);
    // Same bytes, other language: re-index (different fingerprint), language refreshed.
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn g() {}\nfn h() {}\n", Some("Zig"))
        .unwrap();
    assert!(st.replaced && !st.unchanged);
    assert_eq!(st.language, "zig");
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert_eq!(s.count_nodes(NodeKind::Symbol).unwrap(), 0);
    // Language is compared case-insensitively.
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn g() {}\nfn h() {}\n", Some("ZIG"))
        .unwrap();
    assert!(st.unchanged);
}

struct Versioned(&'static str);
impl Extractor for Versioned {
    fn language(&self) -> &str {
        "vx"
    }
    fn version(&self) -> String {
        self.0.to_string()
    }
    fn extract(&self, source: &str) -> Extraction {
        Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: tokenize(source),
        }
    }
}

#[test]
fn extractor_version_change_reindexes() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("g.redb");
    let put = |v: &'static str| {
        let mut s = RedbStore::open(&path).unwrap();
        s.register(Box::new(Versioned(v)));
        s.index_bytes("o", "r", "a.vx", b"a b c\n", Some("vx"))
            .unwrap()
    };
    assert!(!put("1").unchanged);
    assert!(put("1").unchanged);
    let st = put("2");
    assert!(st.replaced && !st.unchanged);
    assert!(put("2").unchanged);
    let s = RedbStore::open(&path).unwrap();
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert_eq!(s.count_nodes(NodeKind::Token).unwrap(), 3);
}

#[test]
fn fallback_and_rust_versions_are_distinct() {
    let mut r = Registry::default();
    r.register(Box::new(Versioned("v")));
    assert_eq!(r.version("vx"), "v");
    assert!(r
        .version("zig")
        .starts_with(graph_core::FALLBACK_EXTRACTOR_VERSION));
    assert_ne!(graph_lang_rust::RustExtractor.version(), r.version("zig"));
    assert_ne!(graph_lang_rust::RustExtractor.version(), "1");
}

#[test]
fn file_without_fingerprint_reindexes_once() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    // ingest_file has no content, so it stores no fingerprint (like an old DB).
    let ex = Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize("fn f() {}\n"),
    };
    s.ingest_file("o", "r", "a.rs", "rust", &ex).unwrap();
    assert_eq!(file_fingerprint(&s, "a.rs"), None);
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    assert!(st.replaced && !st.unchanged);
    assert!(file_fingerprint(&s, "a.rs").is_some());
    assert!(
        s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
            .unwrap()
            .unchanged
    );
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
}

#[test]
fn nodes_without_fingerprint_field_still_deserialize() {
    let old = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"origin":"directory","span":null}"#;
    assert_eq!(dec(old).unwrap().fingerprint, None);
}

#[test]
fn reindex_option_reindexes_unchanged_files() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    let st = s
        .index_bytes_opts(
            "o",
            "r",
            "a.rs",
            b"fn f() {}\n",
            None,
            None,
            IndexOptions { reindex: true },
        )
        .unwrap();
    assert!(st.replaced && !st.unchanged);
    assert_eq!(st.symbols, 1);
    assert!(
        s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
            .unwrap()
            .unchanged
    );
}

#[test]
fn batch_skips_unchanged_and_reindexes_changed() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let f = |p, b: &'static [u8], origin| BatchFile {
        path: p,
        bytes: b,
        language: None,
        origin,
    };
    let v1 = [
        f("a.rs", b"fn a() {}\n", None),
        f("b.rs", b"fn b() {}\n", None),
    ];
    assert!(s
        .index_batch("o", "r", &v1, IndexOptions::default())
        .unwrap()
        .iter()
        .all(|r| !r.as_ref().unwrap().unchanged));
    let ids = file_token_ids(&s, "a.rs");
    let v2 = [
        f("./a.rs", b"fn a() {}\n", Some(ORIGIN_DIRECTORY)),
        f("b.rs", b"fn b2() {}\n", None),
    ];
    let out = s
        .index_batch("o", "r", &v2, IndexOptions::default())
        .unwrap();
    let (a, b) = (out[0].as_ref().unwrap(), out[1].as_ref().unwrap());
    assert!(a.unchanged && a.path == "a.rs");
    assert!(b.replaced && !b.unchanged);
    assert_eq!(file_token_ids(&s, "a.rs"), ids);
    assert_eq!(
        origin_of(&s, "o", "r", "a.rs").as_deref(),
        Some(ORIGIN_DIRECTORY)
    );
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    let out = s
        .index_batch("o", "r", &v2, IndexOptions { reindex: true })
        .unwrap();
    assert!(out.iter().all(|r| !r.as_ref().unwrap().unchanged));
}

#[test]
fn skipped_files_are_still_seen_by_prune() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    let f = |p, b: &'static [u8]| BatchFile {
        path: p,
        bytes: b,
        language: None,
        origin: Some(ORIGIN_DIRECTORY),
    };
    let files = [f("a.rs", b"fn a() {}\n"), f("b.rs", b"fn b() {}\n")];
    s.index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    let out = s
        .index_batch("o", "r", &files[..1], IndexOptions::default())
        .unwrap();
    assert!(out[0].as_ref().unwrap().unchanged);
    let keep: std::collections::HashSet<String> = ["a.rs".to_string()].into();
    let removed = s.prune_files("o", "r", &keep, false).unwrap();
    assert_eq!(removed, vec!["b.rs".to_string()]);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
}

#[test]
fn extractor_and_tokenizer_versions_participate_in_fingerprint() {
    let d = tempfile::tempdir().unwrap();
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    let rust = RedbStore::open(d.path().join("h.redb")).unwrap();
    let mut rust = rust;
    rust.register(Box::new(graph_lang_rust::RustExtractor));
    let fp_fallback = s.fingerprint(b"fn f() {}\n", "rust");
    let fp_rust = rust.fingerprint(b"fn f() {}\n", "rust");
    assert_ne!(fp_fallback, fp_rust);
    assert!(fp_rust.contains(&graph_lang_rust::RustExtractor.version()));
    assert!(fp_fallback.contains(graph_core::FALLBACK_EXTRACTOR_VERSION));
    let tok = format!("tok{}", graph_core::tokenizer::TOKENIZER_VERSION);
    assert!(fp_rust.contains(&tok) && fp_fallback.contains(&tok));
}

#[test]
fn unregistered_extractor_downgrades_so_callers_must_register() {
    // Documents the risk on `Store::register`: without the Rust extractor the
    // fingerprint differs, so the file is re-indexed token-only.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("g.redb");
    let mut s = RedbStore::open(&path).unwrap();
    s.register(Box::new(graph_lang_rust::RustExtractor));
    s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    drop(s);
    let bare = RedbStore::open(&path).unwrap();
    let st = bare
        .index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    assert!(!st.unchanged && st.symbols == 0);
}

#[test]
fn unchanged_file_with_null_language_reports_fingerprint_language() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    // Null the stored language but keep the fingerprint.
    {
        let wt = s.db.begin_write().unwrap();
        {
            let names = wt.open_table(NAMES).unwrap();
            let o = names
                .get(name_key(None, NodeKind::Org, "o").as_str())
                .unwrap()
                .unwrap()
                .value();
            let r = names
                .get(name_key(Some(o), NodeKind::Repo, "r").as_str())
                .unwrap()
                .unwrap()
                .value();
            let f = names
                .get(name_key(Some(r), NodeKind::File, "a.rs").as_str())
                .unwrap()
                .unwrap()
                .value();
            let mut nodes = wt.open_table(NODES).unwrap();
            let mut n = dec(nodes.get(f).unwrap().unwrap().value()).unwrap();
            n.language = None;
            nodes.insert(f, enc(&n).as_slice()).unwrap();
        }
        wt.commit().unwrap();
    }
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    assert!(st.unchanged);
    assert_eq!(st.language, "rust");
}

#[test]
fn language_override_on_indexed_file_changes_fingerprint() {
    let d = tempfile::tempdir().unwrap();
    let s = rust_store(d.path());
    s.index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    let before = file_fingerprint(&s, "a.rs");
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn f() {}\n", Some("python"))
        .unwrap();
    assert!(st.replaced && !st.unchanged && st.language == "python");
    assert_ne!(file_fingerprint(&s, "a.rs"), before);
    // Back to the detected language: re-indexed again, symbols return.
    let st = s
        .index_bytes("o", "r", "a.rs", b"fn f() {}\n", None)
        .unwrap();
    assert!(st.replaced && !st.unchanged && st.symbols == 1);
    assert_eq!(file_fingerprint(&s, "a.rs"), before);
}

// ---- describe catalog (ADR 0003 story 0) ----

fn assert_catalog_matches_scan(s: &RedbStore, ctx: &str) {
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap(),
        "{ctx}"
    );
    assert_eq!(
        s.describe(Some("o1"), Some("r1")).unwrap(),
        s.describe_by_scan(Some("o1"), Some("r1")).unwrap(),
        "{ctx} (scoped)"
    );
}

const SRCS: [&str; 5] = [
    "fn a() { x(); }\nfn b() { y(1); }\n",
    "struct S;\nimpl S { fn m(&self) { q(); } }\n",
    "plain text here\n",
    "# comment\nx = 1\n",
    "",
];

#[test]
fn catalog_equals_scan_after_scripted_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = RedbStore::open(dir.path().join("g.redb")).unwrap();
    s.register(Box::new(graph_lang_rust::RustExtractor));
    s.register(Box::new(CountingExtractor(Default::default())));
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let langs = [None, Some("rust"), Some("python"), Some("zig")];
    for step in 0..300 {
        let org = ["o1", "o2"][rnd(2) as usize];
        let repo = ["r1", "r2"][rnd(2) as usize];
        let path = format!("f{}.txt", rnd(6));
        let src = SRCS[rnd(SRCS.len() as u64) as usize];
        let lang = langs[rnd(4) as usize];
        let reindex = IndexOptions {
            reindex: rnd(4) == 0,
        };
        match rnd(6) {
            0 | 1 => {
                s.index_bytes_opts(
                    org,
                    repo,
                    &path,
                    src.as_bytes(),
                    lang,
                    Some("directory"),
                    reindex,
                )
                .unwrap();
            }
            2 => {
                let files = [
                    BatchFile {
                        path: &path,
                        bytes: src.as_bytes(),
                        language: lang,
                        origin: Some("directory"),
                    },
                    BatchFile {
                        path: "bad.bin",
                        bytes: &[0xff, 0xfe],
                        language: None,
                        origin: None,
                    },
                    BatchFile {
                        path: "fail.count",
                        bytes: b"bad-span",
                        language: Some("count"),
                        origin: Some("directory"),
                    },
                    BatchFile {
                        path: "b2.rs",
                        bytes: SRCS[0].as_bytes(),
                        language: Some("rust"),
                        origin: Some("directory"),
                    },
                ];
                let out = s.index_batch(org, repo, &files, reindex).unwrap();
                assert!(matches!(out[2], Err(StoreError::InvalidSpan(_))));
                assert!(out[3].is_ok());
            }
            3 => {
                let keep: std::collections::HashSet<String> = (0..6)
                    .filter(|_| rnd(2) == 0)
                    .map(|i| format!("f{i}.txt"))
                    .collect();
                s.prune_files(org, repo, &keep, rnd(3) == 0).unwrap();
            }
            _ => {
                s.index_bytes(org, repo, &path, src.as_bytes(), lang)
                    .unwrap();
            }
        }
        assert_catalog_matches_scan(&s, &format!("step {step}"));
    }
}

#[test]
fn catalog_unchanged_by_aborted_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    let before = s.describe(None, None).unwrap();
    // A span error aborts the batch after earlier files were stored in it.
    let bad = Extraction {
        has_errors: false,
        symbols: vec![sym("x", SymbolKind::Function, span_of(RUST, "fn a()", 0))],
        tokens: vec![],
    };
    let mut bad_span = bad.clone();
    bad_span.symbols[0].span.start = 5;
    bad_span.symbols[0].span.end = 2; // start > end, explicitly
    assert!(s
        .ingest_file("o9", "r9", "new.rs", "rust", &bad_span)
        .is_err());
    assert!(s
        .ingest_file("o1", "r1", "lib.rs", "rust", &bad_span)
        .is_err());
    assert_eq!(s.describe(None, None).unwrap(), before);
    assert_catalog_matches_scan(&s, "after aborted ingest");
    // A dry-run prune aborts its transaction.
    s.prune_files("o1", "r1", &Default::default(), true)
        .unwrap();
    assert_eq!(s.describe(None, None).unwrap(), before);
}

#[test]
fn old_db_without_catalog_backfills_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.redb");
    let expected = {
        let s = setup(dir.path());
        let e = s.describe_by_scan(None, None).unwrap();
        assert!(!e.is_empty());
        // Simulate a pre-catalog database.
        let wt = s.db.begin_write().unwrap();
        wt.delete_table(CATALOG).unwrap();
        wt.open_table(META)
            .unwrap()
            .remove("catalog_version")
            .unwrap();
        wt.commit().unwrap();
        e
    };
    let s = RedbStore::open(&path).unwrap();
    assert_eq!(s.describe(None, None).unwrap(), expected);
    let ver = |s: &RedbStore| {
        s.db.begin_read()
            .unwrap()
            .open_table(META)
            .unwrap()
            .get("catalog_version")
            .unwrap()
            .map(|v| v.value())
    };
    assert_eq!(ver(&s), Some(CATALOG_VERSION));
    // Current catalog: a second open must not rebuild. Poison a row to prove it.
    {
        let wt = s.db.begin_write().unwrap();
        wt.open_table(CATALOG)
            .unwrap()
            .insert("r\0zz\0zz", 0)
            .unwrap();
        wt.commit().unwrap();
    }
    drop(s);
    let s = RedbStore::open(&path).unwrap();
    assert!(
        s.describe(Some("zz"), None).unwrap().len() == 1,
        "rebuilt again"
    );
}

#[test]
fn describe_does_not_decode_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    // Corrupt a node: a full scan trips over it, the catalog never reads it.
    let wt = s.db.begin_write().unwrap();
    wt.open_table(NODES)
        .unwrap()
        .insert(u64::MAX, b"not json".as_slice())
        .unwrap();
    wt.commit().unwrap();
    assert!(s.describe_by_scan(None, None).is_err());
    assert!(s.describe(None, None).is_ok());
    assert!(s.describe(Some("o1"), Some("r1")).is_ok());
}

#[test]
fn nul_bytes_rejected_in_org_repo_language_and_lang_kind() {
    let d = tempfile::tempdir().unwrap();
    let s = RedbStore::open(d.path().join("g.redb")).unwrap();
    let ok = Extraction::default();
    for (o, r, l) in [("a\0b", "r", "x"), ("o", "a\0b", "x"), ("o", "r", "a\0b")] {
        let e = s.ingest_file(o, r, "f", l, &ok).unwrap_err();
        assert!(
            matches!(&e, StoreError::Rejected(m) if m.contains("NUL")),
            "{e}"
        );
    }
    let mut ex = Extraction::default();
    let mut d1 = sym("x", SymbolKind::Function, span_of("abcd", "ab", 0));
    d1.lang_kind = Some("k\0k".into());
    ex.symbols.push(d1);
    let e = s.ingest_file("o", "r", "f", "x", &ex).unwrap_err();
    assert!(
        matches!(&e, StoreError::Rejected(m) if m.contains("NUL")),
        "{e}"
    );
    assert!(s.describe(None, None).unwrap().is_empty());
}

#[test]
fn hard_failure_mid_batch_leaves_catalog_equal_to_scan() {
    let d = tempfile::tempdir().unwrap();
    let mut s = setup(d.path());
    let before = s.describe(None, None).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    s.register(Box::new(CountingExtractor(calls)));
    let f = |p, l| BatchFile {
        path: p,
        bytes: b"xxxx",
        language: Some(l),
        origin: None,
    };
    let files = [f("ok1.c", "count"), f("nul.c", "a\0b")];
    assert!(s
        .index_batch("o1", "newrepo", &files, IndexOptions::default())
        .is_err());
    assert_eq!(s.describe(None, None).unwrap(), before);
    assert_catalog_matches_scan(&s, "after failed batch");
}

#[test]
fn v1_database_upgrades_in_place_and_stamps_schema() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("g.redb");
    let expected = {
        let s = setup(d.path());
        let e = s.describe_by_scan(None, None).unwrap();
        set_meta(&s, "schema_version", Some(1));
        set_meta(&s, "catalog_version", None);
        e
    };
    let s = RedbStore::open(&path).unwrap();
    assert_eq!(meta(&s, "schema_version"), Some(SCHEMA_VERSION));
    assert_eq!(s.describe(None, None).unwrap(), expected);
}

#[test]
fn missing_catalog_table_is_rebuilt_and_partial_rows_are_reported() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("g.redb");
    let expected = {
        let s = setup(d.path());
        let wt = s.db.begin_write().unwrap();
        wt.delete_table(CATALOG).unwrap();
        wt.commit().unwrap();
        s.describe_by_scan(None, None).unwrap()
    };
    let s = RedbStore::open(&path).unwrap();
    assert_eq!(s.describe(None, None).unwrap(), expected);
    // Drop one repo marker row: describe says so instead of returning a wrong answer.
    {
        let wt = s.db.begin_write().unwrap();
        wt.open_table(CATALOG).unwrap().remove("r\0o1\0r1").unwrap();
        wt.commit().unwrap();
    }
    let e = s.describe(None, None).unwrap_err();
    assert!(matches!(e, StoreError::Corrupt(_)), "{e}");
}

#[test]
fn redb_passes_conformance_suite() {
    conformance::run_all(&|| {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("g.redb");
        conformance::Harness {
            open: Box::new(move |ex| open_store(Backend::Redb, &path, ex)),
            exclusive: true,
            guard: Some(Box::new(d)),
        }
    });
}

#[test]
fn v2_passes_conformance_suite() {
    conformance::run_all(&|| {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("g2.redb");
        conformance::Harness {
            open: Box::new(move |ex| open_store(Backend::RedbV2, &path, ex)),
            exclusive: true,
            guard: Some(Box::new(d)),
        }
    });
}

/// The point of the differential harness: v1 is the oracle for v2.
#[test]
fn v1_vs_v2_differential() {
    let d = tempfile::tempdir().unwrap();
    let a = open_store(Backend::Redb, &d.path().join("a.redb"), vec![]).unwrap();
    let b = open_store(Backend::RedbV2, &d.path().join("b.redb"), vec![]).unwrap();
    conformance::run_differential(&*a, &*b);
}

#[test]
fn v1_and_v2_files_refuse_each_other_untouched() {
    let d = tempfile::tempdir().unwrap();
    let (p1, p2) = (d.path().join("a.redb"), d.path().join("b.redb"));
    open_store(Backend::Redb, &p1, vec![])
        .unwrap()
        .index_bytes("o", "r", "a.txt", b"foo", None)
        .unwrap();
    open_store(Backend::RedbV2, &p2, vec![])
        .unwrap()
        .index_bytes("o", "r", "a.txt", b"foo", None)
        .unwrap();
    let before = std::fs::read(&p1).unwrap();
    assert!(matches!(
        open_store(Backend::RedbV2, &p1, vec![]),
        Err(StoreError::Rejected(_))
    ));
    assert!(matches!(
        open_store(Backend::Redb, &p2, vec![]),
        Err(StoreError::SchemaMismatch { found: 3 })
    ));
    assert_eq!(std::fs::read(&p1).unwrap(), before, "v1 file untouched");
    // Reopening v2 keeps its data.
    let s = open_store(Backend::RedbV2, &p2, vec![]).unwrap();
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 1);
}

#[test]
fn redb_differential_against_itself() {
    let mk = || {
        let d = tempfile::tempdir().unwrap();
        let s = open_store(Backend::Redb, &d.path().join("g.redb"), vec![]).unwrap();
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
    assert_send_sync::<RedbStore>();
    fn assert_send<T: Send + ?Sized>() {}
    assert_send::<dyn StoreRead + Send>();
}
