//! The scoped-read conformance cases (ADR 0013 D5, "How tests prove
//! scoped read == unscoped read filtered").
//!
//! - [`run_scoped_differential`], the **strong form**: store A holds the
//!   whole scoped corpus, store B only the repos `scope` makes visible, fed
//!   in A's relative order (A's input sequence with the hidden repos' inputs
//!   removed). Every aggregate query, and every named query of a visible
//!   resource, must answer identically on A under `scope` and on B. Hits
//!   and describe rows carry no node ids, so they compare as they are, in
//!   the order the API returns them.
//! - The **weak form**: every aggregate query on A under `scope` equals the
//!   unscoped answer filtered to the visible repos, then paged.
//! - **Named probes**: every existing hidden org or repo a query names is
//!   `PermissionDenied` (whose text names no resource); every name that
//!   does not exist now (never indexed, or deleted) keeps today's answer,
//!   equal to B's and to the unscoped store's, so the two answers differ.
//! - [`run_scoped_crash_rerun_differential`]: the same with A seeded by a
//!   batch that crashed mid-way and was re-run.
//! - [`run_search_insertion_order`]: search and symbol search answer the
//!   same whatever order the same inputs were indexed in (their ranking is
//!   content-defined: repos by (org, repo) name, then path, then position).
//!
//! - [`scoped_node_queries`] (story 74b): the node-id APIs (`get`,
//!   `parent`, `roots`, `children`, `descendants`, their pages, `ancestors`,
//!   `count_nodes`, `file_tokens`) through [`StoreRead::scoped`], on the
//!   stable projection, plus node-id probes of every hidden node.
//! - [`run_scoped_deleted_and_snapshot`]: deleted and pruned hidden repos,
//!   and existence judged against a snapshot handle's snapshot.
use super::*;
use crate::{ReadOpts, ReadScope, RepoInfo, ScopeRules};
use std::collections::BTreeSet;

/// Same bytes in a visible and a hidden repo (the same-content case).
const SAME: &[u8] = b"samecontent foo shared\n";

/// The scoped corpus: every org and repo name a query below names.
pub const SCOPED_REPOS: &[(&str, &str)] = &[
    ("o1", "r1"),
    ("o1", "r2"),
    ("o1", "r3"),
    ("o2", "r1"),
    ("o2", "r2"),
    ("o2", "enc"),
    ("o2", "own"),
    ("o3", "attr"),
    ("o4", "solo"),
];

/// Names that never exist, or existed and were deleted: today's answer.
const MISSING: &[(&str, Option<&str>)] = &[
    ("nope", None),
    ("nope", Some("r1")),
    ("o1", Some("nope")),
    ("o5", None),
    ("o5", Some("g1")),
    ("o1", Some("doomed")),
];

fn batch_files() -> Vec<BatchFile<'static>> {
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    let bodies: [&'static [u8]; 5] = [
        b"foo bar baz",
        b"foo (bar) qux",
        b"let x = foo;",
        b"bar bar bar",
        b"fn dup() { dup(); }",
    ];
    names
        .iter()
        .zip(bodies)
        .map(|(n, b)| BatchFile {
            path: n,
            bytes: b,
            language: Some("text"),
            origin: Some(ORIGIN_DIRECTORY),
            ..Default::default()
        })
        .collect()
}

