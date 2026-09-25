//! Reusable conformance suite for [`Store`] implementations: the store-level
//! behaviours every implementation must share (the redb store in every
//! configuration today; a `RemoteStore` later, ADR 0003 Q5).
//!
//! [`run_differential`] is the fixed-query *configuration equivalence*
//! harness: it seeds two stores with the same corpus, runs one query set
//! against both and requires identical results, so query-visible behaviour
//! provably does not depend on chunk size, cache size, thread count, memory
//! budget, compaction or a reopen. [`run_crash_rerun_differential`] extends
//! it to a batch that crashed mid-way and was re-run.
//!
//! Use: build a [`Harness`] per case with a factory, then call [`run_all`]:
//!
//! ```ignore
//! graph_store::conformance::run_all(&|| Harness {
//!     open: Box::new(move |ex| open_store(&path, ex)),
//!     exclusive: true,
//!     guard: Some(Box::new(tempdir)),
//! });
//! ```
//! The factory is called once per case, so each case sees an empty database.
use crate::{
    BatchFile, Grain, IndexOptions, Query, Store, StoreError, SymbolQuery, ORIGIN_DIRECTORY,
};
use graph_core::tokenizer::tokenize;
use graph_core::{Extraction, Extractor, Node, NodeId, NodeKind, Span, SymbolDecl, SymbolKind};
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
    ("batch_reindex_and_unchanged", batch_reindex_and_unchanged),
    (
        "failed_batch_leaves_consistent_state",
        failed_batch_leaves_consistent_state,
    ),
    ("snapshot_filtered_reads", snapshot_filtered_reads),
    ("symbol_language_filter", symbol_language_filter),
    ("describe_repo_filter", describe_repo_filter),
    ("describe_unknown_vs_empty", describe_unknown_vs_empty),
    ("default_origin", default_origin),
    ("order_and_limit_determinism", order_and_limit_determinism),
    ("prune_empty_keep", prune_empty_keep),
    ("nul_handling", nul_handling),
    ("batch_origin_refresh", batch_origin_refresh),
    ("traversal", traversal),
    ("vacuum_preserves_reads", vacuum_preserves_reads),
    ("claimed_extension_extractor", claimed_extension_extractor),
    ("prepared_matches_batch", prepared_matches_batch),
    ("prepare_skips_unchanged", prepare_skips_unchanged),
    ("prepared_rejections_in_order", prepared_rejections_in_order),
    (
        "prepared_changed_since_prepare",
        prepared_changed_since_prepare,
    ),
    (
        "prepared_changed_file_replaces",
        prepared_changed_file_replaces,
    ),
    ("prepared_duplicate_paths", prepared_duplicate_paths),
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

fn bf<'a>(path: &'a str, bytes: &'a [u8]) -> BatchFile<'a> {
    BatchFile {
        path,
        bytes,
        language: Some("text"),
        origin: Some(ORIGIN_DIRECTORY),
    }
}

/// Whatever the chunking: after a batch that fails with a storage error,
/// whatever is stored is complete and consistent, and a re-run stores the
/// rest and skips what is stored.
fn failed_batch_leaves_consistent_state(h: &Harness) {
    let s = open(h);
    let bad = BatchFile {
        path: "nul.txt",
        bytes: b"foo bar",
        language: Some("a\0b"), // NUL in the language: a whole-batch error
        origin: Some(ORIGIN_DIRECTORY),
    };
    let names = ["a.txt", "b.txt", "c.txt"];
    let bytes: [&[u8]; 3] = [b"foo bar", b"foo baz", b"foo qux"];
    let mut files: Vec<BatchFile<'_>> = vec![bf(names[0], bytes[0]), bf(names[1], bytes[1])];
    files.push(bad);
    files.push(bf(names[2], bytes[2]));
    assert!(s
        .index_batch("o", "r", &files, IndexOptions::default())
        .is_err());
    // Stored files are whole and the catalog agrees with a full scan.
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
    let mut stored = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if let Some(toks) = s.file_tokens("o", "r", n).unwrap() {
            assert_eq!(toks.len(), 2, "{n} is complete, never partial");
            assert_eq!(toks[0].name, "foo");
            let _ = i;
            stored.push(*n);
        }
    }
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), stored.len());
    assert!(!stored.contains(&"c.txt"), "later files are never reached");
    // Re-run without the bad file: stored files are skipped, the rest stored.
    let rerun: Vec<BatchFile<'_>> = names.iter().zip(bytes).map(|(n, b)| bf(n, b)).collect();
    let out = s
        .index_batch("o", "r", &rerun, IndexOptions::default())
        .unwrap();
    for (n, r) in names.iter().zip(&out) {
        assert_eq!(
            r.as_ref().unwrap().unchanged,
            stored.contains(n),
            "{n}: skipped iff already stored"
        );
    }
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

fn batch_reindex_and_unchanged(h: &Harness) {
    let s = open(h);
    let files = [bf("a.txt", b"foo bar"), bf("b.txt", b"foo baz")];
    let ok = |r: &Result<crate::IngestStats, StoreError>| r.as_ref().unwrap().clone();
    let first = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert!(first.iter().all(|r| !ok(r).unchanged && !ok(r).replaced));
    let again = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    for (a, f) in again.iter().zip(&first) {
        let (a, f) = (ok(a), ok(f));
        assert!(a.unchanged && !a.replaced);
        assert_eq!((a.symbols, a.tokens), (0, 0));
        assert_eq!(a.file_id, f.file_id, "skipped file keeps its node");
    }
    let forced = s
        .index_batch("o", "r", &files, IndexOptions { reindex: true })
        .unwrap();
    assert!(forced.iter().all(|r| !ok(r).unchanged && ok(r).replaced));
    // Mixed: one changed, one unchanged.
    let mixed = [bf("a.txt", b"foo CHANGED"), bf("b.txt", b"foo baz")];
    let out = s
        .index_batch("o", "r", &mixed, IndexOptions::default())
        .unwrap();
    assert!(!ok(&out[0]).unchanged && ok(&out[0]).replaced);
    assert!(ok(&out[1]).unchanged);
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 0);
    assert_eq!(s.search(&Query::new("CHANGED")).unwrap().len(), 1);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    // An empty batch is a no-op.
    assert!(s
        .index_batch("o", "r", &[], IndexOptions::default())
        .unwrap()
        .is_empty());
}

