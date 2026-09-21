//! Reusable conformance suite for [`Store`] implementations: the store-level
//! behaviours every backend must share. It is the seed of the ADR 0003
//! differential oracle (the same cases run against the redb backend, a future
//! v2 store and a `RemoteStore`; the full fixed-query differential harness is
//! ADR story 1's later half).
//!
//! Use: build a [`Harness`] per case with a factory, then call [`run_all`]:
//!
//! ```ignore
//! graph_store::conformance::run_all(&|| Harness {
//!     open: Box::new(move |ex| open_store(Backend::Redb, &path, ex)),
//!     exclusive: true,
//!     guard: Some(Box::new(tempdir)),
//! });
//! ```
//! The factory is called once per case, so each case sees an empty database.
use crate::{
    BatchFile, Grain, IndexOptions, Query, Store, StoreError, SymbolQuery, ORIGIN_DIRECTORY,
};
use graph_core::tokenizer::tokenize;
use graph_core::{Extraction, Extractor, NodeKind, Span, SymbolDecl, SymbolKind};
use std::collections::HashSet;

type Opened = Result<Box<dyn Store>, StoreError>;
type OpenFn = dyn Fn(Vec<Box<dyn Extractor>>) -> Opened;
type Case = fn(&Harness);

/// How to open the store under test.
pub struct Harness {
    /// Open the store over the harness's (initially empty) data with the given
    /// extractors registered. Every call opens the same underlying data.
    pub open: Box<OpenFn>,
    /// The backend allows one open handle at a time: a second concurrent
    /// `open` must fail with `StoreError::Locked`. Set false for a client
    /// backend (e.g. `RemoteStore`) where many handles are normal.
    pub exclusive: bool,
    /// Kept alive for the case's duration (e.g. a temp dir).
    pub guard: Option<Box<dyn std::any::Any>>,
}

/// Every case, by name.
pub const CASES: &[(&str, Case)] = &[
    ("ingest_replace_and_grains", ingest_replace_and_grains),
    ("filters_and_limit", filters_and_limit),
    ("symbol_search", symbol_search),
    ("unchanged_skip_and_reindex", unchanged_skip_and_reindex),
    ("prune", prune),
    ("describe_matches_scan", describe_matches_scan),
    ("invalid_span_slots", invalid_span_slots),
    ("reopen_persists", reopen_persists),
    ("snapshot_is_frozen", snapshot_is_frozen),
    ("locking", locking),
];

/// Run every case; `make` builds a fresh harness (empty data) per case.
pub fn run_all(make: &dyn Fn() -> Harness) {
    for (name, case) in CASES {
        eprintln!("conformance: {name}");
        let h = make();
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&h))) {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!("conformance case `{name}` failed: {msg}");
        }
    }
}

const RUST: &str = "impl S {\n    fn a() { foo(); foo(); }\n    fn b() { foo(); }\n}\n";
const ZIG: &str = "pub fn main() void { foo(); }\n";