/// Index the scoped corpus into `s`, skipping every input of a repo for
/// which `keep` is false, in one fixed order (B is A's sequence filtered).
fn scoped_seed(s: &dyn Store, keep: &dyn Fn(&str, &str) -> bool) {
    let opts = IndexOptions::default();
    if keep("o1", "r1") {
        let r = s.index_batch("o1", "r1", &batch_files(), opts).unwrap();
        assert!(r.iter().all(|r| r.is_ok()), "{r:?}");
    }
    type Op<'k> = Box<dyn Fn(&dyn Store) + 'k>;
    let ops: Vec<Op<'_>> = vec![
        Box::new(|s| {
            if keep("o1", "r1") {
                s.ingest_file("o1", "r1", "lib.rs", "rust", &rust_extraction(RUST))
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o2", "r2") {
                s.ingest_file("o2", "r2", "main.zig", "zig", &plain(ZIG))
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o1", "r2") {
                s.index_bytes("o1", "r2", "m.txt", b"foo mfoo m\n", None)
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o2", "r1") {
                s.index_bytes("o2", "r1", "same.txt", SAME, None).unwrap();
                s.index_bytes("o2", "r1", "other.txt", b"foo dup x\n", None)
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o1", "r2") {
                s.index_bytes("o1", "r2", "same.txt", SAME, None).unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o3", "attr") {
                for (p, src, syms) in name_pos_fixtures() {
                    s.ingest_file("o3", "attr", p, "attrtoy", &decl_extraction(&src, &syms))
                        .unwrap();
                }
            }
        }),
        Box::new(|s| {
            if keep("o1", "r3") {
                let src = "fn q() { bar(); }\nfoo();\n";
                let whole = span_of(src, "fn q() { bar(); }");
                let mut ex = plain(src);
                ex.symbols.push(sym("dup", SymbolKind::Function, whole));
                ex.symbols.push(sym("dup", SymbolKind::Method, whole));
                ex.symbols.push(sym("dup2", SymbolKind::Type, whole));
                s.ingest_file("o1", "r3", "eq.rs", "rust", &ex).unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o2", "own") {
                s.ingest_file("o2", "own", "own.toy", "toy", &owner_extraction())
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o2", "enc") {
                let opts = IndexOptions {
                    encoding: Some(encoding_rs::WINDOWS_1252),
                    ..Default::default()
                };
                let bytes = legacy("CustomerId caf\u{e9} foo\n", encoding_rs::WINDOWS_1252);
                s.index_bytes_opts("o2", "enc", "w.txt", &bytes, None, None, opts)
                    .unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o4", "solo") {
                s.index_bytes("o4", "solo", "x.rs", b"fn foo() { bar(); }\n", Some("rust"))
                    .unwrap();
            }
        }),
        // Deleted repos and orgs (#323): they do not exist now, so naming
        // them is today's answer under every scope.
        Box::new(|s| {
            if keep("o5", "g1") {
                s.index_bytes("o5", "g1", "x.txt", b"foo gone\n", None)
                    .unwrap();
                s.delete_scope("o5", None, false).unwrap();
            }
        }),
        Box::new(|s| {
            if keep("o1", "doomed") {
                s.index_bytes("o1", "doomed", "x.txt", b"foo doomed\n", None)
                    .unwrap();
                s.delete_scope("o1", Some("doomed"), false).unwrap();
            }
        }),
    ];
    for op in &ops {
        op(s);
    }
}

fn texts() -> &'static [&'static str] {
    &[
        "foo",
        "bar",
        "dup",
        "x",
        "(",
        "samecontent",
        "S",
        "CustomerId",
        "Shape",
        "missing",
    ]
}

const GRAINS: [Grain; 7] = [
    Grain::Token,
    Grain::Symbol,
    Grain::Method,
    Grain::Class,
    Grain::File,
    Grain::Repo,
    Grain::Org,
];

/// Every search variant a filter (or none) is combined with.
fn search_variants(text: &str, grain: Grain) -> Vec<Query> {
    let mut q = Query::new(text);
    q.grain = grain;
    let mut out = vec![q.clone()];
    for n in [0, 1, 2, 3, 5] {
        q.limit = Some(n);
        out.push(q.clone());
        q.offset = Some(1);
        out.push(q.clone());
        q.offset = None;
    }
    q.limit = None;
    q.offset = Some(2);
    out.push(q.clone());
    q.offset = None;
    q.language = Some("rust".into());
    out.push(q.clone());
    q.language = None;
    q.class = Some(graph_core::TokenClass::Identifier);
    out.push(q.clone());
    q.class = None;
    q.symbol_kind = Some("function".into());
    out.push(q);
    out
}

fn symbol_variants(pattern: &str) -> Vec<SymbolQuery> {
    let mut q = SymbolQuery::new(pattern);
    let mut out = vec![q.clone()];
    for n in [0, 1, 2, 4] {
        q.limit = Some(n);
        out.push(q.clone());
        q.offset = Some(1);
        out.push(q.clone());
        q.offset = None;
    }
    q.limit = None;
    q.kind = Some("method".into());
    out.push(q.clone());
    q.kind = None;
    q.file = Some("eq.rs".into());
    out.push(q.clone());
    q.file = None;
    q.exact_case = true;
    out.push(q);
    out
}

const PATTERNS: &[&str] = &["*", "dup", "dup*", "S", "*ha*", "q", "m*", "T", "Big"];

