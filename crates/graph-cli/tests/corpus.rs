//! Checks over the vendored public test corpus (testdata/corpus): the manifest
//! is consistent and public-only, cross-repo links resolve, and every token of
//! every file is parsed with exact spans and stored in the graph.
use graph_core::tokenizer::tokenize;
use graph_core::{detect_language_from_content, TokenClass};
use graph_store::{BatchFile, Query, Store};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

fn manifest() -> Value {
    serde_json::from_str(&std::fs::read_to_string(corpus_dir().join("corpus.json")).unwrap())
        .unwrap()
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect()
}

/// Every file under `dir`, relative and sorted, `/`-separated.
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn manifest_is_consistent_public_and_licensed() {
    let m = manifest();
    let repos = m["repos"].as_array().unwrap();
    let dirs: BTreeSet<&str> = repos.iter().map(|r| r["dir"].as_str().unwrap()).collect();
    assert_eq!(dirs.len(), repos.len(), "duplicate repo dirs");
    // Every folder on disk is a declared repo, and vice versa.
    let root: Vec<_> = std::fs::read_dir(corpus_dir())
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert!(
        root.iter()
            .all(|e| e.path().is_dir() || e.file_name() == "corpus.json"),
        "stray file in corpus root"
    );
    let on_disk: BTreeSet<String> = root
        .into_iter()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        on_disk,
        dirs.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>()
    );
    // Every repo belongs to exactly one application; each app has several or a single repo.
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for app in m["applications"].as_array().unwrap() {
        for r in strings(&app["repos"]) {
            assert!(dirs.contains(r), "app references unknown repo {r}");
            assert!(
                owner.insert(r, app["name"].as_str().unwrap()).is_none(),
                "{r} in two apps"
            );
        }
    }
    assert_eq!(owner.len(), dirs.len(), "repo without an application");
    for r in repos {
        let dir = corpus_dir().join(r["dir"].as_str().unwrap());
        let upstream = r["upstream"].as_str().unwrap();
        // Public GitHub code only, never the maintainer's own (possibly private) repos.
        assert!(upstream.starts_with("https://github.com/"), "{upstream}");
        let slug = upstream
            .trim_start_matches("https://github.com/")
            .trim_end_matches('/')
            .trim_end_matches(".git");
        assert_eq!(slug.split('/').count(), 2, "{upstream}");
        let owner_name = slug.split('/').next().unwrap().to_lowercase();
        for banned in ["p47phoenix", "michaelconne"] {
            assert_ne!(
                owner_name, banned,
                "corpus must not contain personal repos: {upstream}"
            );
        }
        let license = r["license"].as_str().unwrap();
        assert!(
            license == "MIT" || license == "Apache-2.0" || license == "MIT OR Apache-2.0",
            "{license}"
        );
        assert_eq!(r["commit"].as_str().unwrap().len(), 40);
        let listing = files(&dir);
        let texts: Vec<String> = listing
            .iter()
            .filter(|f| f.starts_with("LICENSE"))
            .map(|f| std::fs::read_to_string(dir.join(f)).unwrap())
            .collect();
        assert!(!texts.is_empty(), "{dir:?} has no license file");
        let mit = texts
            .iter()
            .any(|t| t.contains("Permission is hereby granted"));
        let apache = texts.iter().any(|t| t.contains("Apache License"));
        match license {
            "MIT" => assert!(mit, "{dir:?}: MIT text missing"),
            "Apache-2.0" => assert!(apache, "{dir:?}: Apache text missing"),
            _ => assert!(mit || apache, "{dir:?}: no recognisable licence text"),
        }
        let up = std::fs::read_to_string(dir.join("UPSTREAM.md")).unwrap();
        assert!(up.contains(upstream) && up.contains(r["commit"].as_str().unwrap()));
    }
}

