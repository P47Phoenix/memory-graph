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
//!     accepts_remote_prepared: false,
//!     guard: Some(Box::new(tempdir)),
//! });
//! ```
//! The factory is called once per case, so each case sees an empty database.
use crate::{
    BatchFile, Grain, IndexOptions, Query, Store, StoreError, SymbolQuery, ORIGIN_DIRECTORY,
};
use graph_core::tokenizer::tokenize;
use graph_core::{Extraction, Extractor, Node, NodeId, NodeKind, Span, SymbolDecl, SymbolKind};
use std::collections::{BTreeMap, HashSet};

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
    /// The backend commits a [`PreparedFile::remote`](crate::PreparedFile::remote)
    /// file (ADR 0004 D2): true for a client backend that forwards the bytes
    /// to a server that parses them; false (the default) for the embedded
    /// store, which must reject it in that file's slot.
    pub accepts_remote_prepared: bool,
    /// Kept alive for the case's duration (e.g. a temp dir).
    pub guard: Option<Box<dyn std::any::Any>>,
}

/// Every case, by name.
pub const CASES: &[(&str, Case)] = &[
    ("ingest_replace_and_grains", ingest_replace_and_grains),
    ("method_and_class_grains", method_and_class_grains),
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
    (
        "prepared_counted_matches_uncounted",
        prepared_counted_matches_uncounted,
    ),
    ("prepare_skips_unchanged", prepare_skips_unchanged),
    ("prepare_with_snapshot", prepare_with_snapshot),
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
    ("remote_prepared_is_rejected", remote_prepared_is_rejected),
    ("backslash_paths", backslash_paths),
    ("prune_backslash_keep", prune_backslash_keep),
    ("owner_hint_class_grain", owner_hint_class_grain),
    (
        "extractor_gaps_name_a_missing_extractor",
        extractor_gaps_name_a_missing_extractor,
    ),
    ("space_usage_is_consistent", space_usage_is_consistent),
    ("encoded_files", encoded_files),
    (
        "encoding_hint_strict_and_binary",
        encoding_hint_strict_and_binary,
    ),
    ("batch_level_encoding_hint", batch_level_encoding_hint),
    ("encoding_exposure", encoding_exposure),
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
        owner: None,
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

/// Method and class grains: the nearest enclosing callable / type-or-impl,
/// with that symbol's full span; `symbol_kind` narrows further.
fn method_and_class_grains(h: &Harness) {
    const SRC: &str = "foo();\nstruct T { foo: u32 }\nimpl S {\n    fn a() { foo(); foo(); }\n    fn b() { foo(); }\n}\nfn free() { foo(); }\n";
    const JS: &str = "class C { m() { foo(); } }\n";
    let lk = |name: &str, kind: SymbolKind, lang_kind: &str, needle: &str| SymbolDecl {
        owner: None,
        name: name.into(),
        kind,
        lang_kind: Some(lang_kind.into()),
        span: span_of(SRC, needle),
    };
    let impl_src = "impl S {\n    fn a() { foo(); foo(); }\n    fn b() { foo(); }\n}";
    let rust = Extraction {
        has_errors: false,
        symbols: vec![
            lk("T", SymbolKind::Type, "struct", "struct T { foo: u32 }"),
            lk("S", SymbolKind::Other, "impl", impl_src),
            lk("a", SymbolKind::Method, "fn", "fn a() { foo(); foo(); }"),
            lk("b", SymbolKind::Method, "fn", "fn b() { foo(); }"),
            lk("free", SymbolKind::Function, "fn", "fn free() { foo(); }"),
        ],
        tokens: tokenize(SRC),
    };
    let js = Extraction {
        has_errors: false,
        symbols: vec![
            SymbolDecl {
                owner: None,
                name: "C".into(),
                kind: SymbolKind::Type,
                lang_kind: Some("class".into()),
                span: span_of(JS, JS.trim_end()),
            },
            SymbolDecl {
                owner: None,
                name: "m".into(),
                kind: SymbolKind::Method,
                lang_kind: Some("method".into()),
                span: span_of(JS, "m() { foo(); }"),
            },
        ],
        tokens: tokenize(JS),
    };
    let s = open(h);
    s.ingest_file("o1", "r1", "c.js", "javascript", &js)
        .unwrap();
    s.ingest_file("o1", "r1", "lib.rs", "rust", &rust).unwrap();
    s.ingest_file("o2", "r2", "main.zig", "zig", &plain(ZIG))
        .unwrap();
    // (file, symbol, count, lang_kind, no_symbols, no_matching_symbol)
    type Row = (String, Option<String>, usize, Option<String>, bool, bool);
    let rows = |q: &Query| -> Vec<Row> {
        let hits = s.search(q).unwrap();
        for h in &hits {
            assert_eq!(h.grain, q.grain);
        }
        hits.into_iter()
            .map(|h| {
                (
                    h.file.unwrap(),
                    h.symbol,
                    h.count,
                    h.lang_kind,
                    h.no_symbols,
                    h.no_matching_symbol,
                )
            })
            .collect()
    };
    let row = |file: &str, sym: Option<&str>, n: usize, lk: Option<&str>, ns: bool, nm: bool| {
        (
            file.to_string(),
            sym.map(str::to_string),
            n,
            lk.map(str::to_string),
            ns,
            nm,
        )
    };

    let mut q = Query::new("foo");
    q.grain = Grain::Method;
    assert_eq!(
        rows(&q),
        [
            row("c.js", Some("C::m"), 1, Some("method"), false, false),
            // module-level `foo()` and the struct field: no enclosing callable
            row("lib.rs", None, 2, None, false, true),
            row("lib.rs", Some("S::a"), 2, Some("fn"), false, false),
            row("lib.rs", Some("S::b"), 1, Some("fn"), false, false),
            row("lib.rs", Some("free"), 1, Some("fn"), false, false),
            row("main.zig", None, 1, None, true, false),
        ]
    );
    // The method's full span, byte and line/col.
    let a = &s.search(&q).unwrap()[2];
    let sp = a.span.unwrap();
    assert_eq!(
        &SRC[sp.start as usize..sp.end as usize],
        "fn a() { foo(); foo(); }"
    );
    assert_eq!(sp, span_of(SRC, "fn a() { foo(); foo(); }"));
    assert_eq!(
        (sp.start_line, sp.start_col, sp.end_line, sp.end_col),
        (4, 5, 4, 29)
    );

    q.grain = Grain::Class;
    assert_eq!(
        rows(&q),
        [
            row("c.js", Some("C"), 1, Some("class"), false, false),
            // module-level `foo()` and the one in `free`: no enclosing type
            row("lib.rs", None, 2, None, false, true),
            row("lib.rs", Some("T"), 1, Some("struct"), false, false),
            row("lib.rs", Some("S"), 3, Some("impl"), false, false),
            row("main.zig", None, 1, None, true, false),
        ]
    );
    let hits = s.search(&q).unwrap();
    let imp = &hits[3];
    assert_eq!(imp.symbol_kind, Some(SymbolKind::Other));
    assert_eq!(imp.span, Some(span_of(SRC, impl_src)));

    // `symbol_kind` narrows within the grain.
    q.symbol_kind = Some("struct".into());
    assert_eq!(
        rows(&q),
        [
            row("c.js", None, 1, None, false, true),
            row("lib.rs", None, 5, None, false, true),
            row("lib.rs", Some("T"), 1, Some("struct"), false, false),
            row("main.zig", None, 1, None, true, false),
        ]
    );
    q.grain = Grain::Method;
    q.symbol_kind = Some("function".into());
    assert_eq!(
        rows(&q),
        [
            row("c.js", None, 1, None, false, true),
            row("lib.rs", None, 5, None, false, true),
            row("lib.rs", Some("free"), 1, Some("fn"), false, false),
            row("main.zig", None, 1, None, true, false),
        ]
    );
    // Language filter still applies.
    q.symbol_kind = None;
    q.language = Some("javascript".into());
    assert_eq!(
        rows(&q),
        [row("c.js", Some("C::m"), 1, Some("method"), false, false)]
    );
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
            IndexOptions {
                reindex: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!forced.unchanged && forced.tokens == first.tokens);
    let changed = s.index_bytes("o", "r", "a.txt", b"foo baz", None).unwrap();
    assert!(!changed.unchanged && changed.replaced);
    assert_eq!(s.search(&Query::new("bar")).unwrap().len(), 0);
    assert_eq!(s.search(&Query::new("baz")).unwrap().len(), 1);
    // Rejections store nothing: a binary file (ADR 0007 C5), and invalid
    // UTF-8 under a strict `utf-8` hint (today's `NotUtf8`, ADR 0007 C8).
    assert!(matches!(
        s.index_bytes("o", "r", "bin", PNG, None),
        Err(StoreError::Binary(_))
    ));
    assert!(s.file_tokens("o", "r", "bin").unwrap().is_none());
    let strict_utf8 = IndexOptions {
        encoding: Some(encoding_rs::UTF_8),
        strict_encoding: true,
        ..Default::default()
    };
    assert!(matches!(
        s.index_bytes_opts("o", "r", "bad", b"a \xff b", None, None, strict_utf8),
        Err(StoreError::NotUtf8(_))
    ));
    assert!(s.file_tokens("o", "r", "bad").unwrap().is_none());
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        .index_batch(
            "o",
            "r",
            &files,
            IndexOptions {
                reindex: true,
                ..Default::default()
            },
        )
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
        Grain::Method,
        Grain::Class,
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
        owner: None,
        lang_kind: Some("k\0".into()),
        ..sym("f", SymbolKind::Function, span_of("foo", "foo"))
    });
    rejected(s.ingest_file("o", "r", "a.txt", "text", &ex));
    let mut ex = plain("foo");
    ex.symbols
        .push(sym("f", SymbolKind::Function, span_of("foo", "foo")).with_owner("T\0"));
    rejected(s.ingest_file("o", "r", "a.txt", "text", &ex));
    rejected(s.ingest_file("", "r", "a.txt", "text", &plain("foo")));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert_eq!(s.count_nodes(NodeKind::Org).unwrap(), 0);
    assert!(s.describe(None, None).unwrap().is_empty());
    // NUL in file content makes it binary unless it is UTF-16 (ADR 0007 C5,
    // one check on every path), and NUL in a query matches nothing rather
    // than erroring.
    assert!(matches!(
        s.index_bytes("o", "r", "n.txt", b"foo\0bar", None),
        Err(StoreError::Binary(_))
    ));
    s.index_bytes("o", "r", "n.txt", b"foo bar", None).unwrap();
    assert!(s.search(&Query::new("fo\0o")).unwrap().is_empty());
    let mut q = Query::new("foo");
    q.org = Some("o\0".into());
    assert!(s.search(&q).unwrap().is_empty());
    assert!(s
        .search_symbols(&SymbolQuery::new("a\0b"))
        .unwrap()
        .is_empty());
}

