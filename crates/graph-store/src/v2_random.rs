//! Configuration-equivalence comparisons that cross stream checkpoints
//! (files of 64+ tokens), with replace, prune, language and class filters,
//! plus the old-schema and legacy-format refusal tests.
use super::*;
use crate::v2_tests::two_configs;
use graph_core::{Extraction, Span, SymbolDecl, TokenClass, TokenDecl};
use proptest::prelude::*;

const VOCAB: [&str; 4] = ["new", "a", "b", "("];
const KINDS: [SymbolKind; 3] = [SymbolKind::Function, SymbolKind::Method, SymbolKind::Type];
const NAMES: [&str; 4] = ["new", "a", "ab", "x"];
const CLASSES: [TokenClass; 3] = [
    TokenClass::Identifier,
    TokenClass::Keyword,
    TokenClass::Punctuation,
];

/// Token `(vocab index, class index)`; symbol `(name, kind, a, b)` where `a`
/// and `b` are reduced modulo the token count into token positions.
type Tok = (usize, usize);
type Sym = (usize, usize, usize, usize);

fn sp(s: u32, e: u32) -> Span {
    Span {
        start: s,
        end: e,
        start_line: 1 + s / 40,
        start_col: s % 40,
        end_line: 1 + e / 40,
        end_col: e % 40,
    }
}

fn extraction(toks: &[Tok], syms: &[Sym]) -> Extraction {
    let n = toks.len() + 1;
    Extraction {
        has_errors: false,
        tokens: toks
            .iter()
            .enumerate()
            .map(|(i, &(v, c))| TokenDecl {
                text: VOCAB[v].into(),
                class: CLASSES[c],
                span: sp(i as u32 * 4, i as u32 * 4 + 2),
            })
            .collect(),
        symbols: syms
            .iter()
            .map(|&(nm, k, a, b)| {
                let (a, b) = (a % n, b % n);
                SymbolDecl {
                    name: NAMES[nm].into(),
                    kind: KINDS[k],
                    lang_kind: None,
                    span: sp(a.min(b) as u32 * 4, a.max(b) as u32 * 4 + 3),
                }
            })
            .collect(),
    }
}

/// A deterministic file of `n` tokens with terms, classes and symbols spread
/// over it, so hits land in every checkpoint block.
fn long_file(n: usize) -> Extraction {
    let toks: Vec<Tok> = (0..n).map(|i| ((i * 7 + i / 3) % 4, (i / 2) % 3)).collect();
    let syms: Vec<Sym> = vec![(0, 2, 0, n), (1, 1, 5, 20), (2, 0, n / 2, n / 2 + 10)];
    extraction(&toks, &syms)
}

fn all_queries() -> Vec<Query> {
    let mut out = Vec::new();
    for text in VOCAB {
        for grain in [
            Grain::Token,
            Grain::Symbol,
            Grain::File,
            Grain::Repo,
            Grain::Org,
        ] {
            for class in [
                None,
                Some(TokenClass::Identifier),
                Some(TokenClass::Keyword),
            ] {
                for (language, limit) in [(None, None), (Some("zig"), None), (None, Some(3))] {
                    let mut q = Query::new(text);
                    q.grain = grain;
                    q.class = class;
                    q.language = language.map(Into::into);
                    q.limit = limit;
                    out.push(q);
                }
            }
        }
    }
    out
}

fn same(a: &dyn Store, b: &dyn Store) -> std::result::Result<(), TestCaseError> {
    for q in all_queries() {
        prop_assert_eq!(a.search(&q).unwrap(), b.search(&q).unwrap(), "{:?}", q);
    }
    for pat in ["*", "new", "a*", "x"] {
        for lang in [None, Some("zig")] {
            let mut q = SymbolQuery::new(pat);
            q.language = lang.map(Into::into);
            prop_assert_eq!(
                a.search_symbols(&q).unwrap(),
                b.search_symbols(&q).unwrap(),
                "{:?}",
                q
            );
        }
    }
    prop_assert_eq!(
        a.describe(None, None).unwrap(),
        b.describe(None, None).unwrap()
    );
    Ok(())
}

#[test]
fn files_around_the_checkpoint_boundary_match_across_configurations() {
    let (_d, a, b) = two_configs();
    for (i, n) in [63usize, 64, 65, 127, 128, 129, 200]
        .into_iter()
        .enumerate()
    {
        let ex = long_file(n);
        let lang = if i % 2 == 0 { "rust" } else { "zig" };
        for s in [&a, &b] {
            s.ingest_file("o", "r", &format!("f{n}.txt"), lang, &ex)
                .unwrap();
        }
    }
    same(&*a, &*b).unwrap();
}