fn span_of(src: &str, needle: &str) -> Span {
    let start = src.find(needle).unwrap();
    let end = start + needle.len();
    let pos = |o: usize| {
        let b = &src[..o];
        (
            1 + b.matches('\n').count() as u32,
            1 + b.rsplit('\n').next().unwrap().chars().count() as u32,
        )
    };
    let ((sl, sc), (el, ec)) = (pos(start), pos(end));
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

fn rust_extraction(src: &str) -> Extraction {
    Extraction {
        has_errors: false,
        symbols: vec![
            sym("S", SymbolKind::Type, span_of(src, src.trim_end())),
            sym(
                "a",
                SymbolKind::Method,
                span_of(src, "fn a() { foo(); foo(); }"),
            ),
            sym("b", SymbolKind::Method, span_of(src, "fn b() { foo(); }")),
        ],
        tokens: tokenize(src),
    }
}

fn plain(src: &str) -> Extraction {
    Extraction {
        has_errors: false,
        symbols: vec![],
        tokens: tokenize(src),
    }
}

fn open(h: &Harness) -> Box<dyn Store> {
    (h.open)(vec![]).expect("open store")
}

/// Two orgs: `o1/r1/lib.rs` (rust, symbols) and `o2/r2/main.zig` (no symbols).
fn seed(s: &dyn Store) {
    s.ingest_file("o1", "r1", "lib.rs", "rust", &rust_extraction(RUST))
        .unwrap();
    s.ingest_file("o2", "r2", "main.zig", "zig", &plain(ZIG))
        .unwrap();
}

fn counts(hits: &[crate::Hit]) -> Vec<usize> {
    hits.iter().map(|h| h.count).collect()
}

fn ingest_replace_and_grains(h: &Harness) {
    let s = open(h);
    seed(&*s);
    let mut q = Query::new("foo");
    assert_eq!(s.search(&q).unwrap().len(), 4, "token grain");
    let hits = s.search(&q).unwrap();
    assert_eq!(hits[0].symbol.as_deref(), Some("S::a"));
    let sp = hits[0].span.unwrap();
    assert_eq!(&RUST[sp.start as usize..sp.end as usize], "foo");

    q.grain = Grain::Symbol;
    q.symbol_kind = Some("method".into());
    let got: Vec<_> = s
        .search(&q)
        .unwrap()
        .iter()
        .map(|x| (x.symbol.clone(), x.count, x.no_symbols))
        .collect();
    assert_eq!(
        got,
        [
            (Some("S::a".into()), 2, false),
            (Some("S::b".into()), 1, false),
            (None, 1, true)
        ]
    );
    q.symbol_kind = None;
    q.grain = Grain::File;
    assert_eq!(counts(&s.search(&q).unwrap()), [3, 1]);
    q.grain = Grain::Repo;
    assert_eq!(counts(&s.search(&q).unwrap()), [3, 1]);
    q.grain = Grain::Org;
    assert_eq!(counts(&s.search(&q).unwrap()), [3, 1]);

    // Replace: same file, new content; no duplicates, old tokens gone.
    let before = (
        s.count_nodes(NodeKind::File).unwrap(),
        s.count_nodes(NodeKind::Symbol).unwrap(),
    );
    let st = s
        .ingest_file("o2", "r2", "main.zig", "zig", &plain("bar baz\n"))
        .unwrap();
    assert!(st.replaced);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), before.0);
    assert_eq!(s.count_nodes(NodeKind::Symbol).unwrap(), before.1);
    q.grain = Grain::Token;
    assert_eq!(s.search(&q).unwrap().len(), 3);
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 1);
    let toks = s.file_tokens("o2", "r2", "main.zig").unwrap().unwrap();
    assert_eq!(toks.len(), 2);
    assert!(s.file_tokens("o2", "r2", "nope.zig").unwrap().is_none());
    // get/parent: a token's parent chain is reachable.
    let t = &toks[0];
    let p = s.parent(t.id).unwrap().expect("token has a parent");
    assert_eq!(p.kind, NodeKind::File);
    assert!(s.get(t.id).unwrap().is_some());
}

fn filters_and_limit(h: &Harness) {
    let s = open(h);
    seed(&*s);
    let mut q = Query::new("foo");
    q.language = Some("rust".into());
    assert_eq!(s.search(&q).unwrap().len(), 3);
    q.language = Some("zig".into());
    q.org = Some("o2".into());
    assert_eq!(s.search(&q).unwrap().len(), 1);
    q.org = Some("o1".into());
    assert!(s.search(&q).unwrap().is_empty());
    let mut q = Query::new("foo");
    q.repo = Some("r1".into());
    assert_eq!(s.search(&q).unwrap().len(), 3);
    q.limit = Some(2);
    let l = s.search(&q).unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(
        l[0].symbol.as_deref(),
        Some("S::a"),
        "limit keeps the first rows"
    );
    let mut q = Query::new("foo");
    q.class = Some(graph_core::TokenClass::Keyword);
    assert!(s.search(&q).unwrap().is_empty());
    q.class = Some(graph_core::TokenClass::Identifier);
    assert_eq!(s.search(&q).unwrap().len(), 4);
}

fn symbol_search(h: &Harness) {
    let s = open(h);
    seed(&*s);
    let names = |q: &SymbolQuery| -> Vec<String> {
        s.search_symbols(q)
            .unwrap()
            .into_iter()
            .map(|x| x.qualified)
            .collect()
    };
    assert_eq!(names(&SymbolQuery::new("a")), ["S::a"]);
    assert_eq!(names(&SymbolQuery::new("*")), ["S", "S::a", "S::b"]);
    let mut q = SymbolQuery::new("*");
    q.kind = Some("method".into());
    assert_eq!(names(&q), ["S::a", "S::b"]);
    q.limit = Some(1);
    assert_eq!(names(&q), ["S::a"]);
    let mut q = SymbolQuery::new("*");
    q.file = Some("lib.rs".into());
    q.org = Some("o1".into());
    assert_eq!(names(&q).len(), 3);
    assert!(
        s.search_symbols(&SymbolQuery::new("")).is_err(),
        "empty pattern is ambiguous"
    );
    assert!(s
        .search_symbols(&SymbolQuery::new("nope"))
        .unwrap()
        .is_empty());
}