/// Every filter a named query can carry: none, each org, each repo name
/// alone, each (org, repo), and the missing ones.
fn filters() -> Vec<(Option<String>, Option<String>)> {
    let mut out: Vec<(Option<String>, Option<String>)> = vec![(None, None)];
    let orgs: BTreeSet<&str> = SCOPED_REPOS.iter().map(|(o, _)| *o).collect();
    let repos: BTreeSet<&str> = SCOPED_REPOS.iter().map(|(_, r)| *r).collect();
    for o in &orgs {
        out.push((Some(o.to_string()), None));
    }
    for r in &repos {
        out.push((None, Some(r.to_string())));
    }
    for (o, r) in SCOPED_REPOS {
        out.push((Some(o.to_string()), Some(r.to_string())));
    }
    for (o, r) in MISSING {
        out.push((Some(o.to_string()), r.map(str::to_string)));
        if let Some(r) = r {
            out.push((None, Some(r.to_string())));
        }
    }
    out
}

/// What the named-read rule says about a filter on the unscoped store `a`:
/// `None` for an aggregate or a missing name (answered), `Some(true)` for
/// visible, `Some(false)` for an existing hidden name (refused).
fn named_visibility(
    a: &dyn StoreRead,
    scope: &ReadScope,
    org: Option<&str>,
    repo: Option<&str>,
) -> Option<bool> {
    if org.is_none() && repo.is_none() {
        return None;
    }
    let existing = a.describe(org, repo).unwrap();
    if existing.is_empty() {
        return None;
    }
    Some(match (org, repo) {
        (Some(o), None) => {
            scope.org_granted(o) || existing.iter().any(|i| scope.repo_visible(&i.org, &i.repo))
        }
        _ => existing.iter().any(|i| scope.repo_visible(&i.org, &i.repo)),
    })
}

fn assert_denied<T: std::fmt::Debug>(r: Result<T, StoreError>, action: &str, what: &str) {
    match r {
        Err(StoreError::PermissionDenied { msg, action: a, .. }) => {
            assert_eq!(a, action, "{what}");
            for (o, r) in SCOPED_REPOS {
                assert!(
                    !msg.contains(&format!("\"{o}\"")) && !msg.contains(&format!("{o}/{r}")),
                    "the refusal names a resource: {msg} ({what})"
                );
            }
            assert_eq!(
                msg,
                format!("permission denied for action `{action}`"),
                "{what}"
            );
        }
        other => panic!("{what}: expected PermissionDenied, got {other:?}"),
    }
}

fn page<T: Clone>(all: &[T], offset: Option<usize>, limit: Option<usize>) -> Vec<T> {
    let off = offset.unwrap_or(0).min(all.len());
    let rest = &all[off..];
    match limit {
        Some(n) => rest[..n.min(rest.len())].to_vec(),
        None => rest.to_vec(),
    }
}