/// Ordinals in the same block, in adjacent blocks and far apart, with a
/// rare term whose hits straddle checkpoints.
#[test]
fn sparse_hits_across_blocks_match_across_configurations() {
    let (_d, a, b) = two_configs();
    let mut toks: Vec<Tok> = vec![(1, 0); 300];
    for i in [0, 62, 63, 64, 65, 127, 128, 250, 299] {
        toks[i] = (0, if i % 2 == 0 { 0 } else { 1 });
    }
    let ex = extraction(&toks, &[(3, 2, 60, 130)]);
    for s in [&a, &b] {
        s.ingest_file("o", "r", "x.rs", "rust", &ex).unwrap();
    }
    same(&*a, &*b).unwrap();
    let mut q = Query::new("new");
    q.grain = Grain::Token;
    assert_eq!(b.search(&q).unwrap().len(), 9);
}

#[test]
fn old_schema_v2_file_is_rejected_and_left_unchanged() {
    use sha2::{Digest, Sha256};
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("old.redb");
    {
        let s = V2Store::open(&path).unwrap();
        s.ingest_file("o", "r", "x.rs", "rust", &long_file(70))
            .unwrap();
    }
    // Stamp the previous v2 layout version.
    {
        let db = redb::Database::open(&path).unwrap();
        let wt = db.begin_write().unwrap();
        wt.open_table(crate::META)
            .unwrap()
            .insert("schema_version", 3)
            .unwrap();
        wt.commit().unwrap();
    }
    let digest = |p: &std::path::Path| Sha256::digest(std::fs::read(p).unwrap());
    let before = digest(&path);
    match V2Store::open(&path) {
        Err(StoreError::SchemaMismatch { found: 3 }) => {}
        Err(e) => panic!("wrong error {e:?}"),
        Ok(_) => panic!("old schema opened"),
    }
    assert_eq!(digest(&path), before, "file unchanged");
    // A retired v1 file (schema version 2) is refused as legacy, untouched.
    let legacy = d.path().join("legacy.redb");
    crate::tests::stamped_file(&legacy, 2);
    let before = digest(&legacy);
    match V2Store::open(&legacy) {
        Err(StoreError::LegacyFormat { version: 2, .. }) => {}
        Err(e) => panic!("wrong error {e:?}"),
        Ok(_) => panic!("legacy file opened"),
    }
    assert_eq!(digest(&legacy), before, "legacy file unchanged");
}

type FileSpec = (usize, usize, usize, Vec<Tok>, Vec<Sym>);

fn file_spec() -> impl Strategy<Value = FileSpec> {
    (
        0usize..2, // repo
        0usize..2, // org
        0usize..2, // language
        prop::collection::vec((0usize..VOCAB.len(), 0usize..3), 0..200),
        prop::collection::vec(
            (0usize..NAMES.len(), 0usize..3, 0usize..300, 0usize..300),
            0..4,
        ),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn configurations_agree_on_random_corpora(
        files in prop::collection::vec(file_spec(), 1..5),
        replacement in file_spec(),
        keep in prop::collection::vec(any::<bool>(), 5),
    ) {
        let (_d, a, b) = two_configs();
        let langs = ["rust", "zig"];
        let ing = |s: &dyn Store, o: &str, r: &str, p: &str, l: &str, ex: &Extraction| {
            s.ingest_file_with_origin(o, r, p, l, ex, Some(ORIGIN_DIRECTORY))
        };
        let mut ingested = 0;
        for (i, (repo, org, lang, toks, syms)) in files.iter().enumerate() {
            let ex = extraction(toks, syms);
            let (o, r, p) = (format!("o{org}"), format!("r{repo}"), format!("f{i}.rs"));
            // Partially overlapping symbols are rejected by both.
            if ing(&*a, &o, &r, &p, langs[*lang], &ex).is_ok() {
                ing(&*b, &o, &r, &p, langs[*lang], &ex).unwrap();
                ingested += 1;
            } else {
                prop_assert!(ing(&*b, &o, &r, &p, langs[*lang], &ex).is_err());
            }
        }
        prop_assume!(ingested > 0);
        same(&*a, &*b)?;
        // Replace f0.rs where it went (if it was accepted) with another spec.
        let (_, _, lang, toks, syms) = &replacement;
        let ex = extraction(toks, syms);
        let (o, r) = (format!("o{}", files[0].1), format!("r{}", files[0].0));
        if ing(&*a, &o, &r, "f0.rs", langs[*lang], &ex).is_ok() {
            ing(&*b, &o, &r, "f0.rs", langs[*lang], &ex).unwrap();
        }
        same(&*a, &*b)?;
        // Prune every repo down to a random subset of files.
        let set: std::collections::HashSet<String> = (0..5)
            .filter(|&i| keep[i])
            .map(|i| format!("f{i}.rs"))
            .collect();
        for org in 0..2 {
            for repo in 0..2 {
                let (o, r) = (format!("o{org}"), format!("r{repo}"));
                prop_assert_eq!(
                    a.prune_files(&o, &r, &set, false).unwrap(),
                    b.prune_files(&o, &r, &set, false).unwrap()
                );
            }
        }
        same(&*a, &*b)?;
    }
}
