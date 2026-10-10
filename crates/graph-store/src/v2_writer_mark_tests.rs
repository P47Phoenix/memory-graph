//! Epic story 58 (#276): the derived tables (`sym_fold`, `refs`,
//! `content_files`) are rebuilt on open after a binary that does not
//! maintain them wrote to the file, and only then. The old binary is
//! simulated by `v2::LEGACY_WRITER`, which writes exactly as releases before
//! these tables did: no maintenance, and a repo catalog row of 0.
use super::*;
use crate::v2_tests::span_ext;
use graph_core::Extraction;
use std::collections::HashSet;
use std::path::Path;

fn sha(p: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(std::fs::read(p).unwrap()).to_vec()
}

fn reset_rebuild_counters() {
    crate::v2::SYM_FOLD_REBUILDS.with(|c| c.set(0));
    crate::v2::REFS_REBUILDS.with(|c| c.set(0));
}

/// `(refs rebuilds, sym_fold rebuilds)` on this thread since the last reset.
fn rebuild_counters() -> (usize, usize) {
    (
        crate::v2::REFS_REBUILDS.with(std::cell::Cell::get),
        crate::v2::SYM_FOLD_REBUILDS.with(std::cell::Cell::get),
    )
}

/// Run `f` as a binary that maintains no derived table would.
fn as_old_writer(p: &Path, f: impl FnOnce(&V2Store)) {
    let s = V2Store::open(p).unwrap();
    crate::v2::LEGACY_WRITER.with(|c| c.set(true));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&s)));
    crate::v2::LEGACY_WRITER.with(|c| c.set(false));
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

/// Whether the file at `p` shows an old writer, read without `V2Store::open`
/// (which would heal it).
fn detected(p: &Path) -> bool {
    let db = redb::Database::open(p).unwrap();
    crate::v2::old_writer_detected(&db).unwrap()
}

fn repo_rows(s: &V2Store) -> Vec<(String, u64)> {
    let rt = s.db.begin_read().unwrap();
    let cat = rt.open_table(crate::CATALOG).unwrap();
    let rows = cat
        .range("r\0".."r\u{1}")
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.value().to_string(), v.value())
        })
        .collect();
    rows
}

/// One type symbol per name, side by side.
fn cs(names: &[&str]) -> Extraction {
    let syms: Vec<(&str, SymbolKind, u32, u32)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, SymbolKind::Type, 10 * i as u32, 10 * i as u32 + 9))
        .collect();
    span_ext(&syms, &[])
}

/// Healed: one rebuild of each derived table, a consistent store, every
/// repo row marked, and later reopens neither rebuild nor write.
fn assert_old_writer_healed(p: &Path) {
    reset_rebuild_counters();
    let s = V2Store::open(p).unwrap();
    assert_eq!(rebuild_counters(), (1, 1), "one rebuild of each table");
    s.check_consistency(false);
    crate::conformance::assert_fold_agrees(&s, "old writer healed");
    assert!(repo_rows(&s)
        .iter()
        .all(|(_, v)| *v == crate::v2::DERIVED_WRITER_MARK));
    drop(s);
    assert!(!detected(p));
    let before = sha(p);
    reset_rebuild_counters();
    drop(V2Store::open(p).unwrap());
    drop(V2Store::open(p).unwrap());
    assert_eq!(rebuild_counters(), (0, 0), "a healed store rebuilt again");
    assert_eq!(sha(p), before, "a healed store was written on reopen");
}

/// The case no count catches: an old binary re-indexes a file in place with
/// a renamed symbol (same file id, same number of symbols, no new term).
/// Only the repo row it resets to 0 shows the write.
#[test]
fn an_old_writer_renaming_in_place_is_detected_and_rebuilt() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    {
        let s = V2Store::open(&p).unwrap();
        s.ingest_file("o", "r", "a.cs", "csharp", &cs(&["Alpha", "Beta"]))
            .unwrap();
        s.ingest_file("o", "r", "b.cs", "csharp", &cs(&["Gamma"]))
            .unwrap();
    }
    assert!(!detected(&p));
    as_old_writer(&p, |s| {
        s.ingest_file("o", "r", "a.cs", "csharp", &cs(&["Gamma", "Beta"]))
            .unwrap();
        assert_eq!(repo_rows(s), [("r\0o\0r".to_string(), 0)]);
    });
    assert!(detected(&p));
    assert_old_writer_healed(&p);
    let s = V2Store::open(&p).unwrap();
    let files = |pat: &str| -> Vec<String> {
        s.search_symbols(&SymbolQuery::new(pat))
            .unwrap()
            .into_iter()
            .map(|h| h.file)
            .collect()
    };
    assert_eq!(files("ALPHA"), Vec::<String>::new());
    assert_eq!(files("gamma"), ["a.cs", "b.cs"]);
}

