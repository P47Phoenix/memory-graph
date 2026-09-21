//! `detect_backend`: which backend wrote a file, and that it never writes.
use crate::{detect_backend, Backend, StoreError};
use sha2::{Digest, Sha256};
use std::path::Path;

fn digest(p: &Path) -> Vec<u8> {
    Sha256::digest(std::fs::read(p).unwrap()).to_vec()
}

/// A redb file whose meta table holds `version` (no row for `None`).
fn make(p: &Path, version: Option<u64>) {
    let db = redb::Database::create(p).unwrap();
    let wt = db.begin_write().unwrap();
    {
        let mut m = wt.open_table(crate::META).unwrap();
        if let Some(v) = version {
            m.insert("schema_version", v).unwrap();
        }
    }
    wt.commit().unwrap();
}

#[test]
fn missing_empty_and_directory_are_none() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(detect_backend(&d.path().join("nope")).unwrap(), None);
    let e = d.path().join("empty");
    std::fs::write(&e, b"").unwrap();
    assert_eq!(detect_backend(&e).unwrap(), None);
    assert_eq!(std::fs::metadata(&e).unwrap().len(), 0);
    assert_eq!(detect_backend(d.path()).unwrap(), None);
}

#[test]
fn garbage_file_is_open_failed_and_untouched() {
    let d = tempfile::tempdir().unwrap();
    let g = d.path().join("g");
    std::fs::write(&g, vec![0x5a; 4096]).unwrap();
    let before = digest(&g);
    assert!(matches!(
        detect_backend(&g),
        Err(StoreError::OpenFailed { .. })
    ));
    assert_eq!(digest(&g), before);
}

#[test]
fn held_lock_is_locked() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l");
    make(&p, Some(2));
    let before = digest(&p);
    let held = redb::Database::create(&p).unwrap();
    assert!(matches!(detect_backend(&p), Err(StoreError::Locked(_))));
    drop(held);
    assert_eq!(digest(&p), before);
}

#[test]
fn schema_versions_map_to_backends() {
    let d = tempfile::tempdir().unwrap();
    for (v, want) in [(1, Backend::Redb), (2, Backend::Redb), (4, Backend::RedbV2)] {
        let p = d.path().join(format!("v{v}"));
        make(&p, Some(v));
        let before = digest(&p);
        assert_eq!(detect_backend(&p).unwrap(), Some((want, v)), "v{v}");
        assert_eq!(digest(&p), before, "v{v} bytes");
    }
    for v in [0, 3, 5] {
        let p = d.path().join(format!("bad{v}"));
        make(&p, Some(v));
        let before = digest(&p);
        assert!(
            matches!(detect_backend(&p), Err(StoreError::SchemaMismatch { found }) if found == v),
            "v{v}"
        );
        assert_eq!(digest(&p), before, "v{v} bytes");
    }
}

#[test]
fn redb_file_without_meta_table_is_none() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nometa");
    {
        let db = redb::Database::create(&p).unwrap();
        let wt = db.begin_write().unwrap();
        wt.open_table(crate::CATALOG).unwrap();
        wt.commit().unwrap();
    }
    let before = digest(&p);
    assert_eq!(detect_backend(&p).unwrap(), None);
    assert_eq!(digest(&p), before);
    // A meta table with no schema_version row is also None.
    let q = d.path().join("norow");
    make(&q, None);
    assert_eq!(detect_backend(&q).unwrap(), None);
}
