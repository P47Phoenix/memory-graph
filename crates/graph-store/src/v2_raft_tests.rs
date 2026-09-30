//! ADR 0004 D5/D7 on v2: the `raft_sm` table (lazily created, never by
//! `open`), the marker's golden bytes and monotonic rule, the marked writes
//! as one transaction, and `export_snapshot`/`install_snapshot`.
use super::*;
use crate::v2::RAFT_SM;
use crate::v2_tests::span_ext;
use std::collections::HashSet;

fn marker(index: u64) -> RaftMarker {
    RaftMarker {
        term: 1,
        index,
        node_id: 7,
    }
}

fn bf<'a>(path: &'a str, bytes: &'a [u8], language: &'a str) -> BatchFile<'a> {
    BatchFile {
        path,
        bytes,
        language: Some(language),
        origin: Some(ORIGIN_DIRECTORY),
        ..Default::default()
    }
}

fn prepared(s: &V2Store, files: &[BatchFile<'_>]) -> Vec<PreparedFile> {
    files
        .iter()
        .map(|f| s.prepare("o", "r", f, IndexOptions::default()).unwrap())
        .collect()
}

/// Whether the `raft_sm` table exists at all (not just whether it holds a
/// marker): an embedded store must never create it.
fn has_raft_table(s: &V2Store) -> bool {
    let rt = s.db.begin_read().unwrap();
    match rt.open_table(RAFT_SM) {
        Ok(_) => true,
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => panic!("{e}"),
    }
}

/// Every read surface answers the same on both stores.
fn assert_same_answers(a: &V2Store, b: &V2Store) {
    assert_eq!(
        a.describe(None, None).unwrap(),
        b.describe(None, None).unwrap()
    );
    assert_eq!(
        a.describe_by_scan(None, None).unwrap(),
        b.describe_by_scan(None, None).unwrap()
    );
    for k in [
        NodeKind::Org,
        NodeKind::Repo,
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Token,
    ] {
        assert_eq!(
            a.count_nodes(k).unwrap(),
            b.count_nodes(k).unwrap(),
            "{k:?}"
        );
    }
    for text in ["alpha", "beta", "gamma", "missing"] {
        for grain in [Grain::Token, Grain::Symbol, Grain::File, Grain::Repo] {
            let mut q = Query::new(text);
            q.grain = grain;
            assert_eq!(
                a.search(&q).unwrap(),
                b.search(&q).unwrap(),
                "{text}/{grain:?}"
            );
        }
    }
    assert_eq!(
        a.search_symbols(&SymbolQuery::new("*")).unwrap(),
        b.search_symbols(&SymbolQuery::new("*")).unwrap()
    );
    assert_eq!(
        a.file_tokens("o", "r", "a.txt").unwrap(),
        b.file_tokens("o", "r", "a.txt").unwrap()
    );
    assert_eq!(a.raft_marker().unwrap(), b.raft_marker().unwrap());
    assert_eq!(a.raft_membership().unwrap(), b.raft_membership().unwrap());
}

#[test]
fn raft_marker_golden_bytes() {
    let m = RaftMarker {
        term: 0x0102_0304_0506_0708,
        index: 2,
        node_id: 0xff,
    };
    let bytes = m.encode();
    assert_eq!(
        bytes,
        [
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // term, little-endian
            2, 0, 0, 0, 0, 0, 0, 0, // index
            0xff, 0, 0, 0, 0, 0, 0, 0, // node id
        ]
    );
    assert_eq!(RaftMarker::decode(&bytes).unwrap(), m);
    assert_eq!(RaftMarker::ENCODED_LEN, 24);
    assert_eq!(RaftMarker::default().encode(), [0u8; 24]);
    for bad in [&[][..], &bytes[..23], &[0u8; 25][..]] {
        assert!(
            matches!(RaftMarker::decode(bad), Err(StoreError::Corrupt(_))),
            "{} bytes must be Corrupt",
            bad.len()
        );
    }
}

#[test]
fn marker_absent_on_fresh_store_and_after_plain_reopen() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.raft_marker().unwrap(), None);
    assert_eq!(s.raft_membership().unwrap(), None);
    assert!(!has_raft_table(&s), "open must not create raft_sm");
    s.index_bytes("o", "r", "a.txt", b"alpha beta", None)
        .unwrap();
    s.index_batch(
        "o",
        "r",
        &[bf("b.txt", b"beta", "text")],
        IndexOptions::default(),
    )
    .unwrap();
    s.vacuum().unwrap();
    s.prune_files("o", "r", &HashSet::new(), false).unwrap();
    assert!(!has_raft_table(&s), "plain writes must not create raft_sm");
    drop(s);
    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.raft_marker().unwrap(), None);
    assert!(!has_raft_table(&s), "reopen must not create raft_sm");
    let (s, _) = s.compact().unwrap();
    assert!(
        !has_raft_table(&s),
        "compact of an unmarked store adds no raft_sm"
    );
}