/// A [`PreparedFile::remote`](crate::PreparedFile::remote) file (ADR 0004
/// D2) carries raw bytes for a server to parse. An embedded store rejects it
/// in that file's slot (the other files of the call are stored as usual); a
/// client backend (`accepts_remote_prepared`) forwards it and stores it like
/// any other file. Either way the file's normalized path, `bytes_len`,
/// footprint and `remote_parts` are what the constructor was given.
fn remote_prepared_is_rejected(h: &Harness) {
    let s = open(h);
    let bytes = b"alpha beta".to_vec();
    let remote = crate::PreparedFile::remote(
        "o",
        "r",
        "./src/../src/remote.txt",
        bytes.clone(),
        Some("Text".into()),
        Some(ORIGIN_DIRECTORY.into()),
    );
    assert_eq!(remote.path(), "src/remote.txt", "normalized like prepare");
    assert_eq!(remote.language(), "text");
    assert_eq!(remote.bytes_len(), bytes.len());
    assert!(!remote.is_unchanged());
    assert!(remote.memory_footprint() >= bytes.len());
    let parts = remote.remote_parts().expect("remote parts");
    assert_eq!(parts.org, "o");
    assert_eq!(parts.repo, "r");
    assert_eq!(parts.path, "src/remote.txt");
    assert_eq!(parts.bytes, bytes.as_slice());
    assert_eq!(parts.language, Some("text"));
    assert_eq!(parts.origin, Some(ORIGIN_DIRECTORY));
    let no_lang = crate::PreparedFile::remote("o", "r", "x.txt", vec![], None, None);
    assert_eq!(no_lang.language(), "");
    assert_eq!(no_lang.remote_parts().unwrap().language, None);

    let local = s
        .prepare(
            "o",
            "r",
            &bf("local.txt", b"gamma"),
            IndexOptions::default(),
        )
        .unwrap();
    // A remote backend's own `prepare` is remote too.
    assert_eq!(local.remote_parts().is_some(), h.accepts_remote_prepared);
    let results = s
        .index_prepared("o", "r", vec![local, remote], IndexOptions::default())
        .unwrap();
    assert_eq!(results.len(), 2);
    let local_stats = results[0].as_ref().expect("local file stored");
    assert_eq!(local_stats.path, "local.txt");
    if h.accepts_remote_prepared {
        let stats = results[1]
            .as_ref()
            .expect("remote file stored by the server");
        assert_eq!(stats.path, "src/remote.txt");
        assert_eq!(stats.language, "text");
        assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
        assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 2);
    } else {
        match &results[1] {
            Err(StoreError::Rejected(msg)) => assert_eq!(
                msg,
                "remote-prepared file cannot be committed to an embedded store"
            ),
            other => panic!("embedded store must reject a remote-prepared file, got {other:?}"),
        }
        assert!(s.search(&Query::new("alpha")).unwrap().is_empty());
        assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
        // The rejection reaches nothing: no org/repo/file rows for it.
        assert!(s.file_tokens("o", "r", "src/remote.txt").unwrap().is_none());
    }
    assert_eq!(s.search(&Query::new("gamma")).unwrap().len(), 1);
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
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
        Grain::Method,
        Grain::Class,
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
        ENC_ID,
        ENC_LATIN,
        ENC_CJK,
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
            variants.push(("function", q.clone()));
            q.symbol_kind = Some("struct".into());
            variants.push(("struct", q));
            for (tag, q) in variants {
                assert_eq!(
                    a.search(&q).unwrap(),
                    b.search(&q).unwrap(),
                    "search {text}/{grain:?}/{tag}"
                );
            }
        }
    }
    for pat in [
        "*", "a", "S", "m*", "nope", "dup", "dup*", "z", "q", "b", "T",
    ] {
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
        ("o2", "enc", "u8.txt"),
        ("o2", "enc", "le.txt"),
        ("o2", "enc", "be.txt"),
        ("o2", "enc", "w1252.txt"),
        ("o2", "enc", "sjis.txt"),
        ("o2", "enc", "hinted.txt"),
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
    let names = [
        "a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt", "g.txt", "h.txt",
    ];
    // ADR 0007: a UTF-16 file (auto) and a Shift_JIS one (hinted) too.
    let le = utf16("foo \u{e9}t\u{e9} bar\n", false, true);
    let sjis = legacy("foo \u{65e5}\u{672c}\n", encoding_rs::SHIFT_JIS);
    let bodies: [&[u8]; 8] = [
        b"foo bar baz",
        b"foo (bar) qux",
        b"let x = foo;",
        b"bar bar bar",
        "\u{1F600} foo".as_bytes(),
        b"fn dup() { dup(); }",
        &le,
        &sjis,
    ];
    let good: Vec<BatchFile<'_>> = names
        .iter()
        .zip(bodies)
        .map(|(n, b)| BatchFile {
            path: n,
            bytes: b,
            language: Some("text"),
            origin: Some(ORIGIN_DIRECTORY),
            encoding: (*n == "h.txt").then_some(encoding_rs::SHIFT_JIS),
            ..Default::default()
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
        ..Default::default()
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
        for text in [
            "foo",
            "bar",
            "dup",
            "x",
            "(",
            "\u{1F600}",
            "missing",
            "\u{e9}t\u{e9}",
            "\u{65e5}\u{672c}",
        ] {
            for grain in [
                Grain::Token,
                Grain::Symbol,
                Grain::Method,
                Grain::Class,
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
    s.ingest_file("o1", "r1", "own.toy", "toy", &owner_extraction())
        .unwrap();
    s.index_bytes("o1", "r1", "notes.md", b"# foo\nbar (foo)\n", None)
        .unwrap();
    s.index_bytes("o1", "r2", "m.txt", b"foo mfoo m\n", None)
        .unwrap();
    // A Windows-style path (#100, #120): stored as `sub/dir/.gitignore`.
    s.index_bytes("o1", "r2", r"sub\dir\.gitignore", b"foo x\n", None)
        .unwrap();
    // ADR 0007: encoded files (auto-detected and hinted) answer alike on
    // every backend and configuration.
    for fx in enc_fixtures() {
        let opts = IndexOptions {
            encoding: fx.hint,
            ..Default::default()
        };
        let path = fx.path.replace(".toy", ".txt");
        s.index_bytes_opts("o2", "enc", &path, &fx.bytes, None, None, opts)
            .unwrap();
    }
    // And a lossy one (a `utf-8` hint on windows-1252 bytes), so the
    // catalog's lossy count and the hits' `lossy` are compared too.
    let lossy = IndexOptions {
        encoding: Some(encoding_rs::UTF_8),
        ..Default::default()
    };
    let st = s
        .index_bytes_opts(
            "o2",
            "enc",
            "lossy.txt",
            &legacy("CustomerId caf\u{e9}\n", encoding_rs::WINDOWS_1252),
            None,
            None,
            lossy,
        )
        .unwrap();
    assert!(st.lossy);
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
            ..Default::default()
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
                owner: None,
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

/// #74: a store opened without an extractor that indexed stored files names
/// that language and the stored extractor version. A client backend reports
/// its server's gaps (#165: the server parses, so its registry decides), so
/// every backend answers the same.
fn extractor_gaps_name_a_missing_extractor(h: &Harness) {
    let s = (h.open)(vec![Box::new(ToyExtractor)]).expect("open store");
    s.index_bytes("o", "r", "a.toy", b"alpha beta\n", None)
        .unwrap();
    s.index_bytes("o", "r2", "b.toy", b"gamma\n", None).unwrap();
    // Pre-extracted symbols (no fingerprint) never count as a gap.
    s.ingest_file("o", "r", "lib.rs", "rust", &rust_extraction(RUST))
        .unwrap();
    assert_eq!(s.extractor_gaps(None, None).unwrap(), vec![]);
    drop(s);
    let s = open(h);
    let gaps = s.extractor_gaps(None, None).unwrap();
    let scoped = s.extractor_gaps(Some("o"), Some("r2")).unwrap();
    let unknown = s.extractor_gaps(Some("nope"), None).unwrap();
    let before = s.search(&Query::new("alpha")).unwrap();
    let g = |repo: &str| crate::ExtractorGap {
        org: "o".into(),
        repo: repo.into(),
        language: "toylang".into(),
        stored_version: "toy-1".into(),
        symbols: 1,
    };
    assert_eq!(gaps, vec![g("r"), g("r2")]);
    assert_eq!(scoped, vec![g("r2")]);
    let msg = gaps[0].to_string();
    assert!(msg.contains("toylang") && msg.contains("toy-1"), "{msg}");
    assert!(unknown.is_empty());
    // Asking changes nothing.
    assert_eq!(s.search(&Query::new("alpha")).unwrap(), before);
    // Once re-indexed without the extractor, no symbols remain: no gap.
    s.index_bytes_opts(
        "o",
        "r2",
        "b.toy",
        b"gamma\n",
        Some("toylang"),
        None,
        IndexOptions {
            reindex: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(s.extractor_gaps(Some("o"), Some("r2")).unwrap().is_empty());
}

/// #90: `space_usage`, where a backend reports it, is consistent (live
/// data within the file) and changes no read.
fn space_usage_is_consistent(h: &Harness) {
    let s = open(h);
    s.index_bytes("o", "r", "a.txt", b"alpha beta", None)
        .unwrap();
    let before = s.search(&Query::new("alpha")).unwrap();
    if let Some(u) = s.space_usage().unwrap() {
        assert!(u.live_bytes > 0 && u.live_bytes <= u.file_bytes, "{u:?}");
    }
    assert_eq!(s.search(&Query::new("alpha")).unwrap(), before);
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

/// `index_prepared_counted` stores and answers exactly as `index_prepared`
/// does, and reports at least one commit per call (#89); the count itself
/// depends on the chunk size, so only its lower bound is portable.
fn prepared_counted_matches_uncounted(h: &Harness) {
    let s = (h.open)(vec![]).expect("open store");
    let f = |p, b: &'static [u8]| BatchFile {
        path: p,
        bytes: b,
        language: None,
        origin: Some(ORIGIN_DIRECTORY),
        ..Default::default()
    };
    let files = [f("a.txt", b"foo bar\n"), f("b.txt", b"baz foo\n")];
    let d = IndexOptions::default();
    let prep = |repo| {
        files
            .iter()
            .map(|file| s.prepare("o", repo, file, d).unwrap())
            .collect::<Vec<_>>()
    };
    let plain = s.index_prepared("o", "a", prep("a"), d).unwrap();
    let (counted, commits) = s.index_prepared_counted("o", "b", prep("b"), d).unwrap();
    assert!(commits >= 1, "a call commits at least once");
    let (x, y): (Vec<_>, Vec<_>) = (
        plain.iter().map(stats_proj).collect(),
        counted.iter().map(stats_proj).collect(),
    );
    assert_eq!(x, y, "per-file outcomes");
    let toks = |repo| s.file_tokens("o", repo, "b.txt").unwrap().map(|v| v.len());
    assert_eq!(toks("a"), toks("b"));
    // An empty call still answers (and commits nothing visible).
    let (none, _) = s.index_prepared_counted("o", "c", vec![], d).unwrap();
    assert!(none.is_empty());
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
        ..Default::default()
    };
    let files = [
        f("src/lib.rs", lib, Some("rust")),
        f("./notes.md", b"# foo\nbar (foo)\n", None),
        f("bad.c", b"bad", Some("conf-bad")),
        f("bin.dat", b"\xff\xfe", None),
        f("m.txt", b"foo mfoo m\n", None),
        f("src/lib.rs", b"fn dup() {}\n", Some("rust")),
    ];
    for opts in [
        IndexOptions::default(),
        IndexOptions {
            reindex: true,
            ..Default::default()
        },
    ] {
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
/// A remote backend (`accepts_remote_prepared`) defers the check and the
/// extraction to the server, so its prepared file never says `unchanged`
/// and the extractor runs at commit; the stored outcome and the number of
/// extractions are the same either way, so those are asserted after each
/// commit.
fn prepare_skips_unchanged(h: &Harness) {
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let s = (h.open)(vec![Box::new(Counting(n.clone()))]).expect("open store");
    let calls = || n.load(std::sync::atomic::Ordering::SeqCst);
    let local = !h.accepts_remote_prepared;
    let file = |origin| BatchFile {
        path: "a.cnt",
        bytes: b"foo bar",
        language: Some("conf-count"),
        origin,
        ..Default::default()
    };
    let first = prepare_all(&*s, "r", &[file(None)], IndexOptions::default());
    assert!(!first[0].is_unchanged());
    assert_eq!(first[0].bytes_len(), 7);
    if local {
        assert_eq!(calls(), 1, "extracted at prepare");
    }
    // Nothing is stored until the commit.
    assert!(s.file_tokens("o", "r", "a.cnt").unwrap().is_none());
    let out = s
        .index_prepared("o", "r", first, IndexOptions::default())
        .unwrap();
    assert!(!out[0].as_ref().unwrap().unchanged);
    assert_eq!(calls(), 1, "extracted exactly once");

    let again = prepare_all(
        &*s,
        "r",
        &[file(Some(ORIGIN_DIRECTORY))],
        IndexOptions::default(),
    );
    if local {
        assert!(again[0].is_unchanged());
    }
    assert_eq!(calls(), 1, "unchanged file not extracted");
    let out = s
        .index_prepared("o", "r", again, IndexOptions::default())
        .unwrap();
    assert!(out[0].as_ref().unwrap().unchanged);
    assert_eq!(calls(), 1, "unchanged file not extracted at commit either");
    let origin = s.file_tokens("o", "r", "a.cnt").unwrap().unwrap()[0].parent;
    let f = s.get(origin.unwrap()).unwrap().unwrap();
    assert_eq!(
        f.origin.as_deref(),
        Some(ORIGIN_DIRECTORY),
        "origin refreshed"
    );

    let forced = prepare_all(
        &*s,
        "r",
        &[file(None)],
        IndexOptions {
            reindex: true,
            ..Default::default()
        },
    );
    assert!(!forced[0].is_unchanged());
    if local {
        assert_eq!(calls(), 2, "reindex extracts");
    }
    let out = s
        .index_prepared(
            "o",
            "r",
            forced,
            IndexOptions {
                reindex: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(out[0].as_ref().unwrap().replaced);
    assert_eq!(calls(), 2, "reindex extracts exactly once more");
}

/// `prepare_with` against one `fingerprint_snapshot` per batch (#172)
/// stores exactly what `prepare` would: unchanged files are skipped (a
/// local backend says so at prepare time), changed and new ones stored, and
/// a snapshot gone stale before the commit, or one of another repo, still
/// ends in the right outcome.
fn prepare_with_snapshot(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    let local = !h.accepts_remote_prepared;
    s.index_batch("o", "r", &[bf("a.txt", b"alpha"), bf("b.txt", b"beta")], d)
        .unwrap();
    let snap = s.fingerprint_snapshot("o", "r").unwrap();
    if local {
        assert_eq!(snap.len(), 2);
    }
    let files = [
        bf("a.txt", b"alpha"),
        bf("b.txt", b"beta CHANGED"),
        bf("c.txt", b"gamma"),
    ];
    let p: Vec<_> = files
        .iter()
        .map(|f| s.prepare_with("o", "r", f, d, &snap).unwrap())
        .collect();
    if local {
        assert!(p[0].is_unchanged() && !p[1].is_unchanged() && !p[2].is_unchanged());
    }
    // Stale: `a.txt` changes after the snapshot, before the commit.
    s.index_bytes("o", "r", "a.txt", b"alpha NEW", Some("text"))
        .unwrap();
    let out = s.index_prepared("o", "r", p, d).unwrap();
    let st: Vec<_> = out.iter().map(|r| r.as_ref().unwrap()).collect();
    assert!(st[0].replaced && !st[0].unchanged, "{:?}", st[0]);
    assert!(st[1].replaced, "{:?}", st[1]);
    assert!(!st[2].replaced && !st[2].unchanged, "{:?}", st[2]);
    for (q, n) in [("alpha", 1), ("NEW", 0), ("CHANGED", 1), ("gamma", 1)] {
        assert_eq!(s.search(&Query::new(q)).unwrap().len(), n, "{q}");
    }
    // Paths in subdirectories key the snapshot by their stored
    // (normalized, `/`-separated) path, whatever separator the caller sent.
    s.index_batch("o", "r", &[bf("a/b/c.txt", b"deep")], d)
        .unwrap();
    let snap = s.fingerprint_snapshot("o", "r").unwrap();
    for path in ["a/b/c.txt", "a\\b\\c.txt"] {
        let p = s
            .prepare_with("o", "r", &bf(path, b"deep"), d, &snap)
            .unwrap();
        if local {
            assert!(p.is_unchanged(), "{path}");
        }
        let out = s.index_prepared("o", "r", vec![p], d).unwrap();
        assert!(out[0].as_ref().unwrap().unchanged, "{path}");
    }
    let p = s
        .prepare_with("o", "r", &bf("a/b/c.txt", b"deeper"), d, &snap)
        .unwrap();
    assert!(!p.is_unchanged());
    // Another repo's snapshot: the same outcome as a plain `prepare`.
    let other = s.fingerprint_snapshot("o", "elsewhere").unwrap();
    let p = s
        .prepare_with("o", "r", &bf("c.txt", b"gamma"), d, &other)
        .unwrap();
    if local {
        assert!(p.is_unchanged());
    }
    let out = s.index_prepared("o", "r", vec![p], d).unwrap();
    assert!(out[0].as_ref().unwrap().unchanged);
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
        ..Default::default()
    };
    let files = [
        f("ok1.c", b"xxxx", Some("text")),
        f("bad.c", b"bad", Some("conf-bad")),
        f("bin.c", b"\x00\xff\x00", None),
        f("ok2.c", b"yyyy", Some("text")),
    ];
    let p = prepare_all(&*s, "r", &files, IndexOptions::default());
    let out = s
        .index_prepared("o", "r", p, IndexOptions::default())
        .unwrap();
    assert_eq!(out.len(), 4);
    assert!(out[0].is_ok() && out[3].is_ok());
    assert!(matches!(&out[1], Err(StoreError::InvalidSpan(m)) if m.contains("bad.c")));
    assert!(matches!(&out[2], Err(StoreError::Binary(m)) if m.contains("bin.c")));
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
    // A remote backend defers the check to the server (never `unchanged`
    // at prepare time); the commit-time outcome below is the same.
    if !h.accepts_remote_prepared {
        assert!(p[0].is_unchanged());
    }
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

/// Stored paths are `/`-separated whatever the caller sent (#100, #120): a
/// `\` path from a Windows walk or client is the same file as its `/` twin
/// on every write path (batch, prepared, bytes, pre-extracted ingest), and
/// the language is read from the last component on either separator, so
/// `dir\.gitignore` is `unknown`, not `gitignore`.
fn backslash_paths(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    let auto = |path| BatchFile {
        path,
        bytes: b"foo",
        language: None,
        origin: Some(ORIGIN_DIRECTORY),
        ..Default::default()
    };
    s.index_batch(
        "o",
        "r",
        &[auto(r"dir\.gitignore"), bf(r"src\a.txt", b"foo")],
        d,
    )
    .unwrap();
    let p = prepare_all(&*s, "r", &[bf(r"src\b.txt", b"foo")], d);
    s.index_prepared("o", "r", p, d).unwrap();
    s.index_bytes("o", "r", r".\src\c.txt", b"foo", Some("text"))
        .unwrap();
    s.ingest_file("o", "r", r"src\lib.rs", "rust", &rust_extraction(RUST))
        .unwrap();
    for f in [
        "dir/.gitignore",
        "src/a.txt",
        "src/b.txt",
        "src/c.txt",
        "src/lib.rs",
    ] {
        assert!(s.file_tokens("o", "r", f).unwrap().is_some(), "{f}");
        let back = f.replace('/', "\\");
        assert!(s.file_tokens("o", "r", &back).unwrap().is_some(), "{back}");
    }
    let info = &s.describe(Some("o"), Some("r")).unwrap()[0];
    assert_eq!(info.files, 5, "{info:?}");
    let langs: Vec<_> = info.languages.keys().cloned().collect();
    assert_eq!(langs, ["rust", "text", "unknown"], "{info:?}");
    // The `/` twin of an indexed `\` path is the same, unchanged file.
    let st = s
        .index_batch("o", "r", &[bf("src/a.txt", b"foo")], d)
        .unwrap();
    assert!(st[0].as_ref().unwrap().unchanged, "{st:?}");
    let mut files: Vec<_> = s
        .search(&Query::new("foo"))
        .unwrap()
        .into_iter()
        .filter_map(|h| h.file)
        .collect();
    files.sort();
    files.dedup();
    assert!(files.iter().all(|f| !f.contains('\\')), "{files:?}");
    let mut q = SymbolQuery::new("*");
    q.file = Some(r"src\lib.rs".into());
    assert_eq!(s.search_symbols(&q).unwrap().len(), 3);
}

/// A keep set spelled with `\` (a Windows caller) keeps the same files as
/// its `/` twin; it must not match nothing and prune everything.
fn prune_backslash_keep(h: &Harness) {
    let s = open(h);
    let d = IndexOptions::default();
    let files = [bf("src/a.txt", b"foo"), bf("src/b.txt", b"foo")];
    s.index_batch("o", "r", &files, d).unwrap();
    let keep: HashSet<String> = [r"src\a.txt".to_string()].into();
    assert_eq!(s.prune_files("o", "r", &keep, true).unwrap(), ["src/b.txt"]);
    assert_eq!(
        s.prune_files("o", "r", &keep, false).unwrap(),
        ["src/b.txt"]
    );
    assert!(s.file_tokens("o", "r", "src/a.txt").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "src/b.txt").unwrap().is_none());
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

/// A source where methods name their type by an owner hint instead of
/// nesting in it (issue #137; the shape of a Go receiver method), in a
/// language the store knows nothing about.
const OWNED: &str = "type T struct { foo int }
                     func (t T) m() { foo() }
                     func free() { foo() }
                     func (x Missing) n() { foo() }
                     func (t free) o() { foo() }
                     type U struct { k() { foo() } }
";

fn owner_extraction() -> Extraction {
    let d = |name: &str, kind, lang_kind: &str, needle: &str, owner: Option<&str>| SymbolDecl {
        name: name.into(),
        kind,
        lang_kind: Some(lang_kind.into()),
        span: span_of(OWNED, needle),
        owner: owner.map(Into::into),
    };
    Extraction {
        has_errors: false,
        symbols: vec![
            d(
                "T",
                SymbolKind::Type,
                "struct",
                "type T struct { foo int }",
                None,
            ),
            d(
                "m",
                SymbolKind::Method,
                "method",
                "func (t T) m() { foo() }",
                Some("T"),
            ),
            d(
                "free",
                SymbolKind::Function,
                "func",
                "func free() { foo() }",
                None,
            ),
            // An owner with no symbol of that name in the file.
            d(
                "n",
                SymbolKind::Method,
                "method",
                "func (x Missing) n() { foo() }",
                Some("Missing"),
            ),
            // An owner naming a symbol that is not type-like.
            d(
                "o",
                SymbolKind::Method,
                "method",
                "func (t free) o() { foo() }",
                Some("free"),
            ),
            d(
                "U",
                SymbolKind::Type,
                "struct",
                "type U struct { k() { foo() } }",
                None,
            ),
            // Nested by span in `U`: the enclosing type wins over the hint.
            d(
                "k",
                SymbolKind::Method,
                "method",
                "k() { foo() }",
                Some("T"),
            ),
        ],
        tokens: tokenize(OWNED),
    }
}

/// Issue #137: under the class grain, a hit that no type-like symbol
/// encloses by span (whatever the kind filter) rolls up under the type its owner hint names in the
/// same file (full span, qualified name, kind filter applied to the type);
/// an unresolvable hint is `no_matching_symbol`; span nesting wins over a
/// hint; the other grains ignore hints; `search_symbols` reports them; and a
/// hint survives a replace and a reopen.
fn owner_hint_class_grain(h: &Harness) {
    let s = open(h);
    s.ingest_file("o", "r", "own.toy", "toy", &owner_extraction())
        .unwrap();
    type Row = (Option<String>, usize, Option<Span>, bool);
    let rows = |s: &dyn Store, q: &Query| -> Vec<Row> {
        s.search(q)
            .unwrap()
            .into_iter()
            .map(|h| (h.symbol, h.count, h.span, h.no_matching_symbol))
            .collect()
    };
    let t = Some(span_of(OWNED, "type T struct { foo int }"));
    let u = Some(span_of(OWNED, "type U struct { k() { foo() } }"));
    let mut q = Query::new("foo");
    q.grain = Grain::Class;
    let want: Vec<Row> = vec![
        // `free`, `n` (unknown owner) and `o` (owner is a function).
        (None, 3, None, true),
        // The field and the body of `m`, rolled up under `T`.
        (Some("T".into()), 2, t, false),
        (Some("U".into()), 1, u, false),
    ];
    assert_eq!(rows(&*s, &q), want);
    let hit = &s.search(&q).unwrap()[1];
    assert_eq!(hit.symbol_kind, Some(SymbolKind::Type));
    assert_eq!(hit.lang_kind.as_deref(), Some("struct"));

    // The kind filter applies to the owner type.
    q.symbol_kind = Some("struct".into());
    assert_eq!(rows(&*s, &q), want);
    q.symbol_kind = Some("interface".into());
    assert_eq!(rows(&*s, &q), [(None, 6, None, true)]);
    q.symbol_kind = None;

    // The method grain ignores hints: `m` is its own row, unqualified.
    q.grain = Grain::Method;
    let got: Vec<_> = s
        .search(&q)
        .unwrap()
        .into_iter()
        .map(|h| h.symbol)
        .collect();
    assert!(got.contains(&Some("m".into())), "{got:?}");
    assert!(got.contains(&Some("U::k".into())), "{got:?}");

    // `search_symbols` reports the hint.
    let owner_of = |s: &dyn Store, n: &str| {
        s.search_symbols(&SymbolQuery::new(n)).unwrap()[0]
            .owner
            .clone()
    };
    assert_eq!(owner_of(&*s, "m").as_deref(), Some("T"));
    assert_eq!(owner_of(&*s, "k").as_deref(), Some("T"));
    assert_eq!(owner_of(&*s, "T"), None);

    // A hint survives a reopen, and a replace without hints drops it.
    drop(s);
    let s = open(h);
    q.grain = Grain::Class;
    assert_eq!(rows(&*s, &q), want);
    let mut plain_ex = owner_extraction();
    for d in &mut plain_ex.symbols {
        d.owner = None;
    }
    s.ingest_file("o", "r", "own.toy", "toy", &plain_ex)
        .unwrap();
    assert_eq!(owner_of(&*s, "m"), None);
    assert_eq!(
        rows(&*s, &q),
        [
            (None, 4, None, true),
            (Some("T".into()), 1, t, false),
            (Some("U".into()), 1, u, false),
        ]
    );
    // Nested symbols that both carry owners: the innermost one that
    // resolves wins; an unresolvable inner owner falls through to the outer.
    // A hit inside a type that the kind filter rejects does not fall back to
    // an owner hint: the hint is only for hits with no enclosing type.
    const NEST: &str = "type A struct {}\ntype B struct {}\nf() { g() { foo } h() { foo } }\ntype C interface { i() { foo } }\n";
    let n = |name: &str, kind, lk: Option<&str>, needle: &str, owner: Option<&str>| {
        let d = SymbolDecl::new(name, kind, lk.map(Into::into), span_of(NEST, needle));
        match owner {
            Some(o) => d.with_owner(o),
            None => d,
        }
    };
    let (ty, fun) = (SymbolKind::Type, SymbolKind::Function);
    let nest = Extraction {
        has_errors: false,
        symbols: vec![
            n("A", ty, Some("struct"), "type A struct {}", None),
            n("B", ty, Some("struct"), "type B struct {}", None),
            n("f", fun, None, "f() { g() { foo } h() { foo } }", Some("A")),
            n("g", fun, None, "g() { foo }", Some("B")),
            n("h", fun, None, "h() { foo }", Some("Nope")),
            n(
                "C",
                ty,
                Some("interface"),
                "type C interface { i() { foo } }",
                None,
            ),
            n("i", fun, None, "i() { foo }", Some("A")),
        ],
        tokens: tokenize(NEST),
    };
    s.ingest_file("o", "r2", "nest.toy", "toy", &nest).unwrap();
    let mut nq = q.clone();
    nq.grain = Grain::Class;
    nq.repo = Some("r2".into());
    let (a, b, c) = (
        Some(span_of(NEST, "type A struct {}")),
        Some(span_of(NEST, "type B struct {}")),
        Some(span_of(NEST, "type C interface { i() { foo } }")),
    );
    assert_eq!(
        rows(&*s, &nq),
        [
            (Some("A".into()), 1, a, false),
            (Some("B".into()), 1, b, false),
            (Some("C".into()), 1, c, false),
        ]
    );
    nq.symbol_kind = Some("struct".into());
    assert_eq!(
        rows(&*s, &nq),
        [
            (None, 1, None, true),
            (Some("A".into()), 1, a, false),
            (Some("B".into()), 1, b, false),
        ]
    );
}

// --- ADR 0007: source encodings (epic story 41) ---

/// A PNG header: NULs, no BOM, not UTF-16, so binary (ADR 0007 C5).
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

/// The shared identifiers every encoded fixture carries (ADR 0007 C9).
const ENC_ID: &str = "CustomerId";
const ENC_LATIN: &str = "café";
const ENC_CJK: &str = "日本";

fn utf16(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
    let units = bom
        .then_some(0xFEFF_u16)
        .into_iter()
        .chain(text.encode_utf16());
    if big_endian {
        units.flat_map(u16::to_be_bytes).collect()
    } else {
        units.flat_map(u16::to_le_bytes).collect()
    }
}

fn legacy(text: &str, e: &'static encoding_rs::Encoding) -> Vec<u8> {
    let (bytes, _, unmappable) = e.encode(text);
    assert!(!unmappable, "{} cannot encode {text:?}", e.name());
    bytes.into_owned()
}

/// One encoded fixture: its path, raw bytes, the hint to send, and what the
/// store must record and decode (ADR 0007 C1, C3, C6).
struct EncFixture {
    path: &'static str,
    bytes: Vec<u8>,
    hint: Option<&'static encoding_rs::Encoding>,
    /// The WHATWG name recorded on the File node (`None`: UTF-8).
    encoding: Option<&'static str>,
    decoded: String,
    ids: &'static [&'static str],
}

/// UTF-8, UTF-16LE (BOM), UTF-16BE (BOM-less, sniffed), windows-1252 and
/// Shift_JIS (auto-detected), and a windows-1252 file sent with a hint: the
/// same identifiers in each (Latin only in windows-1252, CJK only in
/// Shift_JIS, which cannot encode the other).
fn enc_fixtures() -> Vec<EncFixture> {
    let both = format!("{ENC_ID} {ENC_LATIN} {ENC_CJK}\n");
    let latin = format!(
        "// Kundennummer für das Café: crème brûlée, naïve résumé, déjà vu, à la carte\n{ENC_ID} {ENC_LATIN}\n"
    );
    let cjk =
        format!("// 顧客番号のクラスです。日本語のコメントを含みます。\n{ENC_ID} {ENC_CJK}\n");
    let fx = |path, bytes, hint, encoding, decoded: &str, ids| EncFixture {
        path,
        bytes,
        hint,
        encoding,
        decoded: decoded.to_string(),
        ids,
    };
    vec![
        fx(
            "u8.toy",
            both.clone().into_bytes(),
            None,
            None,
            &both,
            &[ENC_ID, ENC_LATIN, ENC_CJK],
        ),
        fx(
            "le.toy",
            utf16(&both, false, true),
            None,
            Some("UTF-16LE"),
            &format!("\u{feff}{both}"),
            &[ENC_ID, ENC_LATIN, ENC_CJK],
        ),
        fx(
            "be.toy",
            utf16(&both, true, false),
            None,
            Some("UTF-16BE"),
            &both,
            &[ENC_ID, ENC_LATIN, ENC_CJK],
        ),
        fx(
            "w1252.toy",
            legacy(&latin, encoding_rs::WINDOWS_1252),
            None,
            Some("windows-1252"),
            &latin,
            &[ENC_ID, ENC_LATIN],
        ),
        fx(
            "sjis.toy",
            legacy(&cjk, encoding_rs::SHIFT_JIS),
            None,
            Some("Shift_JIS"),
            &cjk,
            &[ENC_ID, ENC_CJK],
        ),
        fx(
            "hinted.toy",
            legacy(
                &format!("{ENC_ID} {ENC_LATIN}\n"),
                encoding_rs::WINDOWS_1252,
            ),
            Some(encoding_rs::WINDOWS_1252),
            Some("windows-1252"),
            &format!("{ENC_ID} {ENC_LATIN}\n"),
            &[ENC_ID, ENC_LATIN],
        ),
    ]
}

/// The stored File node of `org/repo/path`.
fn file_node(s: &dyn Store, stats: &crate::IngestStats) -> Node {
    let f = s.get(stats.file_id).unwrap().expect("file node");
    assert_eq!(f.kind, NodeKind::File);
    f
}

/// Every stored token of `path` is exactly what the tokenizer gives for the
/// decoded text (text, byte range, line, column, class), and its byte range
/// slices that text out of the decoded source (ADR 0007 C1).
fn assert_exact_against_decoded(s: &dyn Store, repo: &str, path: &str, decoded: &str) {
    let stored: Vec<_> = s
        .file_tokens("o", repo, path)
        .unwrap()
        .expect("file stored")
        .into_iter()
        .map(|n| (n.name, n.span.expect("token span"), n.token_class))
        .collect();
    let want: Vec<_> = tokenize(decoded)
        .into_iter()
        .map(|t| (t.text, t.span, Some(t.class)))
        .collect();
    assert_eq!(stored, want, "{repo}/{path}: tokens of the decoded text");
    for (text, span, _) in &stored {
        assert_eq!(
            &decoded[span.start as usize..span.end as usize],
            text,
            "{repo}/{path}"
        );
    }
    // The toy extractor's one symbol spans the decoded body.
    let mut q = SymbolQuery::new("whole");
    q.repo = Some(repo.into());
    q.file = Some(path.into());
    let hits = s.search_symbols(&q).unwrap();
    assert_eq!(hits.len(), 1, "{repo}/{path}: {hits:?}");
    assert_eq!(
        hits[0].span,
        Some(span_of(decoded, decoded.trim_end())),
        "{repo}/{path}: symbol span"
    );
}

/// Encoded files through every write entry point (`index_bytes_opts`,
/// `index_batch`, `prepare` + `index_prepared`) are decoded the same way:
/// the File node records `encoding` (absent for UTF-8) and `lossy`, tokens
/// and symbols are exact against the decoded text, the fingerprint carries
/// the encoding suffix for non-UTF-8 files only, and the same identifiers
/// match across encodings (ADR 0007 C1, C2, C6, C7, C9).
fn encoded_files(h: &Harness) {
    let s = (h.open)(vec![Box::new(ToyExtractor)]).expect("open store");
    let fixtures = enc_fixtures();
    let mut fingerprints = Vec::new();
    for fx in &fixtures {
        let opts = IndexOptions {
            encoding: fx.hint,
            ..Default::default()
        };
        let one = s
            .index_bytes_opts("o", "single", fx.path, &fx.bytes, None, None, opts)
            .unwrap();
        let file = BatchFile {
            path: fx.path,
            bytes: &fx.bytes,
            encoding: fx.hint,
            ..Default::default()
        };
        let batch = s
            .index_batch("o", "batch", &[file], IndexOptions::default())
            .unwrap()
            .remove(0)
            .unwrap();
        let prepared = prepare_all(&*s, "prepared", &[file], IndexOptions::default());
        let prepared = s
            .index_prepared("o", "prepared", prepared, IndexOptions::default())
            .unwrap()
            .remove(0)
            .unwrap();
        let mut seen = Vec::new();
        for (repo, st) in [("single", &one), ("batch", &batch), ("prepared", &prepared)] {
            assert_eq!(st.language, "toylang", "{repo}/{}", fx.path);
            let f = file_node(&*s, st);
            assert_eq!(f.encoding.as_deref(), fx.encoding, "{repo}/{}", fx.path);
            assert!(
                !f.lossy,
                "{repo}/{}: auto and fitting hints are never lossy",
                fx.path
            );
            let fp = f.fingerprint.clone().expect("fingerprint");
            match fx.encoding {
                None => assert!(!fp.contains("|enc="), "{fp}"),
                Some(name) => assert!(
                    fp.ends_with(&format!(
                        "|enc={name}@{}",
                        graph_core::encoding::DECODER_VERSION
                    )),
                    "{fp}"
                ),
            }
            assert_exact_against_decoded(&*s, repo, fx.path, &fx.decoded);
            seen.push(fp);
        }
        assert!(
            seen.iter().all(|fp| *fp == seen[0]),
            "{}: every write path fingerprints alike: {seen:?}",
            fx.path
        );
        fingerprints.push(seen.remove(0));
        // Unchanged bytes and hint: a no-op on every path.
        let again = s
            .index_bytes_opts("o", "single", fx.path, &fx.bytes, None, None, opts)
            .unwrap();
        assert!(again.unchanged, "{}", fx.path);
    }
    // One search per identifier finds every file that holds it, whatever
    // its source encoding (ADR 0007 C9).
    for id in [ENC_ID, ENC_LATIN, ENC_CJK] {
        let mut q = Query::new(id);
        q.repo = Some("single".into());
        q.grain = Grain::File;
        let mut files: Vec<String> = s
            .search(&q)
            .unwrap()
            .into_iter()
            .filter_map(|h| h.file)
            .collect();
        files.sort();
        let mut want: Vec<String> = fixtures
            .iter()
            .filter(|fx| fx.ids.contains(&id))
            .map(|fx| fx.path.to_string())
            .collect();
        want.sort();
        assert_eq!(files, want, "search {id}");
    }
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

/// Hints, BOMs, lossy decodes, `strict_encoding`, binary files and language
/// detection on the decoded text behave the same on every backend (ADR 0007
/// C3, C5, C7, C8).
fn encoding_hint_strict_and_binary(h: &Harness) {
    let s = open(h);
    let meta = |st: &crate::IngestStats| {
        let f = file_node(&*s, st);
        (f.encoding, f.lossy, f.fingerprint.unwrap_or_default())
    };
    let hinted = |e: &'static encoding_rs::Encoding, strict: bool| IndexOptions {
        encoding: Some(e),
        strict_encoding: strict,
        ..Default::default()
    };
    // A BOM wins over a hint: still UTF-16LE, U+FEFF kept.
    let le = utf16("alpha beta\n", false, true);
    let st = s
        .index_bytes_opts(
            "o",
            "r",
            "bom.txt",
            &le,
            None,
            None,
            hinted(encoding_rs::WINDOWS_1252, true),
        )
        .unwrap();
    assert_eq!(meta(&st).0.as_deref(), Some("UTF-16LE"));
    let toks = s.file_tokens("o", "r", "bom.txt").unwrap().unwrap();
    assert_eq!(toks[0].name, "alpha");
    assert_eq!(
        toks[0].span.unwrap().start,
        3,
        "U+FEFF (3 bytes) stays first"
    );

    // A hint that does not fit the bytes: lossy, recorded and fingerprinted.
    let latin = legacy("caf\u{e9} au lait\n", encoding_rs::WINDOWS_1252);
    let st = s
        .index_bytes_opts(
            "o",
            "r",
            "lossy.txt",
            &latin,
            None,
            None,
            hinted(encoding_rs::UTF_8, false),
        )
        .unwrap();
    let (enc, lossy, fp) = meta(&st);
    assert_eq!((enc, lossy), (None, true), "lossy UTF-8");
    assert!(
        fp.ends_with(&format!(
            "|enc=UTF-8+lossy@{}",
            graph_core::encoding::DECODER_VERSION
        )),
        "{fp}"
    );
    let toks = s.file_tokens("o", "r", "lossy.txt").unwrap().unwrap();
    assert!(toks.iter().any(|t| t.name.contains('\u{fffd}')), "{toks:?}");
    // The same bytes auto-detected: windows-1252, not lossy, and a
    // different fingerprint, so the file is re-indexed (not unchanged).
    let st = s.index_bytes("o", "r", "lossy.txt", &latin, None).unwrap();
    assert!(!st.unchanged && st.replaced, "a changed decode re-indexes");
    let (enc, lossy, fp2) = meta(&st);
    assert_eq!((enc.as_deref(), lossy), (Some("windows-1252"), false));
    assert_ne!(fp, fp2);
    assert_eq!(s.search(&Query::new("caf\u{e9}")).unwrap().len(), 1);
    assert!(
        s.index_bytes("o", "r", "lossy.txt", &latin, None)
            .unwrap()
            .unchanged
    );

    // strict_encoding refuses a lossy decode and stores nothing: today's
    // `NotUtf8` for UTF-8, a typed `StrictEncoding` naming the encoding
    // otherwise (#180: no message matching, on any backend).
    for (path, bytes, e) in [
        ("s1.txt", latin.clone(), encoding_rs::UTF_8),
        ("s2.txt", b"ok \x82".to_vec(), encoding_rs::SHIFT_JIS),
    ] {
        let r = s.index_bytes_opts("o", "r", path, &bytes, None, None, hinted(e, true));
        match (&r, e == encoding_rs::UTF_8) {
            (Err(StoreError::NotUtf8(m)), true) => assert!(m.contains(path), "{m}"),
            (
                Err(StoreError::StrictEncoding {
                    path: p,
                    encoding: enc,
                }),
                false,
            ) => assert_eq!((p.as_str(), enc.as_str()), (path, "Shift_JIS")),
            _ => panic!("{path}: {r:?}"),
        }
        assert!(crate::is_strict_encoding_refusal(r.as_ref().unwrap_err()));
        assert!(s.file_tokens("o", "r", path).unwrap().is_none());
    }
    // Strict in a batch is per file: the others are stored.
    let files = [
        BatchFile {
            path: "b1.txt",
            bytes: &latin,
            encoding: Some(encoding_rs::UTF_8),
            strict_encoding: true,
            ..Default::default()
        },
        BatchFile {
            path: "b2.txt",
            bytes: b"plain words\n",
            strict_encoding: true,
            ..Default::default()
        },
        BatchFile {
            path: "b3.png",
            bytes: PNG,
            ..Default::default()
        },
        BatchFile {
            path: "b4.txt",
            bytes: &le,
            ..Default::default()
        },
    ];
    let out = s
        .index_batch("o", "r", &files, IndexOptions::default())
        .unwrap();
    assert!(matches!(&out[0], Err(StoreError::NotUtf8(_))), "{out:?}");
    assert!(out[1].is_ok() && out[3].is_ok(), "{out:?}");
    assert!(
        matches!(&out[2], Err(StoreError::Binary(m)) if m.contains("b3.png")),
        "{out:?}"
    );
    // A batch's non-UTF-8 strict refusal is typed too, in its own slot.
    let sjis = [
        BatchFile {
            path: "b5.txt",
            bytes: b"ok \x82",
            encoding: Some(encoding_rs::SHIFT_JIS),
            strict_encoding: true,
            ..Default::default()
        },
        BatchFile {
            path: "b6.txt",
            bytes: b"fine\n",
            ..Default::default()
        },
    ];
    let out = s
        .index_batch("o", "r", &sjis, IndexOptions::default())
        .unwrap();
    assert!(
        matches!(&out[0], Err(StoreError::StrictEncoding { path, encoding })
            if path == "b5.txt" && encoding == "Shift_JIS"),
        "{out:?}"
    );
    assert!(out[1].is_ok(), "{out:?}");
    let prepared = prepare_all(&*s, "r", &files[2..3], IndexOptions::default());
    let out = s
        .index_prepared("o", "r", prepared, IndexOptions::default())
        .unwrap();
    assert!(matches!(&out[0], Err(StoreError::Binary(_))), "{out:?}");
    assert!(matches!(
        s.index_bytes("o", "r", "img.png", PNG, None),
        Err(StoreError::Binary(_))
    ));
    // An explicit UTF-16 hint makes a NUL-bearing file text.
    let odd = utf16("x\0y\n", false, false);
    assert!(matches!(
        s.index_bytes("o", "r", "odd.txt", &odd, None),
        Err(StoreError::Binary(_))
    ));
    let st = s
        .index_bytes_opts(
            "o",
            "r",
            "odd.txt",
            &odd,
            None,
            None,
            hinted(encoding_rs::UTF_16LE, false),
        )
        .unwrap();
    assert_eq!(meta(&st).0.as_deref(), Some("UTF-16LE"));
    assert!(s.file_tokens("o", "r", "img.png").unwrap().is_none());
    assert!(s.file_tokens("o", "r", "b3.png").unwrap().is_none());

    // The language is detected from the decoded text: a UTF-16 script's
    // shebang is seen, BOM or not.
    for (path, bom) in [("tool", true), ("tool2", false)] {
        let st = s
            .index_bytes(
                "o",
                "r",
                path,
                &utf16("#!/usr/bin/env python3\nprint(1)\n", false, bom),
                None,
            )
            .unwrap();
        assert_eq!(st.language, "python", "{path}");
    }
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}

/// Encodings are visible on every read (ADR 0007 C8, epic story 43):
/// `describe` counts non-UTF-8 files per encoding and lossy files per repo
/// (from the catalog, kept in step with replace and prune, and equal to the
/// reference scan); `IngestStats`, `search` hits and `search_symbols` hits
/// carry the file's `encoding`/`lossy` (absent for UTF-8, and on repo and
/// org rows).
fn encoding_exposure(h: &Harness) {
    let s = (h.open)(vec![Box::new(ToyExtractor)]).expect("open store");
    let dir = Some(ORIGIN_DIRECTORY);
    let mut expect_enc = BTreeMap::new();
    for fx in enc_fixtures() {
        let opts = IndexOptions {
            encoding: fx.hint,
            ..Default::default()
        };
        let st = s
            .index_bytes_opts("o", "r", fx.path, &fx.bytes, None, dir, opts)
            .unwrap();
        assert_eq!(st.encoding.as_deref(), fx.encoding, "{}", fx.path);
        assert!(!st.lossy, "{}", fx.path);
        if let Some(e) = fx.encoding {
            *expect_enc.entry(e.to_string()).or_insert(0usize) += 1;
        }
    }
    // A lossy UTF-8 file: lossy, no encoding.
    let latin = legacy("caf\u{e9} lossyword\n", encoding_rs::WINDOWS_1252);
    let utf8_hint = IndexOptions {
        encoding: Some(encoding_rs::UTF_8),
        ..Default::default()
    };
    let st = s
        .index_bytes_opts("o", "r", "lossy.toy", &latin, None, dir, utf8_hint)
        .unwrap();
    assert_eq!((st.encoding.as_deref(), st.lossy), (None, true));
    // An unchanged re-index reports the stored encoding too.
    let le = utf16("again\n", false, true);
    s.index_bytes_opts(
        "o",
        "r",
        "again.toy",
        &le,
        None,
        dir,
        IndexOptions::default(),
    )
    .unwrap();
    let again = s
        .index_bytes_opts(
            "o",
            "r",
            "again.toy",
            &le,
            None,
            dir,
            IndexOptions::default(),
        )
        .unwrap();
    assert!(again.unchanged);
    assert_eq!(again.encoding.as_deref(), Some("UTF-16LE"));
    *expect_enc.entry("UTF-16LE".to_string()).or_insert(0) += 1;
    // Another repo stays separate and all UTF-8.
    s.index_bytes("o", "plain", "a.toy", b"plain\n", None)
        .unwrap();

    let check = |enc: &BTreeMap<String, usize>, lossy: usize, files: usize| {
        let d = s.describe(None, None).unwrap();
        assert_eq!(
            d,
            s.describe_by_scan(None, None).unwrap(),
            "catalog vs scan"
        );
        let r = d.iter().find(|i| i.repo == "r").expect("repo r");
        assert_eq!((&r.encodings, r.lossy, r.files), (enc, lossy, files));
        let p = d.iter().find(|i| i.repo == "plain").expect("repo plain");
        assert!(p.encodings.is_empty() && p.lossy == 0, "{p:?}");
        let scoped = s.describe(Some("o"), Some("r")).unwrap();
        assert_eq!(scoped, s.describe_by_scan(Some("o"), Some("r")).unwrap());
        assert_eq!(scoped[0].encodings, *enc);
    };
    let n = enc_fixtures().len() + 2;
    check(&expect_enc, 1, n);

    // Search hits carry their file's encoding at file-level grains.
    let mut q = Query::new(ENC_ID);
    q.repo = Some("r".into());
    for grain in [Grain::Token, Grain::Symbol, Grain::File] {
        q.grain = grain;
        let hits = s.search(&q).unwrap();
        assert!(!hits.is_empty());
        for hit in &hits {
            let want = enc_fixtures()
                .into_iter()
                .find(|f| Some(f.path) == hit.file.as_deref())
                .expect("fixture")
                .encoding;
            assert_eq!(hit.encoding.as_deref(), want, "{grain:?} {hit:?}");
            assert!(!hit.lossy);
        }
    }
    for grain in [Grain::Repo, Grain::Org] {
        q.grain = grain;
        for hit in s.search(&q).unwrap() {
            assert!(hit.encoding.is_none() && !hit.lossy, "{hit:?}");
        }
    }
    let hits = s.search(&Query::new("lossyword")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!((hits[0].encoding.as_deref(), hits[0].lossy), (None, true));
    // Symbol hits too.
    let mut sq = SymbolQuery::new("whole");
    sq.repo = Some("r".into());
    for hit in s.search_symbols(&sq).unwrap() {
        let (enc, lossy) = match hit.file.as_str() {
            "lossy.toy" => (None, true),
            "again.toy" => (Some("UTF-16LE"), false),
            p => (
                enc_fixtures()
                    .into_iter()
                    .find(|f| f.path == p)
                    .expect("fixture")
                    .encoding,
                false,
            ),
        };
        assert_eq!(
            (hit.encoding.as_deref(), hit.lossy),
            (enc, lossy),
            "{hit:?}"
        );
    }

    // Replacing a file moves its count; pruning removes it.
    s.index_bytes_opts(
        "o",
        "r",
        "lossy.toy",
        b"now utf8\n",
        None,
        dir,
        IndexOptions::default(),
    )
    .unwrap();
    check(&expect_enc, 0, n);
    s.index_bytes_opts(
        "o",
        "r",
        "sjis.toy",
        b"now utf8\n",
        None,
        dir,
        IndexOptions::default(),
    )
    .unwrap();
    expect_enc.remove("Shift_JIS");
    check(&expect_enc, 0, n);
    let keep: HashSet<String> = ["u8.toy", "lossy.toy", "sjis.toy", "be.toy"]
        .into_iter()
        .map(String::from)
        .collect();
    s.prune_files("o", "r", &keep, false).unwrap();
    let only_be: BTreeMap<String, usize> = [("UTF-16BE".to_string(), 1)].into();
    check(&only_be, 0, keep.len());
}

/// A batch-level `IndexOptions::encoding` is the hint of every file without
/// its own (ADR 0007 C8), on `index_batch` and on `prepare`; a file's own
/// hint wins. A `utf-8`-hinted lossy file lists with `lossy` and no
/// `encoding` (absent means UTF-8).
fn batch_level_encoding_hint(h: &Harness) {
    let s = open(h);
    let latin = legacy("caf\u{e9} au lait\n", encoding_rs::WINDOWS_1252);
    let opts = IndexOptions {
        encoding: Some(encoding_rs::UTF_8),
        ..Default::default()
    };
    let files = [
        BatchFile {
            path: "batch.txt",
            bytes: &latin,
            ..Default::default()
        },
        BatchFile {
            path: "own.txt",
            bytes: &latin,
            encoding: Some(encoding_rs::WINDOWS_1252),
            ..Default::default()
        },
    ];
    let out = s.index_batch("o", "r", &files, opts).unwrap();
    let prepared = prepare_all(&*s, "p", &files, opts);
    let out2 = s.index_prepared("o", "p", prepared, opts).unwrap();
    for st in out.iter().chain(&out2) {
        let st = st.as_ref().unwrap();
        let f = file_node(&*s, st);
        if st.path == "batch.txt" {
            assert_eq!((f.encoding.as_deref(), f.lossy), (None, true), "{st:?}");
        } else {
            assert_eq!(
                (f.encoding.as_deref(), f.lossy),
                (Some("windows-1252"), false),
                "{st:?}"
            );
        }
    }
    // Listing the repo shows the lossy UTF-8 file as such.
    let repo = s
        .roots()
        .unwrap()
        .into_iter()
        .flat_map(|o| s.children(o.id).unwrap())
        .find(|r| r.name == "r")
        .unwrap();
    let listed = s.children(repo.id).unwrap();
    let lossy = listed.iter().find(|n| n.name == "batch.txt").unwrap();
    assert!(lossy.lossy && lossy.encoding.is_none(), "{lossy:?}");
    assert_eq!(
        s.describe(None, None).unwrap(),
        s.describe_by_scan(None, None).unwrap()
    );
}