fn unchanged_skip_and_reindex(h: &Harness) {
    let s = open(h);
    let first = s.index_bytes("o", "r", "a.txt", b"foo bar", None).unwrap();
    assert!(!first.unchanged && !first.replaced && first.tokens > 0);
    let again = s.index_bytes("o", "r", "a.txt", b"foo bar", None).unwrap();
    assert!(again.unchanged && !again.replaced);
    assert_eq!((again.symbols, again.tokens), (0, 0));
    assert_eq!(again.file_id, first.file_id, "skipped file keeps its node");
    let forced = s
        .index_bytes_opts(
            "o",
            "r",
            "a.txt",
            b"foo bar",
            None,
            None,
            IndexOptions { reindex: true },
        )
        .unwrap();
    assert!(!forced.unchanged && forced.tokens == first.tokens);
    let changed = s.index_bytes("o", "r", "a.txt", b"foo baz", None).unwrap();
    assert!(!changed.unchanged && changed.replaced);
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 0);
    assert_eq!(s.search(&Query::new("baz")).unwrap().len(), 1);
    // Rejections store nothing.
    assert!(matches!(
        s.index_bytes("o", "r", "bin", &[0xff, 0xfe], None),
        Err(StoreError::NotUtf8(_))
    ));
    assert!(s.file_tokens("o", "r", "bin").unwrap().is_none());
}

fn prune(h: &Harness) {
    let s = open(h);
    for p in ["keep.txt", "drop.txt"] {
        s.index_bytes_with_origin("o", "r", p, b"foo", None, Some(ORIGIN_DIRECTORY))
            .unwrap();
    }
    // An agent-supplied file (no directory origin) is never pruned.
    s.index_bytes("o", "r", "manual.txt", b"foo", None).unwrap();
    s.index_bytes_with_origin("o", "other", "x.txt", b"foo", None, Some(ORIGIN_DIRECTORY))
        .unwrap();
    let keep: HashSet<String> = ["keep.txt".to_string()].into();
    let dry = s.prune_files("o", "r", &keep, true).unwrap();
    assert_eq!(dry, ["drop.txt"]);
    assert!(
        s.file_tokens("o", "r", "drop.txt").unwrap().is_some(),
        "dry run changes nothing"
    );
    let done = s.prune_files("o", "r", &keep, false).unwrap();
    assert_eq!(done, ["drop.txt"]);
    assert!(s.file_tokens("o", "r", "drop.txt").unwrap().is_none());
    assert!(s.file_tokens("o", "r", "keep.txt").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "manual.txt").unwrap().is_some());
    assert!(s.file_tokens("o", "other", "x.txt").unwrap().is_some());
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 3);
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

fn describe_matches_scan(h: &Harness) {
    let s = open(h);
    let empty = s.describe(None, None).unwrap();
    assert!(empty.is_empty());
    seed(&*s);
    s.index_bytes("o1", "r1", "notes.md", b"# foo\nbar\n", None)
        .unwrap();
    for (o, r) in [
        (None, None),
        (Some("o1"), None),
        (Some("o1"), Some("r1")),
        (Some("o2"), Some("r2")),
        (Some("zz"), None),
    ] {
        assert_eq!(
            s.describe(o, r).unwrap(),
            s.describe_by_scan(o, r).unwrap(),
            "describe vs scan for {o:?}/{r:?}"
        );
    }
    let all = s.describe(None, None).unwrap();
    assert_eq!(all.len(), 2);
    let r1 = &all[0];
    assert_eq!(
        (r1.org.as_str(), r1.repo.as_str(), r1.files),
        ("o1", "r1", 2)
    );
    let rust = &r1.languages["rust"];
    assert_eq!((rust.files, rust.symbols), (1, 3));
    assert!(r1.kind_names(Some("rust")).contains("method"));
    // Still equal after a replace that changes the symbol set.
    s.ingest_file("o1", "r1", "lib.rs", "rust", &plain("zzz\n"))
        .unwrap();
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

/// Fails span validation on demand: language `conf-bad` yields a symbol whose
/// start is after its end; `conf-ok` yields one token.
struct BadSpans;
impl Extractor for BadSpans {
    fn language(&self) -> &str {
        "conf-bad"
    }
    fn extract(&self, src: &str) -> Extraction {
        let s = span_of(src, src);
        let mut bad = s;
        bad.start = s.end + 1;
        Extraction {
            symbols: vec![sym("x", SymbolKind::Function, bad)],
            tokens: vec![],
            has_errors: false,
        }
    }
}

fn invalid_span_slots(h: &Harness) {
    let s = (h.open)(vec![Box::new(BadSpans)]).expect("open store");
    s.index_bytes("o", "r", "old.c", b"old text", Some("text"))
        .unwrap();
    let f = |p, b: &'static [u8], l| BatchFile {
        path: p,
        bytes: b,
        language: Some(l),
        origin: None,
    };
    let files = [
        f("ok1.c", b"xxxx", "text"),
        f("bad.c", b"bad", "conf-bad"),
        f("old.c", b"bad again", "conf-bad"),
        f("ok2.c", b"yyyy", "text"),
    ];
    let out = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert_eq!(out.len(), 4, "one outcome per input, in order");
    assert!(out[0].is_ok() && out[3].is_ok());
    for i in [1, 2] {
        match &out[i] {
            Err(StoreError::InvalidSpan(m)) => {
                assert!(m.contains(files[i].path), "message names the path: {m}")
            }
            other => panic!("slot {i}: expected InvalidSpan, got {other:?}"),
        }
    }
    assert!(s.file_tokens("o", "r", "ok1.c").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "ok2.c").unwrap().is_some());
    assert!(
        s.file_tokens("o", "r", "bad.c").unwrap().is_none(),
        "failed file not stored"
    );
    let old = s.file_tokens("o", "r", "old.c").unwrap().unwrap();
    assert_eq!(old.len(), 2, "failing re-index left the old version intact");
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
    // The single-file path hard-fails instead.
    assert!(matches!(
        s.index_bytes("o", "r", "solo.c", b"bad", Some("conf-bad")),
        Err(StoreError::InvalidSpan(_))
    ));
    assert!(s.file_tokens("o", "r", "solo.c").unwrap().is_none());
}