/// The query half of the scoped differential: `a` holds the whole corpus,
/// `b` the visible part. With `apply` the scope is set on each query and
/// passed to the `_with` methods (an embedded store); without it `a`
/// applies it itself (a `RemoteStore` whose token the server scopes), and
/// the weak form, which needs `a` unscoped, is skipped.
pub fn scoped_queries(a: &dyn Store, b: &dyn Store, scope: &ReadScope, apply: bool) {
    let opts = ReadOpts::scoped(if apply { scope.clone() } else { ReadScope::All });
    let with = |q: &Query| {
        let mut q = q.clone();
        if apply {
            q.scope = scope.clone();
        }
        q
    };
    let with_sym = |q: &SymbolQuery| {
        let mut q = q.clone();
        if apply {
            q.scope = scope.clone();
        }
        q
    };
    // Existence as the unscoped store `b ∪ hidden` sees it: for an embedded
    // `a`, `a` itself; for a remote `a`, a filter's visibility follows from
    // the scope and the corpus list (which is what `a` holds).
    let unscoped: &dyn StoreRead = a;
    let vis = |o: Option<&str>, r: Option<&str>| -> Option<bool> {
        if apply {
            named_visibility(unscoped, scope, o, r)
        } else {
            named_visibility_by_list(scope, o, r)
        }
    };
    for (org, repo) in filters() {
        let (o, r) = (org.as_deref(), repo.as_deref());
        let what = format!("{o:?}/{r:?}");
        match vis(o, r) {
            Some(false) => {
                // An existing hidden name: refused, and B (which never held
                // it) answers today's empty answer: the two differ (Q2).
                assert_denied(
                    a.describe_with(o, r, &opts),
                    "read",
                    &format!("describe {what}"),
                );
                assert_denied(
                    a.describe_by_scan_with(o, r, &opts),
                    "read",
                    &format!("describe_by_scan {what}"),
                );
                assert_denied(
                    a.extractor_gaps_with(o, r, &opts),
                    "read",
                    &format!("extractor_gaps {what}"),
                );
                assert!(b.describe(o, r).unwrap().is_empty(), "B holds {what}");
                let mut q = Query::new("foo");
                q.org = org.clone();
                q.repo = repo.clone();
                assert_denied(a.search(&with(&q)), "search", &format!("search {what}"));
                let mut sq = SymbolQuery::new("*");
                sq.org = org.clone();
                sq.repo = repo.clone();
                assert_denied(
                    a.search_symbols(&with_sym(&sq)),
                    "search",
                    &format!("search_symbols {what}"),
                );
                continue;
            }
            // Missing now (never indexed, or deleted): today's answer,
            // equal to B's (below) and to the unscoped store's.
            None if apply && (o.is_some() || r.is_some()) => {
                assert_eq!(
                    a.describe_with(o, r, &opts).unwrap(),
                    a.describe(o, r).unwrap(),
                    "missing {what} is today's answer"
                );
            }
            _ => {}
        }
        let describe = a.describe_with(o, r, &opts).unwrap();
        assert_eq!(describe, b.describe(o, r).unwrap(), "describe {what}");
        assert_eq!(
            a.describe_by_scan_with(o, r, &opts).unwrap(),
            b.describe_by_scan(o, r).unwrap(),
            "describe_by_scan {what}"
        );
        assert_eq!(
            a.extractor_gaps_with(o, r, &opts).unwrap(),
            b.extractor_gaps(o, r).unwrap(),
            "extractor_gaps {what}"
        );
        if apply {
            let full = a.describe(o, r).unwrap();
            assert_eq!(
                describe,
                scope.filter_rows(full, |i: &RepoInfo| (&i.org, &i.repo)),
                "describe weak form {what}"
            );
        }
        for text in texts() {
            for grain in GRAINS {
                for mut q in search_variants(text, grain) {
                    q.org = org.clone();
                    q.repo = repo.clone();
                    let got = a.search(&with(&q)).unwrap();
                    assert_eq!(got, b.search(&q).unwrap(), "search {text}/{grain:?} {q:?}");
                    if apply {
                        let mut all = q.clone();
                        all.limit = None;
                        all.offset = None;
                        let full: Vec<_> = a
                            .search(&all)
                            .unwrap()
                            .into_iter()
                            .filter(|h| match &h.repo {
                                Some(r) => scope.repo_visible(&h.org, r),
                                // An org row: the org's visible part only;
                                // checked by the strong form above.
                                None => true,
                            })
                            .collect();
                        if grain != Grain::Org {
                            assert_eq!(
                                got,
                                page(&full, q.offset, q.limit),
                                "search weak form {text}/{grain:?} {q:?}"
                            );
                        }
                    }
                }
            }
        }
        for pat in PATTERNS {
            for mut q in symbol_variants(pat) {
                q.org = org.clone();
                q.repo = repo.clone();
                let got = a.search_symbols(&with_sym(&q)).unwrap();
                assert_eq!(got, b.search_symbols(&q).unwrap(), "symbols {q:?}");
                if apply {
                    let mut all = q.clone();
                    all.limit = None;
                    all.offset = None;
                    let full: Vec<_> = a
                        .search_symbols(&all)
                        .unwrap()
                        .into_iter()
                        .filter(|h| scope.repo_visible(&h.org, &h.repo))
                        .collect();
                    assert_eq!(
                        got,
                        page(&full, q.offset, q.limit),
                        "symbols weak form {q:?}"
                    );
                }
            }
        }
    }
    // The same-content case: the visible occurrence answers with its own
    // repo's path, and the hidden one never shows up.
    let hits = a.search(&with(&Query::new("samecontent"))).unwrap();
    for h in &hits {
        assert!(
            scope.repo_visible(&h.org, h.repo.as_deref().unwrap_or_default()),
            "a hidden occurrence leaked: {h:?}"
        );
    }
    let want = SCOPED_REPOS
        .iter()
        .filter(|(o, r)| matches!((*o, *r), ("o1", "r2") | ("o2", "r1")))
        .filter(|(o, r)| scope.repo_visible(o, r))
        .count();
    assert_eq!(hits.len(), want, "same-content occurrences: {hits:?}");
    // `ReadScope::All` is the unscoped store exactly.
    if apply {
        let all = ReadOpts::default();
        for (org, repo) in filters() {
            let (o, r) = (org.as_deref(), repo.as_deref());
            assert_eq!(
                a.describe_with(o, r, &all).unwrap(),
                a.describe(o, r).unwrap()
            );
        }
    }
}