/// Every filtered read, rendered as one string so two read views compare
/// exactly (a dropped filter changes the string).
fn filtered_probe<R: crate::StoreRead + ?Sized>(r: &R) -> String {
    let mut out = String::new();
    let mut add = |label: &str, v: String| out.push_str(&format!("{label}: {v}\n"));
    let mut qs = Vec::new();
    for grain in [Grain::Token, Grain::Symbol, Grain::File] {
        let mut q = Query::new("foo");
        q.grain = grain;
        qs.push(q.clone());
        q.org = Some("o1".into());
        qs.push(q.clone());
        q.repo = Some("r1".into());
        qs.push(q.clone());
        q.repo = Some("r2".into());
        qs.push(q.clone());
        q.org = None;
        q.repo = None;
        q.class = Some(graph_core::TokenClass::Identifier);
        qs.push(q.clone());
        q.class = Some(graph_core::TokenClass::Keyword);
        qs.push(q.clone());
        q.class = None;
        q.symbol_kind = Some("method".into());
        qs.push(q.clone());
        q.symbol_kind = Some("type".into());
        qs.push(q);
    }
    for q in &qs {
        add(&format!("{q:?}"), format!("{:?}", r.search(q).unwrap()));
    }
    let mut sqs = Vec::new();
    for lang in [None, Some("rust"), Some("zig")] {
        for file in [None, Some("lib.rs"), Some("main.zig")] {
            for limit in [None, Some(1)] {
                let mut q = SymbolQuery::new("*");
                q.language = lang.map(Into::into);
                q.file = file.map(Into::into);
                q.limit = limit;
                sqs.push(q);
            }
        }
    }
    for q in &sqs {
        add(
            &format!("{q:?}"),
            format!("{:?}", r.search_symbols(q).unwrap()),
        );
    }
    for (o, rp) in [
        (None, None),
        (Some("o1"), None),
        (Some("o1"), Some("r1")),
        (None, Some("r2")),
        (Some("o2"), Some("r1")),
    ] {
        let d = r.describe(o, rp).unwrap();
        assert_eq!(d, r.describe_by_scan(o, rp).unwrap(), "describe vs scan");
        add(&format!("describe {o:?}/{rp:?}"), format!("{d:?}"));
    }
    out
}

/// The reads not covered by `snapshot_is_frozen`: count_nodes for every kind,
/// filtered search and the symbol-kind filter, through a snapshot after
/// writes.
fn snapshot_filtered_reads(h: &Harness) {
    let s = open(h);
    seed(&*s);
    let snap = s.snapshot().unwrap();
    let before = filtered_probe(&*s);
    assert_eq!(
        filtered_probe(&*snap),
        before,
        "snapshot == live, no writes"
    );
    s.ingest_file("o1", "r1", "lib.rs", "rust", &plain("zzz\n"))
        .unwrap();
    s.index_bytes("o3", "r3", "new.txt", b"foo", None).unwrap();
    assert_eq!(
        filtered_probe(&*snap),
        before,
        "snapshot == pre-write state"
    );
    assert_ne!(filtered_probe(&*s), before, "live store moved on");
    assert_eq!(snap.count_nodes(NodeKind::Symbol).unwrap(), 3);
    assert_eq!(s.count_nodes(NodeKind::Symbol).unwrap(), 0);
    assert_eq!(snap.count_nodes(NodeKind::Org).unwrap(), 2);
    assert_eq!(s.count_nodes(NodeKind::Org).unwrap(), 3);
    assert_eq!(snap.count_nodes(NodeKind::Repo).unwrap(), 2);
    assert_eq!(s.count_nodes(NodeKind::Repo).unwrap(), 3);
    assert_eq!(snap.count_nodes(NodeKind::File).unwrap(), 2);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    let mut q = Query::new("foo");
    q.language = Some("rust".into());
    assert_eq!(snap.search(&q).unwrap().len(), 3);
    q.org = Some("o1".into());
    q.repo = Some("r1".into());
    q.limit = Some(1);
    assert_eq!(snap.search(&q).unwrap().len(), 1);
    q.grain = Grain::Symbol;
    q.symbol_kind = Some("method".into());
    q.limit = None;
    let got: Vec<_> = snap
        .search(&q)
        .unwrap()
        .into_iter()
        .map(|x| (x.symbol, x.count))
        .collect();
    assert_eq!(got, [(Some("S::a".into()), 2), (Some("S::b".into()), 1)]);
    let mut sq = SymbolQuery::new("*");
    sq.kind = Some("method".into());
    assert_eq!(snap.search_symbols(&sq).unwrap().len(), 2);
    assert!(s.search_symbols(&sq).unwrap().is_empty());
    sq.kind = Some("type".into());
    assert_eq!(snap.search_symbols(&sq).unwrap().len(), 1);
}

fn symbol_language_filter(h: &Harness) {
    let s = open(h);
    seed(&*s);
    // A same-named symbol in another language.
    let src = "fn a() {}\n";
    let ex = Extraction {
        has_errors: false,
        symbols: vec![sym("a", SymbolKind::Function, span_of(src, "fn a() {}"))],
        tokens: tokenize(src),
    };
    s.ingest_file("o1", "r1", "x.py", "python", &ex).unwrap();
    let mut q = SymbolQuery::new("a");
    assert_eq!(s.search_symbols(&q).unwrap().len(), 2);
    q.language = Some("python".into());
    let hits = s.search_symbols(&q).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file, "x.py");
    q.language = Some("RUST".into());
    let hits = s.search_symbols(&q).unwrap();
    assert_eq!(hits.len(), 1, "language filter is case-insensitive");
    assert_eq!(hits[0].qualified, "S::a");
    q.language = Some("zig".into());
    assert!(s.search_symbols(&q).unwrap().is_empty());
    // --file combined with language and kind.
    let mut q = SymbolQuery::new("*");
    q.file = Some("x.py".into());
    q.language = Some("python".into());
    q.kind = Some("function".into());
    assert_eq!(s.search_symbols(&q).unwrap().len(), 1);
    q.file = Some("lib.rs".into());
    assert!(s.search_symbols(&q).unwrap().is_empty());
}

fn describe_repo_filter(h: &Harness) {
    let s = open(h);
    s.index_bytes("o", "r1", "a.txt", b"foo", None).unwrap();
    s.index_bytes("o", "r2", "b.txt", b"foo bar", None).unwrap();
    s.index_bytes("p", "r1", "c.txt", b"foo", None).unwrap();
    let key = |v: Vec<crate::RepoInfo>| -> Vec<(String, String)> {
        v.into_iter().map(|r| (r.org, r.repo)).collect()
    };
    let pair = |o: &str, r: &str| (o.to_string(), r.to_string());
    assert_eq!(
        key(s.describe(Some("o"), Some("r2")).unwrap()),
        [pair("o", "r2")]
    );
    assert_eq!(
        key(s.describe(None, Some("r1")).unwrap()),
        [pair("o", "r1"), pair("p", "r1")],
        "repo filter without org spans orgs"
    );
    assert_eq!(
        key(s.describe(Some("o"), None).unwrap()),
        [pair("o", "r1"), pair("o", "r2")]
    );
    assert!(s.describe(Some("p"), Some("r2")).unwrap().is_empty());
    for (o, r) in [
        (Some("o"), Some("r2")),
        (None, Some("r1")),
        (Some("p"), Some("r2")),
    ] {
        assert_eq!(
            s.describe(o, r).unwrap(),
            s.describe_by_scan(o, r).unwrap(),
            "{o:?}/{r:?}"
        );
    }
}

/// An org that does not exist and an org that exists but has no repo with the
/// filter both describe as an empty list (not an error); a repo emptied by
/// prune still exists.
fn describe_unknown_vs_empty(h: &Harness) {
    let s = open(h);
    assert!(s.describe(Some("nope"), None).unwrap().is_empty());
    s.index_bytes("o", "r", "a.txt", b"foo", None).unwrap();
    assert!(s.describe(Some("nope"), None).unwrap().is_empty());
    assert!(s.describe(Some("o"), Some("nope")).unwrap().is_empty());
    s.index_bytes_with_origin("o", "empty", "d.txt", b"x", None, Some(ORIGIN_DIRECTORY))
        .unwrap();
    s.prune_files("o", "empty", &HashSet::new(), false).unwrap();
    let d = s.describe(Some("o"), Some("empty")).unwrap();
    assert_eq!(d, s.describe_by_scan(Some("o"), Some("empty")).unwrap());
    assert!(d.iter().all(|r| r.files == 0));
    assert_eq!(
        s.describe(Some("nope"), None).unwrap(),
        s.describe_by_scan(Some("nope"), None).unwrap()
    );
}