/// An old binary that only removes files (a prune) writes no repo row; the
/// table lengths it leaves apart show it.
#[test]
fn an_old_writer_that_only_removes_is_detected_and_rebuilt() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    {
        let s = V2Store::open(&p).unwrap();
        for (path, names) in [("a.cs", &["Alpha"][..]), ("b.cs", &["Beta"][..])] {
            s.ingest_file_with_origin(
                "o",
                "r",
                path,
                "csharp",
                &cs(names),
                Some(crate::ORIGIN_DIRECTORY),
            )
            .unwrap();
        }
    }
    as_old_writer(&p, |s| {
        let keep: HashSet<String> = ["a.cs".to_string()].into();
        assert_eq!(s.prune_files("o", "r", &keep, false).unwrap(), ["b.cs"]);
        assert_eq!(repo_rows(s)[0].1, crate::v2::DERIVED_WRITER_MARK);
    });
    assert!(detected(&p));
    assert_old_writer_healed(&p);
    let s = V2Store::open(&p).unwrap();
    assert!(s
        .search_symbols(&SymbolQuery::new("beta"))
        .unwrap()
        .is_empty());
}

/// A store only the current binary wrote (ingest, replace, prune, vacuum,
/// several sessions) is never rebuilt on reopen, and the reopen writes
/// nothing.
#[test]
fn a_store_no_old_binary_wrote_is_not_rebuilt_or_written_on_reopen() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    reset_rebuild_counters();
    crate::conformance::old_writer_phase1(&V2Store::open(&p).unwrap());
    {
        let s = V2Store::open(&p).unwrap();
        crate::conformance::old_writer_phase2(&s);
        s.ingest_file("o", "r", "x.cs", "csharp", &cs(&["Widget", "widGet"]))
            .unwrap();
        s.vacuum().unwrap();
    }
    assert_eq!(rebuild_counters(), (0, 0));
    assert!(!detected(&p));
    let before = sha(&p);
    let s = V2Store::open(&p).unwrap();
    crate::conformance::assert_old_writer_phases(&s);
    drop(s);
    drop(V2Store::open(&p).unwrap());
    assert_eq!(rebuild_counters(), (0, 0), "a reopen rebuilt");
    assert_eq!(sha(&p), before, "a reopen wrote");
}

/// A crash part-way through the heal leaves the old write detected, so the
/// next open runs the rebuild again and answers correctly.
#[test]
fn a_crash_during_the_old_writer_rebuild_reruns_it_on_the_next_open() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    crate::conformance::old_writer_phase1(&V2Store::open(&p).unwrap());
    as_old_writer(&p, |s| crate::conformance::old_writer_phase2(s));
    for fail_after in [0, 1] {
        crate::v2::SYM_FOLD_REBUILD_FAIL_AFTER.with(|c| c.set(Some(fail_after)));
        let failed = V2Store::open(&p);
        crate::v2::SYM_FOLD_REBUILD_FAIL_AFTER.with(|c| c.set(None));
        let Err(StoreError::Storage(msg)) = failed else {
            panic!("the failpoint must fail the open");
        };
        assert!(msg.contains("failpoint"), "{msg}");
        assert!(detected(&p), "a crashed heal must stay detected");
    }
    assert_old_writer_healed(&p);
    crate::conformance::assert_old_writer_phases(&V2Store::open(&p).unwrap());
}

/// A file the story-57 binary wrote (stamps at version 1, repo rows 0) is
/// upgraded on its first open: one rebuild of each table, then the marks.
#[test]
fn a_file_from_before_the_writer_mark_is_upgraded_once() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.redb");
    crate::conformance::old_writer_phase1(&V2Store::open(&p).unwrap());
    {
        let db = redb::Database::open(&p).unwrap();
        let wt = db.begin_write().unwrap();
        {
            let mut m = wt.open_table(crate::META).unwrap();
            m.insert(crate::v2::DERIVED_VERSION_REFS_KEY, 1).unwrap();
            m.insert(crate::v2::DERIVED_VERSION_SYM_FOLD_KEY, 1)
                .unwrap();
            wt.open_table(crate::CATALOG)
                .unwrap()
                .insert("r\0zz\0old", 0)
                .unwrap();
        }
        wt.commit().unwrap();
    }
    assert_old_writer_healed(&p);
}

/// Golden values of the story-58 state (CLAUDE.md, on-disk versioning): the
/// mark in a repo's catalog row and the derived versions that introduced it.
#[test]
fn writer_mark_golden_values() {
    assert_eq!(crate::v2::DERIVED_WRITER_MARK, 1);
    assert_eq!(crate::v2::REFS_DERIVED_VERSION, 2);
    assert_eq!(crate::v2::SYM_FOLD_DERIVED_VERSION, 2);
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("v.redb")).unwrap();
    s.ingest_file("o", "r", "a.cs", "csharp", &cs(&["A"]))
        .unwrap();
    let rt = s.db.begin_read().unwrap();
    let cat = rt.open_table(crate::CATALOG).unwrap();
    let rows: Vec<(Vec<u8>, Vec<u8>)> = cat
        .range("r\0".."r\u{1}")
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            (
                k.value().as_bytes().to_vec(),
                v.value().to_le_bytes().to_vec(),
            )
        })
        .collect();
    assert_eq!(rows, [(b"r\0o\0r".to_vec(), vec![1, 0, 0, 0, 0, 0, 0, 0])]);
}
