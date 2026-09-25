//! `detect_format`: which layout wrote a file, and that it never writes.
use crate::{detect_format, StoreError, LEGACY_SCHEMA_VERSIONS, SCHEMA_VERSION};
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
    assert_eq!(detect_format(&d.path().join("nope")).unwrap(), None);
    let e = d.path().join("empty");
    std::fs::write(&e, b"").unwrap();
    assert_eq!(detect_format(&e).unwrap(), None);
    assert_eq!(std::fs::metadata(&e).unwrap().len(), 0);
    assert_eq!(detect_format(d.path()).unwrap(), None);
}

#[test]
fn garbage_file_is_open_failed_and_untouched() {
    let d = tempfile::tempdir().unwrap();
    let g = d.path().join("g");
    std::fs::write(&g, vec![0x5a; 4096]).unwrap();
    let before = digest(&g);
    assert!(matches!(
        detect_format(&g),
        Err(StoreError::OpenFailed { .. })
    ));
    assert_eq!(digest(&g), before);
}

#[test]
fn held_lock_is_locked() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l");
    make(&p, Some(SCHEMA_VERSION));
    let before = digest(&p);
    let held = redb::Database::create(&p).unwrap();
    assert!(matches!(detect_format(&p), Err(StoreError::Locked(_))));
    drop(held);
    assert_eq!(digest(&p), before);
}

#[test]
fn schema_versions_map_to_current_legacy_or_mismatch() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("current");
    make(&p, Some(SCHEMA_VERSION));
    let before = digest(&p);
    assert_eq!(detect_format(&p).unwrap(), Some(SCHEMA_VERSION));
    assert_eq!(digest(&p), before, "current bytes");
    for v in LEGACY_SCHEMA_VERSIONS {
        let p = d.path().join(format!("v{v}"));
        make(&p, Some(v));
        let before = digest(&p);
        match detect_format(&p) {
            Err(StoreError::LegacyFormat { path, version }) => {
                assert_eq!(version, v);
                assert!(path.ends_with(&format!("v{v}")), "{path}");
            }
            other => panic!("v{v}: {other:?}"),
        }
        assert_eq!(digest(&p), before, "v{v} bytes");
    }
    for v in [0, 3, 4, 5, 6, 7, 8, SCHEMA_VERSION + 1] {
        let p = d.path().join(format!("bad{v}"));
        make(&p, Some(v));
        let before = digest(&p);
        assert!(
            matches!(detect_format(&p), Err(StoreError::SchemaMismatch { found }) if found == v),
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
    assert_eq!(detect_format(&p).unwrap(), None);
    assert_eq!(digest(&p), before);
    // A meta table with no schema_version row is also None.
    let q = d.path().join("norow");
    make(&q, None);
    assert_eq!(detect_format(&q).unwrap(), None);
}