/// [`named_visibility`] from the corpus list instead of a store: for a
/// remote `a`, whose unscoped answers the test cannot see.
fn named_visibility_by_list(
    scope: &ReadScope,
    org: Option<&str>,
    repo: Option<&str>,
) -> Option<bool> {
    if org.is_none() && repo.is_none() {
        return None;
    }
    let existing: Vec<&(&str, &str)> = SCOPED_REPOS
        .iter()
        .filter(|(o, r)| org.is_none_or(|x| x == *o) && repo.is_none_or(|x| x == *r))
        .collect();
    if existing.is_empty() {
        return None;
    }
    Some(match (org, repo) {
        (Some(o), None) => {
            scope.org_granted(o) || existing.iter().any(|(o, r)| scope.repo_visible(o, r))
        }
        _ => existing.iter().any(|(o, r)| scope.repo_visible(o, r)),
    })
}

/// Seed `a` with the whole scoped corpus and `b` with only what `scope`
/// shows, in A's relative order (for a remote B as well as an embedded
/// one).
pub fn seed_scoped(a: &dyn Store, b: &dyn Store, scope: &ReadScope) {
    scoped_seed(a, &|_, _| true);
    scoped_seed(b, &|o, r| scope.repo_visible(o, r));
}

/// [`seed_scoped`] with B's repos given as a fixed list instead of
/// derived from a scope, so a visibility bug in the store cannot also
/// shape the oracle.
pub fn seed_scoped_with(a: &dyn Store, b: &dyn Store, visible: &[(&str, &str)]) {
    scoped_seed(a, &|_, _| true);
    scoped_seed(b, &|o, r| visible.contains(&(o, r)));
}

/// The strong and weak forms and the named probes (ADR 0013 D5) for
/// `scope`, with `a` and `b` empty stores in any configuration.
pub fn run_scoped_differential(a: &dyn Store, b: &dyn Store, scope: &ReadScope) {
    seed_scoped(a, b, scope);
    scoped_queries(a, b, scope, true);
    let av = a.scoped(scope.clone());
    scoped_node_queries(&*av, Some(a), b, scope);
    // The view answers the name-filtered reads exactly as the scoped
    // queries do.
    for (org, repo) in [(None, None), (Some("o1"), None), (None, Some("r1"))] {
        let want = a.describe_with(org, repo, &ReadOpts::scoped(scope.clone()));
        match (av.describe(org, repo), want) {
            (Ok(x), Ok(y)) => assert_eq!(x, y),
            (Err(_), Err(_)) => {}
            (x, y) => panic!("view {x:?} vs describe_with {y:?}"),
        }
    }
    // `ReadScope::All` is the store itself.
    let all = a.scoped(ReadScope::All);
    assert_eq!(projs(&all.roots().unwrap()), projs(&a.roots().unwrap()));
    for k in [
        NodeKind::Org,
        NodeKind::Repo,
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Token,
    ] {
        assert_eq!(all.count_nodes(k).unwrap(), a.count_nodes(k).unwrap());
    }
}

/// [`run_scoped_differential`] with A seeded through a batch that crashed
/// mid-way (a poisoned file after the third) and was re-run.
pub fn run_scoped_crash_rerun_differential(crashed: &dyn Store, b: &dyn Store, scope: &ReadScope) {
    let good = batch_files();
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
            .index_batch("o1", "r1", &poisoned, IndexOptions::default())
            .is_err(),
        "the poisoned batch must fail"
    );
    run_scoped_differential(crashed, b, scope);
}