#[test]
fn marked_index_creates_the_table_lazily_and_persists_the_marker() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    let s = V2Store::open(&p).unwrap();
    let files = prepared(&s, &[bf("a.txt", b"alpha beta", "text")]);
    let out = s
        .index_prepared_marked(
            "o",
            "r",
            files,
            IndexOptions::default(),
            marker(5),
            Some(b"m1"),
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].as_ref().unwrap().tokens, 2);
    assert!(has_raft_table(&s));
    assert_eq!(s.raft_marker().unwrap(), Some(marker(5)));
    assert_eq!(s.raft_membership().unwrap(), Some(b"m1".to_vec()));
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    // Membership is kept when a later entry carries none.
    s.mark_only(marker(6), None).unwrap();
    assert_eq!(s.raft_marker().unwrap(), Some(marker(6)));
    assert_eq!(s.raft_membership().unwrap(), Some(b"m1".to_vec()));
    drop(s);
    let s = V2Store::open(&p).unwrap();
    assert_eq!(s.raft_marker().unwrap(), Some(marker(6)));
    assert_eq!(s.raft_membership().unwrap(), Some(b"m1".to_vec()));
    assert_eq!(s.search(&Query::new("alpha")).unwrap().len(), 1);
    s.check_consistency(false);
}

#[test]
fn every_marked_write_refuses_a_non_increasing_marker_without_writing() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("g.redb")).unwrap();
    s.mark_only(marker(10), Some(b"m1")).unwrap();
    let ex = span_ext(&[("S", SymbolKind::Function, 0, 5)], &[("gamma", 1, 2)]);
    let refused = |r: Result<()>| match r {
        Err(StoreError::AlreadyApplied { index }) => assert_eq!(index, 10),
        other => panic!("expected the marker rejection, got {other:?}"),
    };
    // Equal and lower indexes are both refused (the error names the
    // refused entry's index).
    for index in [10, 3] {
        let files = prepared(&s, &[bf("a.txt", b"alpha", "text")]);
        match s.index_prepared_marked(
            "o",
            "r",
            files,
            IndexOptions::default(),
            marker(index),
            Some(b"m2"),
        ) {
            Err(e @ StoreError::AlreadyApplied { index: got }) => {
                assert_eq!(got, index);
                assert_eq!(
                    e.to_string(),
                    format!("raft marker {index} already applied")
                );
            }
            other => panic!("expected the marker rejection, got {other:?}"),
        }
    }
    refused(
        s.ingest_file_marked(
            "o",
            "r",
            "b.txt",
            "text",
            &ex,
            None,
            marker(10),
            Some(b"m2"),
        )
        .map(|_| ()),
    );
    refused(s.mark_only(marker(10), Some(b"m2")));
    refused(s.vacuum_marked(marker(10), Some(b"m2")).map(|_| ()));
    refused(
        s.prune_files_marked("o", "r", &HashSet::new(), marker(10), Some(b"m2"))
            .map(|_| ()),
    );
    // Nothing was written: no file, marker and membership as before.
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert_eq!(s.raft_marker().unwrap(), Some(marker(10)));
    assert_eq!(s.raft_membership().unwrap(), Some(b"m1".to_vec()));
    // The next index is accepted by each method in turn.
    s.ingest_file_marked("o", "r", "b.txt", "text", &ex, None, marker(11), None)
        .unwrap();
    let out = s
        .index_prepared_marked(
            "o",
            "r",
            prepared(&s, &[bf("a.txt", b"alpha", "text")]),
            IndexOptions::default(),
            marker(12),
            Some(b"m3"),
        )
        .unwrap();
    assert!(out[0].is_ok());
    assert_eq!(s.raft_membership().unwrap(), Some(b"m3".to_vec()));
    // Replace a.txt so a term is dead, then vacuum with a marker: the
    // transaction commits (marker advances) even though a plain vacuum with
    // nothing dead would abort.
    s.index_prepared_marked(
        "o",
        "r",
        prepared(&s, &[bf("a.txt", b"beta", "text")]),
        IndexOptions::default(),
        marker(13),
        None,
    )
    .unwrap();
    let v = s.vacuum_marked(marker(14), None).unwrap();
    assert_eq!(v.terms_removed, 1, "alpha was dead");
    let v = s.vacuum_marked(marker(15), None).unwrap();
    assert_eq!(v.terms_removed, 0);
    assert_eq!(s.raft_marker().unwrap(), Some(marker(15)));
    let removed = s
        .prune_files_marked("o", "r", &HashSet::new(), marker(16), None)
        .unwrap();
    assert_eq!(removed, ["a.txt"], "only the directory-origin file");
    assert_eq!(s.raft_marker().unwrap(), Some(marker(16)));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    s.check_consistency(false);
    // The pruned file's terms are dead until the next vacuum, which again
    // commits with its marker.
    let v = s.vacuum_marked(marker(17), None).unwrap();
    assert!(v.terms_removed > 0);
    assert_eq!(s.raft_marker().unwrap(), Some(marker(17)));
    s.check_consistency(true);
}