#[test]
fn cross_repo_links_resolve() {
    let m = manifest();
    let dirs: BTreeSet<&str> = m["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["dir"].as_str().unwrap())
        .collect();
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for app in m["applications"].as_array().unwrap() {
        for r in strings(&app["repos"]) {
            owner.insert(r, app["name"].as_str().unwrap());
        }
    }
    let links = m["links"].as_array().unwrap();
    assert!(links.len() >= 6);
    for l in links {
        let (from, to) = (l["from"].as_str().unwrap(), l["to"].as_str().unwrap());
        assert!(
            dirs.contains(from) && dirs.contains(to) && from != to,
            "{l}"
        );
        assert_eq!(
            owner[from], owner[to],
            "links stay inside one application: {l}"
        );
        for side in ["from", "to"] {
            let repo = l[side].as_str().unwrap();
            let file = l[format!("{side}_file")].as_str().unwrap();
            let needle = l[format!("{side}_contains")].as_str().unwrap();
            let text = std::fs::read_to_string(corpus_dir().join(repo).join(file))
                .unwrap_or_else(|e| panic!("{repo}/{file}: {e}"));
            assert!(
                text.contains(needle),
                "{repo}/{file} does not contain {needle:?}"
            );
        }
    }
    // Every multi-repo application is connected by links.
    for app in m["applications"].as_array().unwrap() {
        let repos = strings(&app["repos"]);
        if repos.len() < 2 {
            continue;
        }
        let mut reached: BTreeSet<&str> = BTreeSet::from([repos[0]]);
        loop {
            let before = reached.len();
            for l in links {
                let (a, b) = (l["from"].as_str().unwrap(), l["to"].as_str().unwrap());
                if reached.contains(a) || reached.contains(b) {
                    reached.insert(a);
                    reached.insert(b);
                }
            }
            if reached.len() == before {
                break;
            }
        }
        for r in &repos {
            assert!(
                reached.contains(r),
                "{r} is not linked to the rest of {}",
                app["name"]
            );
        }
    }
}

/// Parse checks for every text file: tokens are exact, in order, non-overlapping,
/// and account for every non-whitespace byte.
#[test]
fn every_token_is_parsed_exactly() {
    let m = manifest();
    let (mut n_files, mut n_tokens) = (0usize, 0usize);
    for r in m["repos"].as_array().unwrap() {
        let name = r["dir"].as_str().unwrap();
        let dir = corpus_dir().join(name);
        let mut langs: BTreeSet<String> = BTreeSet::new();
        let mut repo_tokens = 0usize;
        for rel in files(&dir) {
            let bytes = std::fs::read(dir.join(&rel)).unwrap();
            let src =
                String::from_utf8(bytes).unwrap_or_else(|_| panic!("{name}/{rel} is not UTF-8"));
            langs.insert(detect_language_from_content(&rel, &src));
            let toks = tokenize(&src);
            let mut covered = 0usize;
            let mut prev_end = 0usize;
            for t in &toks {
                let (s, e) = (t.span.start as usize, t.span.end as usize);
                assert!(
                    s >= prev_end && e > s,
                    "{name}/{rel}: bad order at byte {s}"
                );
                assert_eq!(
                    &src[s..e],
                    t.text,
                    "{name}/{rel}: text mismatch at byte {s}"
                );
                assert!(
                    src[prev_end..s]
                        .chars()
                        .all(|c| c.is_whitespace() || c == '\u{feff}'),
                    "{name}/{rel}: unparsed bytes before {s}"
                );
                covered += e - s;
                prev_end = e;
            }
            assert!(
                src[prev_end..]
                    .chars()
                    .all(|c| c.is_whitespace() || c == '\u{feff}'),
                "{name}/{rel}: unparsed tail"
            );
            assert!(covered <= src.len());
            repo_tokens += toks.len();
            assert!(
                !toks.iter().any(|t| t.class == TokenClass::Other),
                "{name}/{rel}: unexpected class"
            );
            n_files += 1;
            n_tokens += toks.len();
        }
        assert!(repo_tokens > 200, "{name}: only {repo_tokens} tokens");
        for want in strings(&r["languages"]) {
            assert!(
                langs.contains(want),
                "{name}: expected language {want}, detected {langs:?}"
            );
        }
    }
    assert!(
        n_files > 400 && n_tokens > 100_000,
        "{n_files} files, {n_tokens} tokens"
    );
}

/// Index the whole corpus (one org per application, one repo per folder) and
/// check the graph holds exactly the tokens the parser produced.
#[test]
fn every_parsed_token_is_stored() {
    let m = manifest();
    let db = tempfile::tempdir().unwrap();
    let mut store = Store::open(db.path().join("corpus.redb")).unwrap();
    store.register(Box::new(graph_lang_rust::RustExtractor));
    let mut expected: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut expected_by_lang: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    let mut rust_symbols = 0;
    for app in m["applications"].as_array().unwrap() {
        let org = app["name"].as_str().unwrap();
        for repo in strings(&app["repos"]) {
            let dir = corpus_dir().join(repo);
            // One write transaction per repo instead of one per file.
            let rels = files(&dir);
            let contents: Vec<Vec<u8>> = rels
                .iter()
                .map(|rel| std::fs::read(dir.join(rel)).unwrap())
                .collect();
            let batch: Vec<BatchFile> = rels
                .iter()
                .zip(&contents)
                .map(|(rel, bytes)| BatchFile {
                    path: rel,
                    bytes,
                    language: None,
                    origin: None,
                })
                .collect();
            let stats = store.index_batch(org, repo, &batch).unwrap();
            for ((rel, bytes), st) in rels.iter().zip(&contents).zip(stats) {
                let st = st.unwrap();
                let src = std::str::from_utf8(bytes).unwrap();
                let lang = detect_language_from_content(rel, src);
                assert!(
                    !st.has_errors || lang == "rust",
                    "{repo}/{rel} unexpectedly flagged has_errors"
                );
                if lang == "rust" {
                    rust_symbols += st.symbols;
                }
                // Stored tokens are identical to the parser's, in text, class and span.
                let stored = store.file_tokens(org, repo, rel).unwrap().unwrap();
                let parsed = tokenize(src);
                assert_eq!(stored.len(), parsed.len(), "{repo}/{rel}");
                for (a, b) in stored.iter().zip(&parsed) {
                    assert_eq!(
                        (&a.name, a.token_class, a.span),
                        (&b.text, Some(b.class), Some(b.span)),
                        "{repo}/{rel}"
                    );
                }
                *expected.entry((org.into(), repo.into())).or_default() += parsed.len();
                *expected_by_lang
                    .entry((org.into(), repo.into(), lang))
                    .or_default() += parsed.len();
            }
        }
    }
    // Totals per repo and per language agree with what `describe` reports.
    for info in store.describe(None, None).unwrap() {
        let total: usize = info.languages.values().map(|l| l.tokens).sum();
        assert_eq!(
            total,
            expected[&(info.org.clone(), info.repo.clone())],
            "{}/{}",
            info.org,
            info.repo
        );
        for (lang, li) in &info.languages {
            assert_eq!(
                li.tokens,
                expected_by_lang[&(info.org.clone(), info.repo.clone(), lang.clone())]
            );
        }
    }
    assert!(
        rust_symbols > 50,
        "Rust extractor found only {rust_symbols} symbols in anyhow"
    );
    // Cross-repo, cross-language search works on real code.
    let hits = store.search(&Query::new("ITransport")).unwrap();
    let repos: BTreeSet<_> = hits.iter().filter_map(|h| h.repo.clone()).collect();
    assert!(
        repos.contains("rebus") && repos.contains("rebus-sqlserver"),
        "{repos:?}"
    );
    let mut q = Query::new("articles");
    q.language = Some("sql".into());
    let sql_hits = store.search(&q).unwrap();
    assert!(!sql_hits.is_empty());
    assert!(sql_hits
        .iter()
        .all(|h| h.repo.as_deref() == Some("conduit-sql")));
}

/// Classification spot-checks on real code: the tokenizer should keep string
/// literals, comments and identifiers apart in each language of the corpus.
#[test]
fn token_classes_are_sensible_on_real_code() {
    let classes = |repo: &str, file: &str| {
        let src = std::fs::read_to_string(corpus_dir().join(repo).join(file)).unwrap();
        tokenize(&src)
    };
    let sql = classes(
        "conduit-sql",
        "src/main/resources/db/migration/V1__create_tables.sql",
    );
    assert!(sql
        .iter()
        .any(|t| t.text.eq_ignore_ascii_case("create") && t.class == TokenClass::Identifier));
    let ts = classes(
        "conduit-ui",
        "src/app/features/article/services/articles.service.ts",
    );
    assert!(ts.iter().any(|t| t.text == "\"/articles\""
        || t.text == "'/articles'"
        || t.text.contains("/articles")));
    assert!(ts.iter().any(|t| t.class == TokenClass::Literal));
    let cs = classes("rebus", "Rebus/Transport/AbstractRebusTransport.cs");
    assert!(cs.iter().any(|t| t.class == TokenClass::Comment));
    assert!(cs
        .iter()
        .any(|t| t.text == "AbstractRebusTransport" && t.class == TokenClass::Identifier));
}