/// Search and symbol search are content-ordered (ADR 0013 D5, story 74a):
/// the same inputs indexed in two different orders answer identically.
/// `a` gets the corpus in order, `b` the per-repo inputs in reverse repo
/// order.
pub fn run_search_insertion_order(a: &dyn Store, b: &dyn Store) {
    scoped_seed(a, &|_, _| true);
    for (o, r) in SCOPED_REPOS.iter().rev() {
        scoped_seed(b, &|x, y| x == *o && y == *r);
    }
    for text in texts() {
        for grain in GRAINS {
            for q in search_variants(text, grain) {
                assert_eq!(a.search(&q).unwrap(), b.search(&q).unwrap(), "{q:?}");
            }
        }
    }
    for pat in PATTERNS {
        for q in symbol_variants(pat) {
            assert_eq!(
                a.search_symbols(&q).unwrap(),
                b.search_symbols(&q).unwrap(),
                "{q:?}"
            );
        }
    }
}

/// The fixed scopes every backend runs, including the zero-grant scope,
/// `forbid` holes inside a permitted org, a repo name shared by a visible
/// and a hidden repo, and an org granted with none of its repos visible.
pub fn fixed_scopes() -> Vec<(&'static str, ReadScope)> {
    let s = |f: &dyn Fn(&mut ScopeRules)| {
        let mut r = ScopeRules::default();
        f(&mut r);
        ReadScope::from_rules(r)
    };
    let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
    let pairs = |v: &[(&str, &str)]| {
        v.iter()
            .map(|(o, r)| (o.to_string(), r.to_string()))
            .collect::<BTreeSet<_>>()
    };
    vec![
        ("all", ReadScope::All),
        ("nothing", ReadScope::nothing()),
        ("org o1", s(&|r| r.orgs = set(&["o1"]))),
        (
            "org o1 minus o1/r2",
            s(&|r| {
                r.orgs = set(&["o1"]);
                r.deny_repos = pairs(&[("o1", "r2")]);
            }),
        ),
        (
            "repos o1/r1 + o2/r2",
            s(&|r| r.repos = pairs(&[("o1", "r1"), ("o2", "r2")])),
        ),
        (
            "any minus org o2",
            s(&|r| {
                r.any = true;
                r.deny_orgs = set(&["o2"]);
            }),
        ),
        (
            "o2/r1 only (r1 shared with hidden o1/r1)",
            s(&|r| r.repos = pairs(&[("o2", "r1")])),
        ),
        (
            "org o3 granted, its only repo forbidden; plus o4",
            s(&|r| {
                r.orgs = set(&["o3", "o4"]);
                r.deny_repos = pairs(&[("o3", "attr")]);
            }),
        ),
        (
            "org-only o2 grant",
            s(&|r| {
                r.org_only = set(&["o2"]);
                r.repos = pairs(&[("o1", "r3")]);
            }),
        ),
    ]
}