fn reopen_persists(h: &Harness) {
    let s = open(h);
    seed(&*s);
    drop(s);
    let s = open(h);
    assert_eq!(s.search(&Query::new("foo")).unwrap().len(), 4);
    assert_eq!(s.describe(None, None).unwrap().len(), 2);
}

fn snapshot_is_frozen(h: &Harness) {
    let s = open(h);
    seed(&*s);
    let old_toks = s.file_tokens("o2", "r2", "main.zig").unwrap().unwrap();
    let old_id = old_toks[0].id;
    let syms_before = s.search_symbols(&SymbolQuery::new("*")).unwrap();
    let snap = s.snapshot().unwrap();
    s.index_bytes("o3", "r3", "new.txt", b"foo", None).unwrap();
    s.ingest_file("o2", "r2", "main.zig", "zig", &plain("gone\n"))
        .unwrap();
    s.ingest_file("o1", "r1", "lib.rs", "rust", &plain("zzz\n"))
        .unwrap();
    // Through the snapshot: exactly the state before the writes.
    assert!(snap.search(&Query::new("gone")).unwrap().is_empty());
    let mut zig = Query::new("foo");
    zig.language = Some("zig".into());
    assert_eq!(snap.search(&zig).unwrap().len(), 1, "zig foo still present");
    assert_eq!(snap.search(&Query::new("foo")).unwrap().len(), 4);
    let toks = snap.file_tokens("o2", "r2", "main.zig").unwrap().unwrap();
    assert_eq!(toks, old_toks, "file_tokens returns the old tokens");
    assert_eq!(snap.count_nodes(NodeKind::File).unwrap(), 2);
    assert_eq!(
        snap.search_symbols(&SymbolQuery::new("*")).unwrap(),
        syms_before
    );
    assert_eq!(snap.get(old_id).unwrap().unwrap(), old_toks[0]);
    assert_eq!(snap.parent(old_id).unwrap().unwrap().kind, NodeKind::File);
    assert!(snap.file_tokens("o3", "r3", "new.txt").unwrap().is_none());
    let d = snap.describe(None, None).unwrap();
    assert_eq!(d.len(), 2);
    assert!(d.iter().all(|r| r.org != "o3"), "new org not visible");
    assert_eq!(d, snap.describe_by_scan(None, None).unwrap());
    // Live store sees the writes.
    assert_eq!(s.describe(None, None).unwrap().len(), 3);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    drop(snap);
    let fresh = s.snapshot().unwrap();
    assert_eq!(fresh.search(&Query::new("gone")).unwrap().len(), 1);
}

fn locking(h: &Harness) {
    if !h.exclusive {
        return;
    }
    let first = open(h);
    match (h.open)(vec![]) {
        Err(StoreError::Locked(_)) => {}
        Err(e) => panic!("expected Locked, got {e}"),
        Ok(_) => panic!("second concurrent open must fail with Locked"),
    }
    drop(first);
    assert!(
        (h.open)(vec![]).is_ok(),
        "open succeeds once the holder is gone"
    );
}