fn default_origin(h: &Harness) {
    let s = open(h);
    let file_origin = |s: &dyn Store, p: &str| {
        let t = s.file_tokens("o", "r", p).unwrap().unwrap();
        s.parent(t[0].id).unwrap().unwrap().origin
    };
    s.ingest_file("o", "r", "a.txt", "text", &plain("foo\n"))
        .unwrap();
    assert_eq!(
        file_origin(&*s, "a.txt"),
        None,
        "ingest_file sets no origin"
    );
    s.index_bytes("o", "r", "b.txt", b"foo", None).unwrap();
    assert_eq!(
        file_origin(&*s, "b.txt"),
        None,
        "index_bytes sets no origin"
    );
    s.ingest_file_with_origin(
        "o",
        "r",
        "a.txt",
        "text",
        &plain("foo\n"),
        Some(ORIGIN_DIRECTORY),
    )
    .unwrap();
    assert_eq!(file_origin(&*s, "a.txt").as_deref(), Some(ORIGIN_DIRECTORY));
    // Re-ingesting with the default origin clears it again.
    s.ingest_file("o", "r", "a.txt", "text", &plain("foo\n"))
        .unwrap();
    assert_eq!(file_origin(&*s, "a.txt"), None);
    // So neither is ever pruned.
    let gone = s.prune_files("o", "r", &HashSet::new(), false).unwrap();
    assert!(gone.is_empty());
}