/// The node-id half of the scoped differential (story 74b): `av` is store
/// A seen through `scope` (an embedded `A.scoped(scope)`, or a remote
/// store the server scopes), `b` the visible-only index. With `unscoped`
/// (A itself), every node of a hidden repo or org is probed too.
pub fn scoped_node_queries(
    av: &dyn StoreRead,
    unscoped: Option<&dyn StoreRead>,
    b: &dyn StoreRead,
    scope: &ReadScope,
) {
    // Orgs visible only through a grant on the org itself (none of their
    // repos visible) exist in A but not in B: they are listed with no
    // children, and set aside before comparing.
    let granted_only = |n: &Node| {
        n.kind == NodeKind::Org
            && !SCOPED_REPOS
                .iter()
                .any(|(o, r)| *o == n.name && scope.repo_visible(o, r))
    };
    // Roots are compared by name: an org's creation order is set by its
    // first repo, which may be hidden in A (a residual of ADR 0013 D5's
    // relative-order rule: B cannot reproduce it).
    let by_name = |mut v: Vec<Node>| {
        v.sort_by(|x, y| x.name.cmp(&y.name));
        v
    };
    let a_roots = by_name(av.roots().unwrap());
    let (extra, a_roots): (Vec<Node>, Vec<Node>) = a_roots.into_iter().partition(granted_only);
    for o in &extra {
        assert!(
            scope.org_granted(&o.name),
            "org {} listed without a grant",
            o.name
        );
        assert!(
            av.children(o.id).unwrap().is_empty(),
            "children of {}",
            o.name
        );
        assert_eq!(av.children_page(o.id, 0, 10).unwrap().items.len(), 0);
        assert!(av.descendants(o.id).unwrap().is_empty());
    }
    let b_roots = by_name(b.roots().unwrap());
    assert_eq!(projs(&a_roots), projs(&b_roots), "roots");
    assert_eq!(
        av.count_nodes(NodeKind::Org).unwrap(),
        b.count_nodes(NodeKind::Org).unwrap() + extra.len(),
        "count Org"
    );
    for k in [
        NodeKind::Repo,
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Token,
    ] {
        assert_eq!(
            av.count_nodes(k).unwrap(),
            b.count_nodes(k).unwrap(),
            "count {k:?}"
        );
    }
    for (ra, rb) in a_roots.iter().zip(&b_roots) {
        let what = &ra.name;
        assert_eq!(
            projs(&av.children(ra.id).unwrap()),
            projs(&b.children(rb.id).unwrap()),
            "children {what}"
        );
        for (off, lim) in [(0, 1), (1, 1), (0, 2), (1, 10), (5, 3), (0, 100)] {
            let (pa, pb) = (
                av.children_page(ra.id, off, lim).unwrap(),
                b.children_page(rb.id, off, lim).unwrap(),
            );
            assert_eq!(
                projs(&pa.items),
                projs(&pb.items),
                "children_page {what} {off}/{lim}"
            );
            assert_eq!(
                pa.has_more, pb.has_more,
                "children_page more {what} {off}/{lim}"
            );
            let (pa, pb) = (
                av.descendants_page(ra.id, off, lim).unwrap(),
                b.descendants_page(rb.id, off, lim).unwrap(),
            );
            assert_eq!(
                projs(&pa.items),
                projs(&pb.items),
                "descendants_page {what} {off}/{lim}"
            );
            assert_eq!(pa.has_more, pb.has_more, "descendants_page more {what}");
        }
        let (da, db) = (
            av.descendants(ra.id).unwrap(),
            b.descendants(rb.id).unwrap(),
        );
        assert_eq!(projs(&da), projs(&db), "descendants {what}");
        for (na, nb) in da.iter().zip(&db) {
            if na.kind == NodeKind::Token {
                continue;
            }
            assert_eq!(
                projs(&av.children(na.id).unwrap()),
                projs(&b.children(nb.id).unwrap())
            );
            assert_eq!(
                projs(&av.ancestors(na.id).unwrap()),
                projs(&b.ancestors(nb.id).unwrap())
            );
            assert_eq!(av.get(na.id).unwrap().map(|n| proj(&n)), Some(proj(nb)));
            assert_eq!(
                av.parent(na.id).unwrap().map(|n| proj(&n)),
                b.parent(nb.id).unwrap().map(|n| proj(&n))
            );
        }
    }
    for (o, r) in SCOPED_REPOS {
        for p in [
            "a.txt", "lib.rs", "same.txt", "m.txt", "eq.rs", "x.rs", "main.zig",
        ] {
            let got = av.file_tokens(o, r, p);
            if !scope.repo_visible(o, r) {
                let what = format!("file_tokens {o}/{r}/{p}");
                match unscoped {
                    Some(u) if u.file_tokens(o, r, p).unwrap().is_some() => {
                        assert_denied(got, "read", &what)
                    }
                    Some(_) => assert_eq!(got.unwrap(), None, "{what}"),
                    // Remote: refused if it exists, else today's `None`.
                    None => {
                        if !matches!(got, Ok(None)) {
                            assert_denied(got, "read", &what)
                        }
                    }
                }
                continue;
            }
            let t = |v: Option<Vec<Node>>| v.map(|v| projs(&v));
            assert_eq!(
                t(got.unwrap()),
                t(b.file_tokens(o, r, p).unwrap()),
                "file_tokens {o}/{r}/{p}"
            );
        }
    }
    let Some(u) = unscoped else {
        return;
    };
    // Named probes: every node of a hidden repo (and every hidden org) is
    // refused through every node-id API; never-existing ids keep today's
    // answer.
    let mut max_id = 0;
    for root in u.roots().unwrap() {
        let org_visible = !(av.roots().unwrap().iter().all(|n| n.id != root.id));
        let mut all = vec![root.clone()];
        all.extend(u.descendants(root.id).unwrap());
        let mut repo_visible = true;
        for n in &all {
            max_id = max_id.max(n.id);
            if n.kind == NodeKind::Repo {
                repo_visible = scope.repo_visible(&root.name, &n.name);
            }
            let visible = if n.kind == NodeKind::Org {
                org_visible
            } else {
                repo_visible
            };
            if visible {
                assert!(av.get(n.id).unwrap().is_some());
                continue;
            }
            let what = format!("node {} ({:?} {})", n.id, n.kind, n.name);
            assert_denied(av.get(n.id), "read", &what);
            assert_denied(av.parent(n.id), "read", &what);
            assert_denied(av.children(n.id), "read", &what);
            assert_denied(av.children_page(n.id, 0, 5), "read", &what);
            assert_denied(av.descendants(n.id), "read", &what);
            assert_denied(av.descendants_page(n.id, 0, 5), "read", &what);
            assert_denied(av.ancestors(n.id), "read", &what);
        }
    }
    for id in [max_id + 1, max_id + 1_000_000] {
        assert_eq!(av.get(id).unwrap(), None);
        assert_eq!(av.parent(id).unwrap(), None);
        assert!(av.children(id).unwrap().is_empty());
        assert!(av.children_page(id, 0, 5).unwrap().items.is_empty());
        assert!(av.descendants(id).unwrap().is_empty());
        assert!(av.ancestors(id).unwrap().is_empty());
        assert_eq!(u.get(id).unwrap(), None, "today's answer");
    }
}