/// A marked batch is one transaction whatever the store's chunk cap: a
/// storage error part-way (a NUL in a file's language poisons the
/// transaction, the same mechanism as `run_crash_rerun_differential`)
/// leaves nothing behind, not even the table, where the plain
/// `index_prepared` with the same chunk cap had already committed the
/// files before the poison.
#[test]
fn marked_index_is_one_transaction_whatever_the_chunk_cap() {
    let d = tempfile::tempdir().unwrap();
    let mut s = V2Store::open(d.path().join("g.redb")).unwrap();
    s.set_chunk_bytes(1);
    let files = [
        bf("a.txt", b"alpha", "text"),
        bf("b.txt", b"beta", "text"),
        bf("bad.txt", b"gamma", "poi\0son"),
        bf("c.txt", b"delta", "text"),
    ];
    let err = s
        .index_prepared_marked(
            "o",
            "r",
            prepared(&s, &files),
            IndexOptions::default(),
            marker(1),
            Some(b"m"),
        )
        .expect_err("the poisoned batch must fail");
    assert!(matches!(err, StoreError::Rejected(_)), "{err}");
    assert_eq!(
        s.count_nodes(NodeKind::File).unwrap(),
        0,
        "nothing committed"
    );
    assert_eq!(s.raft_marker().unwrap(), None);
    assert!(
        !has_raft_table(&s),
        "the aborted transaction created no table"
    );
    assert!(!s.describe(None, None).unwrap().iter().any(|r| r.open_batch));

    // Control: the plain path with the same cap commits per file.
    let plain = s
        .index_prepared("o", "r", prepared(&s, &files), IndexOptions::default())
        .expect_err("still poisoned");
    assert!(matches!(plain, StoreError::Rejected(_)));
    assert_eq!(
        s.count_nodes(NodeKind::File).unwrap(),
        2,
        "a and b committed per chunk"
    );
    assert!(s.describe(None, None).unwrap().iter().any(|r| r.open_batch));
    assert!(!has_raft_table(&s));

    // A good marked batch under the same cap: all files, marker, no open batch.
    let good = [files[0], files[1], files[3]];
    let out = s
        .index_prepared_marked(
            "o",
            "r",
            prepared(&s, &good),
            IndexOptions::default(),
            marker(1),
            Some(b"m"),
        )
        .unwrap();
    assert!(out.iter().all(Result::is_ok));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    assert_eq!(s.raft_marker().unwrap(), Some(marker(1)));
    assert!(!s.describe(None, None).unwrap().iter().any(|r| r.open_batch));
    s.check_consistency(false);
}

/// `commit_each` itself forces one transaction for a marked batch even when
/// a caller passes a small chunk cap (the guarantee is not a debug-only
/// assertion): a poison part-way leaves nothing committed.
#[test]
fn commit_each_ignores_the_chunk_cap_for_a_marked_batch() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("g.redb")).unwrap();
    let files = [
        bf("a.txt", b"alpha", "text"),
        bf("b.txt", b"beta", "text"),
        bf("bad.txt", b"gamma", "poi\0son"),
    ];
    let mut it = prepared(&s, &files).into_iter();
    let err = s
        .commit_each(
            "o",
            "r",
            files.len(),
            IndexOptions::default(),
            1,
            Some((marker(1), None)),
            |_, _| Ok(it.next().unwrap()),
        )
        .expect_err("the poisoned batch must fail");
    assert!(matches!(err, StoreError::Rejected(_)), "{err}");
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert_eq!(s.raft_marker().unwrap(), None);
}