/// Rows come back in (org, repo, file, offset) order at every grain, and
/// `limit` is a prefix of the unlimited result.
fn order_and_limit_determinism(h: &Harness) {
    let s = open(h);
    // Insert out of order on purpose.
    let src = "fn o() { foo(); }\nfn p() { foo(); foo(); }\n";
    let ex = Extraction {
        has_errors: false,
        symbols: vec![
            sym("o", SymbolKind::Function, span_of(src, "fn o() { foo(); }")),
            sym(
                "p",
                SymbolKind::Function,
                span_of(src, "fn p() { foo(); foo(); }"),
            ),
        ],
        tokens: tokenize(src),
    };
    for (o, r, f) in [
        ("b", "r", "z.rs"),
        ("a", "s", "a.rs"),
        ("a", "r", "m.rs"),
        ("a", "r", "b.rs"),
    ] {
        s.ingest_file(o, r, f, "rust", &ex).unwrap();
    }
    let s2 = s.snapshot().unwrap();
    for grain in [
        Grain::Token,
        Grain::Symbol,
        Grain::File,
        Grain::Repo,
        Grain::Org,
    ] {
        let mut q = Query::new("foo");
        q.grain = grain;
        let all = s.search(&q).unwrap();
        assert!(!all.is_empty());
        let key = |h: &crate::Hit| {
            (
                h.org.clone(),
                h.repo.clone(),
                h.file.clone(),
                h.span.map(|x| x.start),
            )
        };
        let keys: Vec<_> = all.iter().map(key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            keys, sorted,
            "{grain:?}: ordered by (org, repo, file, offset)"
        );
        assert_eq!(s.search(&q).unwrap(), all, "{grain:?}: repeatable");
        assert_eq!(s2.search(&q).unwrap(), all, "{grain:?}: snapshot agrees");
        for n in 0..=all.len() + 1 {
            q.limit = Some(n);
            let l = s.search(&q).unwrap();
            assert_eq!(l[..], all[..n.min(all.len())], "{grain:?} limit {n}");
        }
    }
    let mut q = SymbolQuery::new("*");
    let all = s.search_symbols(&q).unwrap();
    let keys: Vec<_> = all
        .iter()
        .map(|h| {
            (
                h.org.clone(),
                h.repo.clone(),
                h.file.clone(),
                h.span.map(|x| x.start),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    for n in 0..=all.len() {
        q.limit = Some(n);
        assert_eq!(s.search_symbols(&q).unwrap()[..], all[..n]);
    }
    // `search_symbols` sorts by (org, repo, file, offset, qualified name, node
    // id), so at a shared offset the shorter qualified name (the enclosing
    // symbol) comes first; the id only breaks ties beyond that.
    let src = "struct S { x: u8 }\n";
    let inner = Extraction {
        has_errors: false,
        symbols: vec![
            sym("T", SymbolKind::Type, span_of(src, "struct S")),
            sym("S", SymbolKind::Type, span_of(src, "struct S { x: u8 }")),
        ],
        tokens: tokenize(src),
    };
    s.ingest_file("t", "r", "n.rs", "rust", &inner).unwrap();
    let mut q = SymbolQuery::new("*");
    q.org = Some("t".into());
    let names: Vec<_> = s
        .search_symbols(&q)
        .unwrap()
        .into_iter()
        .map(|h| h.qualified)
        .collect();
    assert_eq!(
        names,
        ["S", "S::T"],
        "enclosing first, whatever the input order"
    );
}

fn prune_empty_keep(h: &Harness) {
    let s = open(h);
    for p in ["a.txt", "b.txt"] {
        s.index_bytes_with_origin("o", "r", p, b"foo", None, Some(ORIGIN_DIRECTORY))
            .unwrap();
    }
    s.index_bytes("o", "r", "manual.txt", b"foo", None).unwrap();
    let none = HashSet::new();
    let dry = s.prune_files("o", "r", &none, true).unwrap();
    assert_eq!(dry, ["a.txt", "b.txt"], "sorted, directory files only");
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    let done = s.prune_files("o", "r", &none, false).unwrap();
    assert_eq!(done, ["a.txt", "b.txt"]);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert!(s.file_tokens("o", "r", "manual.txt").unwrap().is_some());
    assert!(s.prune_files("o", "r", &none, false).unwrap().is_empty());
    assert!(s.prune_files("o", "nope", &none, false).unwrap().is_empty());
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

/// `vacuum` succeeds on an empty store and after churn, never changes what a
/// read returns, and is idempotent (a second run frees nothing more).
fn vacuum_preserves_reads(h: &Harness) {
    let s = open(h);
    s.vacuum().unwrap();
    s.index_bytes("o", "r", "a.txt", b"alpha beta", None)
        .unwrap();
    s.index_bytes("o", "r", "b.txt", b"beta", None).unwrap();
    // Replace a.txt so `alpha` is dead.
    s.index_bytes("o", "r", "a.txt", b"gamma", None).unwrap();
    let before = (
        s.search(&Query::new("gamma")).unwrap(),
        s.search(&Query::new("alpha")).unwrap(),
        s.describe(None, None).unwrap(),
    );
    assert_eq!(before.0.len(), 1);
    assert!(before.1.is_empty());
    s.vacuum().unwrap();
    let again = s.vacuum().unwrap();
    assert_eq!(again.terms_removed, 0, "second vacuum has nothing to free");
    let after = (
        s.search(&Query::new("gamma")).unwrap(),
        s.search(&Query::new("alpha")).unwrap(),
        s.describe(None, None).unwrap(),
    );
    assert_eq!(before, after);
    assert_eq!(s.search(&Query::new("beta")).unwrap().len(), 1);
    // Still writable afterwards, and a dead term can return.
    s.index_bytes("o", "r", "c.txt", b"alpha", None).unwrap();
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
}

/// NUL separates catalog and name key fields, so it is rejected in org, repo
/// and language (and symbol `lang_kind`), and a rejected write stores nothing.
fn nul_handling(h: &Harness) {
    let s = open(h);
    let rejected = |r: Result<crate::IngestStats, StoreError>| {
        assert!(matches!(r, Err(StoreError::Rejected(_))), "{r:?}")
    };
    rejected(s.index_bytes("o\0x", "r", "a.txt", b"foo", None));
    rejected(s.index_bytes("o", "r\0x", "a.txt", b"foo", None));
    rejected(s.index_bytes("o", "r", "a.txt", b"foo", Some("te\0xt")));
    rejected(s.ingest_file("o", "r", "a.txt", "te\0xt", &plain("foo")));
    let mut ex = plain("foo");
    ex.symbols.push(SymbolDecl {
        lang_kind: Some("k\0".into()),
        ..sym("f", SymbolKind::Function, span_of("foo", "foo"))
    });
    rejected(s.ingest_file("o", "r", "a.txt", "text", &ex));
    rejected(s.ingest_file("", "r", "a.txt", "text", &plain("foo")));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert_eq!(s.count_nodes(NodeKind::Org).unwrap(), 0);
    assert!(s.describe(None, None).unwrap().is_empty());
    // NUL in file content is fine (it is text), and NUL in a query matches
    // nothing rather than erroring.
    s.index_bytes("o", "r", "n.txt", b"foo\0bar", None).unwrap();
    assert!(s.search(&Query::new("fo\0o")).unwrap().is_empty());
    let mut q = Query::new("foo");
    q.org = Some("o\0".into());
    assert!(s.search(&q).unwrap().is_empty());
    assert!(s
        .search_symbols(&SymbolQuery::new("a\0b"))
        .unwrap()
        .is_empty());
}

/// Fixed-query differential harness (configuration equivalence): seed two
/// empty stores with the same fixed corpus, run a fixed query set at every
/// grain and filter, and require identical results (rows and order). `a` and
/// `b` are the same store type in two configurations (chunk size, cache
/// size, jobs, compaction, reopened or not), or a reference store and a
/// candidate implementation (`RemoteStore`). Panics naming the first
/// differing query. Both stores may already hold the same data (see
/// [`run_crash_rerun_differential`]); they must not hold different data.
pub fn run_differential(a: &dyn Store, b: &dyn Store) {
    for s in [a, b] {
        differential_seed(s);
        matrix_seed(s);
    }
    let grains = [
        Grain::Token,
        Grain::Symbol,
        Grain::File,
        Grain::Repo,
        Grain::Org,
    ];
    for text in [
        "foo",
        "bar",
        "S",
        "(",
        "missing",
        "fn",
        "a",
        "dup",
        "z",
        "x",
        "\u{1F600}",
        "let",
    ] {
        for grain in grains {
            let mut q = Query::new(text);
            q.grain = grain;
            let mut variants = vec![("plain", q.clone())];
            q.language = Some("rust".into());
            variants.push(("rust", q.clone()));
            q.language = None;
            q.org = Some("o1".into());
            q.repo = Some("r1".into());
            q.limit = Some(2);
            variants.push(("o1/r1/limit2", q.clone()));
            q.limit = None;
            q.org = None;
            q.repo = None;
            for n in [0, 1, 3, 5] {
                q.limit = Some(n);
                variants.push(("limit", q.clone()));
            }
            q.limit = None;
            q.class = Some(graph_core::TokenClass::Identifier);
            variants.push(("class", q.clone()));
            q.class = None;
            q.symbol_kind = Some("method".into());
            variants.push(("method", q.clone()));
            q.symbol_kind = Some("function".into());
            variants.push(("function", q));
            for (tag, q) in variants {
                assert_eq!(
                    a.search(&q).unwrap(),
                    b.search(&q).unwrap(),
                    "search {text}/{grain:?}/{tag}"
                );
            }
        }
    }
    for pat in ["*", "a", "S", "m*", "nope", "dup", "dup*", "z", "q", "b"] {
        let mut q = SymbolQuery::new(pat);
        let mut variants = vec![q.clone()];
        for n in [0, 2, 4] {
            q.limit = Some(n);
            variants.push(q.clone());
        }
        q.limit = None;
        q.file = Some("eq.rs".into());
        variants.push(q.clone());
        q.file = None;
        q.kind = Some("method".into());
        variants.push(q.clone());
        q.kind = None;
        q.language = Some("rust".into());
        variants.push(q.clone());
        q.language = None;
        q.limit = Some(1);
        variants.push(q);
        for q in variants {
            assert_eq!(
                a.search_symbols(&q).unwrap(),
                b.search_symbols(&q).unwrap(),
                "symbols {q:?}"
            );
        }
    }
    for (o, r) in [
        (None, None),
        (Some("o1"), None),
        (Some("o1"), Some("r1")),
        (None, Some("r2")),
        (Some("zz"), None),
    ] {
        assert_eq!(
            a.describe(o, r).unwrap(),
            b.describe(o, r).unwrap(),
            "describe {o:?}/{r:?}"
        );
    }
    for k in [
        NodeKind::Org,
        NodeKind::Repo,
        NodeKind::File,
        NodeKind::Symbol,
    ] {
        assert_eq!(
            a.count_nodes(k).unwrap(),
            b.count_nodes(k).unwrap(),
            "count {k:?}"
        );
    }
    differential_traversal(a, b);
    for (o, r, p) in [
        ("o1", "r1", "lib.rs"),
        ("o2", "r2", "main.zig"),
        ("o1", "r1", "none"),
        ("o1", "r3", "bom_crlf.rs"),
        ("o1", "r3", "cr.rs"),
        ("o1", "r3", "eq.rs"),
        ("o2", "r3", "astral.txt"),
        ("o2", "r3", "empty.txt"),
    ] {
        // Node ids are opaque, so compare content and spans only.
        let tok = |s: &dyn Store| {
            s.file_tokens(o, r, p).unwrap().map(|v| {
                v.into_iter()
                    .map(|n| (n.name, n.span, n.token_class))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(tok(a), tok(b), "file_tokens {p}");
    }
}

/// Crash-then-rerun equivalence: `crashed` indexes a batch that fails with a
/// storage error part-way (a file whose language holds a NUL poisons its
/// chunk: earlier chunks may stay committed, later files are never reached),
/// then re-runs the batch without the poison; `fresh` indexes the good batch
/// once. Both must then answer every query identically -- including after a
/// prune and after the fixed [`run_differential`] corpus is added on top --
/// so a crashed-and-resumed index is indistinguishable from a clean one
/// whatever the chunking. Both stores must be empty on entry.
pub fn run_crash_rerun_differential(fresh: &dyn Store, crashed: &dyn Store) {
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt"];
    let bodies: [&[u8]; 6] = [
        b"foo bar baz",
        b"foo (bar) qux",
        b"let x = foo;",
        b"bar bar bar",
        "\u{1F600} foo".as_bytes(),
        b"fn dup() { dup(); }",
    ];
    let good: Vec<BatchFile<'_>> = names
        .iter()
        .zip(bodies)
        .map(|(n, b)| BatchFile {
            path: n,
            bytes: b,
            language: Some("text"),
            origin: Some(ORIGIN_DIRECTORY),
        })
        .collect();
    // Poison after the third file: with small chunks the first files commit
    // and the rest never run; with one big chunk nothing commits.
    let mut poisoned = good[..3].to_vec();
    poisoned.push(BatchFile {
        path: "nul.txt",
        bytes: b"foo",
        language: Some("a\0b"),
        origin: Some(ORIGIN_DIRECTORY),
    });
    poisoned.extend_from_slice(&good[3..]);
    assert!(
        crashed
            .index_batch("o", "r", &poisoned, IndexOptions::default())
            .is_err(),
        "the poisoned batch must fail"
    );
    let rerun = crashed
        .index_batch("o", "r", &good, IndexOptions::default())
        .unwrap();
    assert!(rerun.iter().all(|r| r.is_ok()), "{rerun:?}");
    let once = fresh
        .index_batch("o", "r", &good, IndexOptions::default())
        .unwrap();
    assert!(once.iter().all(|r| r.is_ok()), "{once:?}");
    let same = |what: &str| {
        assert_eq!(
            fresh.describe(None, None).unwrap(),
            crashed.describe(None, None).unwrap(),
            "describe {what}"
        );
        assert_eq!(
            crashed.describe(None, None).unwrap(),
            crashed.describe_by_scan(None, None).unwrap(),
            "catalog vs scan {what}"
        );
        for k in [
            NodeKind::Org,
            NodeKind::Repo,
            NodeKind::File,
            NodeKind::Symbol,
        ] {
            assert_eq!(
                fresh.count_nodes(k).unwrap(),
                crashed.count_nodes(k).unwrap(),
                "count {k:?} {what}"
            );
        }
        for n in names {
            let tok = |s: &dyn Store| {
                s.file_tokens("o", "r", n).unwrap().map(|v| {
                    v.into_iter()
                        .map(|t| (t.name, t.span, t.token_class))
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(tok(fresh), tok(crashed), "file_tokens {n} {what}");
        }
        for text in ["foo", "bar", "dup", "x", "(", "\u{1F600}", "missing"] {
            for grain in [
                Grain::Token,
                Grain::Symbol,
                Grain::File,
                Grain::Repo,
                Grain::Org,
            ] {
                let mut q = Query::new(text);
                q.grain = grain;
                assert_eq!(
                    fresh.search(&q).unwrap(),
                    crashed.search(&q).unwrap(),
                    "search {text}/{grain:?} {what}"
                );
            }
        }
        assert_eq!(
            fresh.search_symbols(&SymbolQuery::new("*")).unwrap(),
            crashed.search_symbols(&SymbolQuery::new("*")).unwrap(),
            "symbols {what}"
        );
        assert!(
            !fresh.search(&Query::new("foo")).unwrap().is_empty(),
            "the corpus is indexed {what}"
        );
    };
    same("after rerun");
    // The rerun skipped whatever the crash had committed, and stored the rest.
    assert_eq!(crashed.count_nodes(NodeKind::File).unwrap(), names.len());
    let keep: HashSet<String> = ["a.txt", "d.txt"].iter().map(|s| s.to_string()).collect();
    assert_eq!(
        fresh.prune_files("o", "r", &keep, false).unwrap(),
        crashed.prune_files("o", "r", &keep, false).unwrap()
    );
    same("after prune");
    run_differential(fresh, crashed);
}

fn differential_seed(s: &dyn Store) {
    seed(s);
    s.index_bytes("o1", "r1", "notes.md", b"# foo\nbar (foo)\n", None)
        .unwrap();
    s.index_bytes("o1", "r2", "m.txt", b"foo mfoo m\n", None)
        .unwrap();
}

/// A batch re-index of unchanged bytes still refreshes the file's origin, in
/// both directions, and prune follows the origin.
fn batch_origin_refresh(h: &Harness) {
    let s = open(h);
    let origin_of = |s: &dyn Store| {
        let t = s.file_tokens("o", "r", "a.txt").unwrap().unwrap();
        s.parent(t[0].id).unwrap().unwrap().origin
    };
    let run = |origin| {
        let f = BatchFile {
            path: "a.txt",
            bytes: b"foo bar",
            language: Some("text"),
            origin,
        };
        s.index_batch("o", "r", &[f], IndexOptions::default())
            .unwrap()
            .remove(0)
            .unwrap()
    };
    assert!(!run(None).unchanged);
    assert_eq!(origin_of(&*s), None);
    assert!(
        run(Some(ORIGIN_DIRECTORY)).unchanged,
        "content skip still applies"
    );
    assert_eq!(origin_of(&*s).as_deref(), Some(ORIGIN_DIRECTORY));
    let none = HashSet::new();
    assert_eq!(s.prune_files("o", "r", &none, true).unwrap(), ["a.txt"]);
    assert!(run(None).unchanged);
    assert_eq!(origin_of(&*s), None, "origin cleared again");
    assert!(s.prune_files("o", "r", &none, true).unwrap().is_empty());
}

/// Stable projection of a node (ids are opaque per store).
type Proj = (
    String,
    String,
    Option<Span>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn proj(n: &Node) -> Proj {
    (
        format!("{:?}", n.kind),
        n.name.clone(),
        n.span,
        n.token_class.map(|c| format!("{c:?}")),
        n.symbol_kind.map(|k| format!("{k:?}")),
        n.lang_kind.clone(),
    )
}

fn projs(v: &[Node]) -> Vec<Proj> {
    v.iter().map(proj).collect()
}

fn names(v: &[Node]) -> Vec<&str> {
    v.iter().map(|n| n.name.as_str()).collect()
}

/// The org id of a file, found from its first token.
fn org_of_file(s: &dyn Store, org: &str, repo: &str, path: &str) -> NodeId {
    let t = s.file_tokens(org, repo, path).unwrap().unwrap();
    s.ancestors(t[0].id).unwrap().last().unwrap().id
}

/// Containment invariants for every node under `root`: a node's children are
/// exactly the nodes of `descendants(root)` whose parent it is, in the same
/// order, and each child names it as its parent.
fn check_tree(s: &dyn Store, root: NodeId) {
    let all = s.descendants(root).unwrap();
    let mut ids = vec![root];
    ids.extend(all.iter().map(|n| n.id));
    for id in ids {
        let kids = s.children(id).unwrap();
        let want: Vec<&Node> = all.iter().filter(|n| n.parent == Some(id)).collect();
        assert_eq!(
            kids.iter().map(|k| k.id).collect::<Vec<_>>(),
            want.iter().map(|k| k.id).collect::<Vec<_>>(),
            "children of {id}"
        );
        // Descendants are depth first: a node comes before its children.
        let pos = |x: NodeId| all.iter().position(|n| n.id == x);
        for k in &kids {
            assert_eq!(k.parent, Some(id));
            if let Some(p) = pos(id) {
                assert!(p < pos(k.id).unwrap(), "parent before child");
            }
        }
        if let Some(n) = all.iter().find(|n| n.id == id) {
            let anc = s.ancestors(id).unwrap();
            assert_eq!(
                anc.first().map(|a| a.id),
                n.parent,
                "ancestors start at the parent"
            );
            assert_eq!(anc.last().map(|a| a.kind), Some(NodeKind::Org));
        }
    }
}

/// children / descendants / ancestors: containment, order, leaf and unknown ids.
fn traversal(h: &Harness) {
    let s = open(h);
    seed(&*s);
    // Symbols with tokens before, between and after them.
    let src = "x y\nfn a() { foo(); }\nz\n";
    let mut ex = plain(src);
    ex.symbols.push(sym(
        "a",
        SymbolKind::Function,
        span_of(src, "fn a() { foo(); }"),
    ));
    s.ingest_file("o1", "r1", "mixed.rs", "rust", &ex).unwrap();
    let org = org_of_file(&*s, "o1", "r1", "lib.rs");

    // Org -> repo -> file -> top-level symbol; one top-level symbol in lib.rs.
    let repos = s.children(org).unwrap();
    assert_eq!(names(&repos), ["r1"]);
    let files = s.children(repos[0].id).unwrap();
    assert_eq!(names(&files), ["lib.rs", "mixed.rs"]);
    let top = s.children(files[0].id).unwrap();
    assert_eq!(names(&top), ["S"]);
    assert_eq!(top[0].kind, NodeKind::Symbol);

    // S holds its own tokens and the two methods, in source order.
    let in_s = s.children(top[0].id).unwrap();
    assert_eq!(names(&in_s), ["impl", "S", "{", "a", "b", "}"]);
    assert_eq!(in_s[3].kind, NodeKind::Symbol);
    let in_a = s.children(in_s[3].id).unwrap();
    assert_eq!(names(&in_a).join(" "), "fn a ( ) { foo ( ) ; foo ( ) ; }");
    assert!(in_a.iter().all(|n| n.kind == NodeKind::Token));

    // Tokens outside any symbol are children of the file, in source order.
    let mixed = s.children(files[1].id).unwrap();
    assert_eq!(names(&mixed), ["x", "y", "a", "z"]);
    assert_eq!(
        mixed.iter().map(|n| n.kind).collect::<Vec<_>>(),
        [
            NodeKind::Token,
            NodeKind::Token,
            NodeKind::Symbol,
            NodeKind::Token
        ]
    );
    let d = s.descendants(files[1].id).unwrap();
    assert_eq!(
        names(&d).join(" "),
        "x y a fn a ( ) { foo ( ) ; } z",
        "depth first, a symbol before its tokens"
    );

    // A file without symbols lists all its tokens.
    let zig_org = org_of_file(&*s, "o2", "r2", "main.zig");
    let zig_file = s.descendants(zig_org).unwrap();
    let zig_file_id = zig_file
        .iter()
        .find(|n| n.kind == NodeKind::File)
        .unwrap()
        .id;
    assert_eq!(
        s.children(zig_file_id).unwrap().len(),
        s.file_tokens("o2", "r2", "main.zig")
            .unwrap()
            .unwrap()
            .len()
    );

    // descendants of the org: repo, files, symbols and tokens, each once.
    let all = s.descendants(org).unwrap();
    let toks = ["lib.rs", "mixed.rs"]
        .iter()
        .map(|p| s.file_tokens("o1", "r1", p).unwrap().unwrap().len())
        .sum::<usize>();
    assert_eq!(all.len(), 1 + 2 + 4 + toks);
    let ids: HashSet<NodeId> = all.iter().map(|n| n.id).collect();
    assert_eq!(ids.len(), all.len(), "no node twice");
    check_tree(&*s, org);
    check_tree(&*s, zig_org);

    // Ancestors, nearest first: a token in `a`, then S, the file, repo, org.
    let t = s.file_tokens("o1", "r1", "lib.rs").unwrap().unwrap();
    let leaf = t.iter().find(|n| n.name == "foo").unwrap();
    let anc = s.ancestors(leaf.id).unwrap();
    assert_eq!(names(&anc), ["a", "S", "lib.rs", "r1", "o1"]);
    assert_eq!(
        anc.iter().map(|n| n.kind).collect::<Vec<_>>(),
        [
            NodeKind::Symbol,
            NodeKind::Symbol,
            NodeKind::File,
            NodeKind::Repo,
            NodeKind::Org
        ]
    );
    assert!(s.ancestors(org).unwrap().is_empty());

    // Leaves and unknown ids have no children or descendants.
    assert!(s.children(leaf.id).unwrap().is_empty());
    assert!(s.descendants(leaf.id).unwrap().is_empty());
    for bogus in [0, u64::MAX, u64::MAX / 3] {
        assert!(s.children(bogus).unwrap().is_empty(), "children {bogus}");
        assert!(s.descendants(bogus).unwrap().is_empty());
        assert!(s.ancestors(bogus).unwrap().is_empty());
    }

    // The same answers through a snapshot.
    let snap = s.snapshot().unwrap();
    assert_eq!(projs(&snap.descendants(org).unwrap()), projs(&all));
}

/// More shapes for the differential: BOM, CRLF, bare CR, astral text, equal-span
/// and zero-length symbols, tokens outside symbols, an empty file.
fn matrix_seed(s: &dyn Store) {
    let src =
        "\u{feff}fn a() {\r\n    foo();\r\n}\r\nfn b() {\r    foo();\r}\rlet \u{1F600} = 1;\n";
    let mut ex = plain(src);
    ex.symbols.push(sym(
        "a",
        SymbolKind::Function,
        span_of(src, "fn a() {\r\n    foo();\r\n}"),
    ));
    ex.symbols.push(sym(
        "b",
        SymbolKind::Function,
        span_of(src, "fn b() {\r    foo();\r}"),
    ));
    s.ingest_file("o1", "r3", "bom_crlf.rs", "rust", &ex)
        .unwrap();
    let src = "fn a() {\rfoo();\r}\r";
    let mut ex = plain(src);
    ex.symbols.push(sym(
        "a",
        SymbolKind::Function,
        span_of(src, "fn a() {\rfoo();\r}"),
    ));
    s.ingest_file("o1", "r3", "cr.rs", "rust", &ex).unwrap();
    // Three symbols with one span (two share a name), a zero-length symbol,
    // and a token outside every symbol.
    let src = "fn q() { bar(); }\nfoo();\n";
    let whole = span_of(src, "fn q() { bar(); }");
    let mut zero = span_of(src, "foo();");
    zero.end = zero.start;
    zero.end_line = zero.start_line;
    zero.end_col = zero.start_col;
    let mut ex = plain(src);
    ex.symbols.push(sym("dup", SymbolKind::Function, whole));
    ex.symbols.push(sym("dup", SymbolKind::Method, whole));
    ex.symbols.push(sym("dup2", SymbolKind::Type, whole));
    ex.symbols.push(sym("z", SymbolKind::Other, zero));
    s.ingest_file("o1", "r3", "eq.rs", "rust", &ex).unwrap();
    s.index_bytes(
        "o2",
        "r3",
        "astral.txt",
        "foo \u{1F600} foo\r\nx\r".as_bytes(),
        None,
    )
    .unwrap();
    s.index_bytes("o2", "r3", "empty.txt", b"", None).unwrap();
}

/// Compare children, descendants and ancestors of two stores through stable
/// projections, from every org that holds a probe file.
fn differential_traversal(a: &dyn Store, b: &dyn Store) {
    for (o, r, p) in [
        ("o1", "r1", "lib.rs"),
        ("o2", "r2", "main.zig"),
        ("o1", "r3", "eq.rs"),
    ] {
        let (ra, rb) = (org_of_file(a, o, r, p), org_of_file(b, o, r, p));
        let (da, db) = (a.descendants(ra).unwrap(), b.descendants(rb).unwrap());
        assert_eq!(projs(&da), projs(&db), "descendants of {o}");
        for (na, nb) in da.iter().zip(&db) {
            assert_eq!(
                projs(&a.children(na.id).unwrap()),
                projs(&b.children(nb.id).unwrap()),
                "children of {}",
                na.name
            );
            assert_eq!(
                projs(&a.ancestors(na.id).unwrap()),
                projs(&b.ancestors(nb.id).unwrap()),
                "ancestors of {}",
                na.name
            );
        }
        check_tree(a, ra);
        check_tree(b, rb);
    }
}

/// A third-party extractor for a language the built-in table does not know,
/// claiming its own extensions (any case, leading dot optional).
struct ToyExtractor;

impl Extractor for ToyExtractor {
    fn language(&self) -> &str {
        "ToyLang"
    }
    fn version(&self) -> String {
        "toy-1".into()
    }
    fn extensions(&self) -> &[&str] {
        &["TOY", ".toyx", "py"]
    }
    fn extract(&self, source: &str) -> Extraction {
        let body = source.trim_end();
        Extraction {
            has_errors: false,
            symbols: vec![SymbolDecl {
                name: "whole".into(),
                kind: SymbolKind::Other,
                lang_kind: Some("toy_file".into()),
                span: span_of(source, body),
            }],
            tokens: tokenize(source),
        }
    }
}

/// An extractor registered at open time is used for the extensions it
/// claims (auto-detected language), and an explicit language still wins.
fn claimed_extension_extractor(h: &Harness) {
    let s = (h.open)(vec![Box::new(ToyExtractor)]).expect("open store");
    for (path, src) in [
        ("a.toy", "alpha beta\n"),
        ("dir/b.ToyX", "gamma\n"),
        // A claim overrides the built-in table (`py` is python there).
        ("e.py", "eps\n"),
    ] {
        s.index_bytes("o", "r", path, src.as_bytes(), None).unwrap();
    }
    // An explicit language overrides the claim: fallback, no symbols.
    s.index_bytes("o", "r", "c.toy", b"delta\n", Some("zig"))
        .unwrap();
    let hits = s.search_symbols(&SymbolQuery::new("whole")).unwrap();
    let mut files: Vec<(&str, Option<&str>, Option<&str>)> = hits
        .iter()
        .map(|h| {
            (
                h.file.as_str(),
                h.language.as_deref(),
                h.lang_kind.as_deref(),
            )
        })
        .collect();
    files.sort();
    assert_eq!(
        files,
        vec![
            ("a.toy", Some("toylang"), Some("toy_file")),
            ("dir/b.ToyX", Some("toylang"), Some("toy_file")),
            ("e.py", Some("toylang"), Some("toy_file")),
        ]
    );
    let info = s.describe(Some("o"), Some("r")).unwrap();
    let langs: Vec<&String> = info[0].languages.keys().collect();
    assert_eq!(langs, vec!["toylang", "zig"]);
}

/// Stats with the backend-specific node id blanked, for comparing two repos.
fn stats_proj(r: &Result<crate::IngestStats, StoreError>) -> String {
    match r {
        Ok(st) => {
            let mut st = st.clone();
            st.file_id = 0;
            format!("{st:?}")
        }
        Err(e) => format!("Err({e})"),
    }
}

fn prepare_all(
    s: &dyn Store,
    repo: &str,
    files: &[BatchFile<'_>],
    opts: IndexOptions,
) -> Vec<crate::PreparedFile> {
    files
        .iter()
        .map(|f| s.prepare("o", repo, f, opts).unwrap())
        .collect()
}

/// `prepare` + `index_prepared` stores exactly what `index_batch` stores for
/// the same inputs: same per-file outcomes (in order), same tokens and
/// symbols, same catalog. Files are prepared on several threads at once.
fn prepared_matches_batch(h: &Harness) {
    let s = (h.open)(vec![Box::new(BadSpans)]).expect("open store");
    let lib = RUST.as_bytes();
    let f = |p, b: &'static [u8], l: Option<&'static str>| BatchFile {
        path: p,
        bytes: b,
        language: l,
        origin: Some(ORIGIN_DIRECTORY),
    };
    let files = [
        f("src/lib.rs", lib, Some("rust")),
        f("./notes.md", b"# foo\nbar (foo)\n", None),
        f("bad.c", b"bad", Some("conf-bad")),
        f("bin.dat", b"\xff\xfe", None),
        f("m.txt", b"foo mfoo m\n", None),
        f("src/lib.rs", b"fn dup() {}\n", Some("rust")),
    ];
    for opts in [IndexOptions::default(), IndexOptions { reindex: true }] {
        let batch = s.index_batch("o", "a", &files, opts).unwrap();
        let prepared: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = files
                .iter()
                .map(|file| {
                    let s = &*s;
                    sc.spawn(move || s.prepare("o", "b", file, opts).unwrap())
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(prepared[1].path(), "notes.md", "path is normalized");
        let committed = s.index_prepared("o", "b", prepared, opts).unwrap();
        let (x, y): (Vec<_>, Vec<_>) = (
            batch.iter().map(stats_proj).collect(),
            committed.iter().map(stats_proj).collect(),
        );
        assert_eq!(x, y, "per-file outcomes ({opts:?})");
    }
    for p in ["src/lib.rs", "notes.md", "m.txt", "bad.c", "bin.dat"] {
        let toks = |repo| {
            s.file_tokens("o", repo, p)
                .unwrap()
                .map(|v| v.iter().map(proj).collect::<Vec<_>>())
        };
        assert_eq!(toks("a"), toks("b"), "file_tokens {p}");
    }
    for q in ["foo", "dup", "m"] {
        let hits = |repo: &str| {
            let mut q = Query::new(q);
            q.repo = Some(repo.into());
            s.search(&q)
                .unwrap()
                .iter()
                .map(|h| h.count)
                .collect::<Vec<_>>()
        };
        assert_eq!(hits("a"), hits("b"), "search {q}");
    }
    let syms = |repo: &str| {
        let mut q = SymbolQuery::new("dup");
        q.repo = Some(repo.into());
        s.search_symbols(&q).unwrap().len()
    };
    assert_eq!(syms("a"), syms("b"));
    let d = s.describe(None, None).unwrap();
    assert_eq!(d, s.describe_by_scan(None, None).unwrap());
    let mut a = d.iter().find(|r| r.repo == "a").unwrap().clone();
    let b = d.iter().find(|r| r.repo == "b").unwrap();
    a.repo = "b".into();
    assert_eq!(format!("{a:?}"), format!("{b:?}"), "describe");
}

/// Counts `extract` calls for language `conf-count`.
struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Extractor for Counting {
    fn language(&self) -> &str {
        "conf-count"
    }
    fn extract(&self, src: &str) -> Extraction {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        plain(src)
    }
}

/// `prepare` skips extraction for a file stored with the same fingerprint
/// (and says so), unless `reindex`; the commit still refreshes `origin`.
fn prepare_skips_unchanged(h: &Harness) {
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let s = (h.open)(vec![Box::new(Counting(n.clone()))]).expect("open store");
    let calls = || n.load(std::sync::atomic::Ordering::SeqCst);
    let file = |origin| BatchFile {
        path: "a.cnt",
        bytes: b"foo bar",
        language: Some("conf-count"),
        origin,
    };
    let first = prepare_all(&*s, "r", &[file(None)], IndexOptions::default());
    assert!(!first[0].is_unchanged());
    assert_eq!(first[0].bytes_len(), 7);
    assert_eq!(calls(), 1);
    // Nothing is stored until the commit.
    assert!(s.file_tokens("o", "r", "a.cnt").unwrap().is_none());
    let out = s
        .index_prepared("o", "r", first, IndexOptions::default())
        .unwrap();
    assert!(!out[0].as_ref().unwrap().unchanged);

    let again = prepare_all(
        &*s,
        "r",
        &[file(Some(ORIGIN_DIRECTORY))],
        IndexOptions::default(),
    );
    assert!(again[0].is_unchanged());
    assert_eq!(calls(), 1, "unchanged file not extracted");
    let out = s
        .index_prepared("o", "r", again, IndexOptions::default())
        .unwrap();
    assert!(out[0].as_ref().unwrap().unchanged);
    let origin = s.file_tokens("o", "r", "a.cnt").unwrap().unwrap()[0].parent;
    let f = s.get(origin.unwrap()).unwrap().unwrap();
    assert_eq!(
        f.origin.as_deref(),
        Some(ORIGIN_DIRECTORY),
        "origin refreshed"
    );

    let forced = prepare_all(&*s, "r", &[file(None)], IndexOptions { reindex: true });
    assert!(!forced[0].is_unchanged());
    assert_eq!(calls(), 2, "reindex extracts");
    let out = s
        .index_prepared("o", "r", forced, IndexOptions { reindex: true })
        .unwrap();
    assert!(out[0].as_ref().unwrap().replaced);
}

/// Per-file rejections (invalid spans, not UTF-8) come back in their slots,
/// in input order, and store nothing; their neighbours are stored.
fn prepared_rejections_in_order(h: &Harness) {
    let s = (h.open)(vec![Box::new(BadSpans)]).expect("open store");
    let f = |p, b: &'static [u8], l| BatchFile {
        path: p,
        bytes: b,
        language: l,
        origin: None,
    };
    let files = [
        f("ok1.c", b"xxxx", Some("text")),
        f("bad.c", b"bad", Some("conf-bad")),
        f("bin.c", b"\xff", None),
        f("ok2.c", b"yyyy", Some("text")),
    ];
    let p = prepare_all(&*s, "r", &files, IndexOptions::default());
    let out = s
        .index_prepared("o", "r", p, IndexOptions::default())
        .unwrap();
    assert_eq!(out.len(), 4);
    assert!(out[0].is_ok() && out[3].is_ok());
    assert!(matches!(&out[1], Err(StoreError::InvalidSpan(m)) if m.contains("bad.c")));
    assert!(matches!(&out[2], Err(StoreError::NotUtf8(m)) if m.contains("bin.c")));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    assert!(s
        .index_prepared("o", "r", vec![], IndexOptions::default())
        .unwrap()
        .is_empty());
}

/// The commit-time check is the authority, both ways: a file prepared as
/// unchanged that changed since is extracted and stored; a file prepared as
/// changed that someone stored meanwhile is reported unchanged.
fn prepared_changed_since_prepare(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    s.index_batch("o", "r", &[bf("a.txt", b"foo bar")], d)
        .unwrap();
    let p = prepare_all(&*s, "r", &[bf("a.txt", b"foo bar")], d);
    assert!(p[0].is_unchanged());
    s.index_bytes("o", "r", "a.txt", b"foo CHANGED", Some("text"))
        .unwrap();
    let out = s.index_prepared("o", "r", p, d).unwrap();
    let st = out[0].as_ref().unwrap();
    assert!(st.replaced && !st.unchanged && st.tokens == 2, "{st:?}");
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 1);
    assert_eq!(s.search(&Query::new("CHANGED")).unwrap().len(), 0);

    let p = prepare_all(&*s, "r", &[bf("a.txt", b"foo new")], d);
    assert!(!p[0].is_unchanged());
    s.index_batch("o", "r", &[bf("a.txt", b"foo new")], d)
        .unwrap();
    let out = s.index_prepared("o", "r", p, d).unwrap();
    assert!(out[0].as_ref().unwrap().unchanged, "stored meanwhile");
}

/// Editing a stored file and preparing it without `reindex` extracts it and
/// the commit replaces the stored copy (the everyday incremental case).
fn prepared_changed_file_replaces(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    s.index_batch("o", "r", &[bf("a.txt", b"foo")], d).unwrap();
    let p = prepare_all(&*s, "r", &[bf("a.txt", b"foo bar")], d);
    assert!(!p[0].is_unchanged());
    assert_eq!(p[0].language(), "text");
    let out = s.index_prepared("o", "r", p, d).unwrap();
    let st = out[0].as_ref().unwrap();
    assert!(st.replaced && !st.unchanged, "{st:?}");
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 1);
}

/// The same path twice in one batch (`./x` normalizes to `x`): the prepared
/// path stores what `index_batch` stores, the last copy winning.
fn prepared_duplicate_paths(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    for repo in ["a", "b"] {
        s.index_batch("o", repo, &[bf("x.txt", b"foo B")], d)
            .unwrap();
    }
    let files = [bf("x.txt", b"foo A"), bf("./x.txt", b"foo B")];
    let batch = s.index_batch("o", "a", &files, d).unwrap();
    let p = prepare_all(&*s, "b", &files, d);
    let prepared = s.index_prepared("o", "b", p, d).unwrap();
    let (x, y): (Vec<_>, Vec<_>) = (
        batch.iter().map(stats_proj).collect(),
        prepared.iter().map(stats_proj).collect(),
    );
    assert_eq!(x.len(), 2);
    assert_eq!(
        x.iter().map(|v| v.replace("\"a\"", "")).collect::<Vec<_>>(),
        y.iter().map(|v| v.replace("\"b\"", "")).collect::<Vec<_>>()
    );
    for repo in ["a", "b"] {
        let toks = s.file_tokens("o", repo, "x.txt").unwrap().unwrap();
        assert_eq!(toks[1].name, "B", "{repo}: last copy wins");
    }
    // Committing to another repo than the one prepared for is refused.
    let p = prepare_all(&*s, "b", &[bf("y.txt", b"y")], d);
    assert!(matches!(
        s.index_prepared("o", "a", p, d),
        Err(StoreError::Rejected(_))
    ));
    assert!(s.file_tokens("o", "a", "y.txt").unwrap().is_none());
}