/// A deleted or pruned hidden repo gets today's answer, and through a
/// snapshot handle existence is judged against the handle's snapshot (ADR
/// 0013 D5, story 74b). `s` is an empty store.
pub fn run_scoped_deleted_and_snapshot(s: &dyn Store) {
    let hide = ReadScope::from_rules(ScopeRules {
        orgs: ["vis".to_string()].into(),
        ..Default::default()
    });
    s.index_bytes("vis", "v", "a.txt", b"foo\n", None).unwrap();
    let del = s
        .index_bytes("hid", "d", "a.txt", b"foo\n", None)
        .unwrap()
        .file_id;
    let files = [bf("a.txt", b"foo\n"), bf("keep.txt", b"foo\n")];
    let r = s
        .index_batch("hid", "p", &files, IndexOptions::default())
        .unwrap();
    let pru = r[0].as_ref().unwrap().file_id;
    let snap = s.snapshot().unwrap();
    s.delete_scope("hid", Some("d"), false).unwrap();
    s.prune_files("hid", "p", &["keep.txt".to_string()].into(), false)
        .unwrap();
    let live = s.scoped(hide.clone());
    // Deleted and pruned: today's answer, the same as a never-existing id.
    // (Entity ids, org/repo/file, are never reused; checked before the
    // next write anyway, so the probe does not depend on that.)
    for id in [del, pru] {
        assert_eq!(live.get(id).unwrap(), None);
        assert!(live.children(id).unwrap().is_empty());
        assert_eq!(s.get(id).unwrap(), None);
    }
    let late = s
        .index_bytes("hid", "late", "a.txt", b"foo\n", None)
        .unwrap()
        .file_id;
    assert!(live.describe(Some("hid"), Some("d")).unwrap().is_empty());
    assert_eq!(live.file_tokens("hid", "p", "a.txt").unwrap(), None);
    // Still existing hidden: refused.
    assert_denied(live.get(late), "read", "late");
    assert_denied(
        live.describe(Some("hid"), Some("p")),
        "read",
        "pruned repo, still there",
    );
    // Through the snapshot: what it holds is judged as of the snapshot.
    let sv = snap.scoped(hide);
    assert_denied(sv.get(del), "read", "deleted after the snapshot");
    assert_denied(sv.get(pru), "read", "pruned after the snapshot");
    assert_denied(
        sv.describe(Some("hid"), Some("d")),
        "read",
        "snapshot describe",
    );
    assert_denied(
        sv.file_tokens("hid", "d", "a.txt"),
        "read",
        "snapshot file_tokens",
    );
    assert!(sv.describe(Some("hid"), Some("late")).unwrap().is_empty());
    let mut q = Query::new("foo");
    q.repo = Some("late".into());
    assert!(sv.search(&q).unwrap().is_empty());
    q.repo = Some("d".into());
    assert_denied(sv.search(&q), "search", "snapshot search");
}