#[test]
fn compact_keeps_the_marker_and_membership() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("g.redb")).unwrap();
    s.index_prepared_marked(
        "o",
        "r",
        prepared(&s, &[bf("a.txt", b"alpha beta", "text")]),
        IndexOptions::default(),
        marker(3),
        Some(b"members"),
    )
    .unwrap();
    let before = s.search(&Query::new("alpha")).unwrap();
    let (s, _) = s.compact().unwrap();
    assert_eq!(s.raft_marker().unwrap(), Some(marker(3)));
    assert_eq!(s.raft_membership().unwrap(), Some(b"members".to_vec()));
    assert_eq!(s.search(&Query::new("alpha")).unwrap(), before);
    // And the rule still holds on the compacted file.
    assert!(s.mark_only(marker(3), None).is_err());
    s.mark_only(marker(4), None).unwrap();
    s.check_consistency(false);
}

#[test]
fn export_snapshot_equals_compact_and_keeps_the_store_open() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.redb");
    let s = V2Store::open(&p).unwrap();
    s.ingest_file(
        "o",
        "r",
        "a.txt",
        "rust",
        &span_ext(
            &[
                ("S", SymbolKind::Type, 0, 20),
                ("m", SymbolKind::Method, 2, 10),
            ],
            &[("alpha", 3, 8), ("beta", 11, 15)],
        ),
    )
    .unwrap();
    s.index_bytes("o", "r", "b.txt", b"beta gamma", None)
        .unwrap();
    s.index_bytes("o", "r", "b.txt", b"gamma", None).unwrap(); // dead term
    s.mark_only(marker(2), Some(b"m")).unwrap();

    let snap = d.path().join("snap.redb");
    let stats = s.export_snapshot(&snap).unwrap();
    assert!(stats.before_bytes > 0 && stats.after_bytes > 0);
    assert_eq!(
        stats.before_bytes,
        std::fs::metadata(&p).unwrap().len(),
        "the source is measured"
    );
    assert_eq!(stats.after_bytes, std::fs::metadata(&snap).unwrap().len());
    // The exporting store is still open and writable.
    s.index_bytes("o", "r", "c.txt", b"delta", None).unwrap();
    assert_eq!(s.search(&Query::new("delta")).unwrap().len(), 1);
    // A second export to the same path is refused, and leaves it alone.
    let len = std::fs::metadata(&snap).unwrap().len();
    assert!(matches!(
        s.export_snapshot(&snap),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(std::fs::metadata(&snap).unwrap().len(), len);

    // The snapshot is the state as of the export, marker included; compact
    // the original (the same copy loop) and compare the two.
    let (s, _) = s.compact().unwrap();
    let opened = V2Store::open(&snap).unwrap();
    assert!(
        opened.search(&Query::new("delta")).unwrap().is_empty(),
        "frozen at export"
    );
    assert_eq!(opened.raft_marker().unwrap(), Some(marker(2)));
    assert_eq!(opened.raft_membership().unwrap(), Some(b"m".to_vec()));
    assert_eq!(opened.count_nodes(NodeKind::File).unwrap(), 2);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    // Bring the snapshot level with the original and they must be equal on
    // every surface, then keep being equal under the differential harness.
    opened
        .index_bytes("o", "r", "c.txt", b"delta", None)
        .unwrap();
    assert_same_answers(&s, &opened);
    opened.check_consistency(false);
    conformance::run_differential(&s, &opened);
}

#[test]
fn install_snapshot_then_open_answers_identically() {
    let d = tempfile::tempdir().unwrap();
    let src_path = d.path().join("g.redb");
    let s = V2Store::open(&src_path).unwrap();
    s.index_prepared_marked(
        "o",
        "r",
        prepared(
            &s,
            &[
                bf("a.txt", b"alpha beta", "text"),
                bf("b.txt", b"gamma", "text"),
            ],
        ),
        IndexOptions::default(),
        marker(9),
        Some(b"m"),
    )
    .unwrap();
    let snap = d.path().join("snap.redb");
    s.export_snapshot(&snap).unwrap();

    // Into a path with no store yet.
    let fresh = d.path().join("fresh.redb");
    V2Store::install_snapshot(&fresh, &snap).unwrap();
    assert!(!snap.exists(), "src is consumed");
    let f = V2Store::open(&fresh).unwrap();
    assert_same_answers(&s, &f);
    f.check_consistency(false);
    drop(f);

    // Over an existing (closed) store holding other data.
    let other = d.path().join("other.redb");
    {
        let o = V2Store::open(&other).unwrap();
        o.index_bytes("x", "y", "z.txt", b"zeta", None).unwrap();
        o.mark_only(marker(100), None).unwrap();
    }
    s.export_snapshot(&snap).unwrap();
    V2Store::install_snapshot(&other, &snap).unwrap();
    assert!(!snap.exists());
    let old = d.path().join("other.redb.old");
    assert!(!old.exists(), "the moved-aside copy is removed on success");
    let o = V2Store::open(&other).unwrap();
    assert!(o.search(&Query::new("zeta")).unwrap().is_empty());
    assert_same_answers(&s, &o);
    assert_eq!(
        o.raft_marker().unwrap(),
        Some(marker(9)),
        "marker from the snapshot"
    );
    drop(o);

    // Not a store: refused, and the target is untouched.
    let junk = d.path().join("junk.redb");
    std::fs::write(&junk, b"not a database").unwrap();
    let before = std::fs::read(&other).unwrap();
    let e = V2Store::install_snapshot(&other, &junk).expect_err("junk is refused");
    assert!(
        matches!(e, StoreError::OpenFailed { .. } | StoreError::Rejected(_)),
        "{e}"
    );
    assert_eq!(std::fs::read(&other).unwrap(), before);
    assert!(junk.exists());
    // An empty (schema-less) redb file is refused too.
    let empty = d.path().join("empty.redb");
    drop(redb::Database::create(&empty).unwrap());
    assert!(matches!(
        V2Store::install_snapshot(&other, &empty),
        Err(StoreError::Rejected(_))
    ));
    // A retired-format file is refused with its own error, not installed.
    let legacy = d.path().join("legacy.redb");
    crate::tests::stamped_file(&legacy, 2);
    assert!(matches!(
        V2Store::install_snapshot(&other, &legacy),
        Err(StoreError::LegacyFormat { .. })
    ));
    assert_eq!(std::fs::read(&other).unwrap(), before);
}

/// The failpoint hook runs inside every marked write's transaction, before
/// its commit: an `Err` leaves neither data nor marker behind, and the
/// same entry applies normally once the hook is gone (stage B's
/// `kill_during_apply_reapplies_exactly_once` relies on exactly this).
#[test]
fn marked_commit_hook_aborts_the_whole_entry() {
    use std::sync::{Arc, Mutex};
    let d = tempfile::tempdir().unwrap();
    let mut s = V2Store::open(d.path().join("g.redb")).unwrap();
    s.mark_only(marker(1), None).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    s.set_marked_commit_hook(Some(Arc::new(move |m: &RaftMarker| {
        seen2.lock().unwrap().push(m.index);
        if m.index == 2 {
            Err(StoreError::Storage("failpoint".into()))
        } else {
            Ok(())
        }
    })));
    let files = [bf("a.rs", b"fn a() {}", "rust")];
    let p = prepared(&s, &files);
    let e = s
        .index_prepared_marked("o", "r", p, IndexOptions::default(), marker(2), None)
        .unwrap_err();
    assert!(matches!(e, StoreError::Storage(_)), "{e:?}");
    assert_eq!(s.raft_marker().unwrap(), Some(marker(1)));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 0);
    assert!(s
        .prune_files_marked("o", "r", &HashSet::new(), marker(2), None)
        .is_err());
    assert!(s.vacuum_marked(marker(2), None).is_err());
    assert!(s.mark_only(marker(2), None).is_err());
    assert_eq!(s.raft_marker().unwrap(), Some(marker(1)));
    s.set_marked_commit_hook(None);
    let p = prepared(&s, &files);
    s.index_prepared_marked("o", "r", p, IndexOptions::default(), marker(2), None)
        .unwrap();
    assert_eq!(s.raft_marker().unwrap(), Some(marker(2)));
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
    assert_eq!(*seen.lock().unwrap(), vec![2, 2, 2, 2]);
}

/// `clear_raft_state` drops the marker and membership (a restored store
/// starts a new log) and is a no-op without the table.
#[test]
fn clear_raft_state_drops_the_table_and_is_a_noop_without_it() {
    let d = tempfile::tempdir().unwrap();
    let s = V2Store::open(d.path().join("g.redb")).unwrap();
    s.clear_raft_state().unwrap();
    assert!(!has_raft_table(&s));
    let files = [bf("a.rs", b"fn a() {}", "rust")];
    let p = prepared(&s, &files);
    s.index_prepared_marked(
        "o",
        "r",
        p,
        IndexOptions::default(),
        marker(3),
        Some(b"m".as_slice()),
    )
    .unwrap();
    assert!(has_raft_table(&s));
    s.clear_raft_state().unwrap();
    assert!(!has_raft_table(&s));
    assert_eq!(s.raft_marker().unwrap(), None);
    assert_eq!(s.raft_membership().unwrap(), None);
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 1);
}
