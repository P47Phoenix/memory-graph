//! One storage format (ADR 0003 D5): a database opens with no flag, the
//! retired `--backend v2` is a harmless no-op, `--backend v1` and a file in
//! the retired v1 format are refused with the migration hint, and refusals
//! never touch the file.
use std::path::Path;
use std::process::Command;

fn run(args: &[&str]) -> (bool, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args(args)
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

/// A small two-language repo directory.
fn corpus(root: &Path) -> String {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("a.rs"),
        "struct S;\nimpl S {\n    fn foo() { bar(); bar(); }\n}\nfn bar() {}\n",
    )
    .unwrap();
    std::fs::write(src.join("b.zig"), "pub fn foo() void { bar(); }\n").unwrap();
    src.to_string_lossy().into_owned()
}

fn db_in(root: &Path, name: &str) -> String {
    root.join(name).to_string_lossy().into_owned()
}

/// Every read command, as argument lists (after `--db <db> [--backend ..]`).
const READS: &[&[&str]] = &[
    &["describe"],
    &["describe", "--json"],
    &["describe", "--org", "o", "--repo", "r"],
    &["search", "bar"],
    &["search", "bar", "--json"],
    &["search", "foo", "--grain", "file"],
    &["search", "foo", "--grain", "symbol", "--json"],
    &["search", "bar", "--grain", "repo", "--limit", "1"],
    &["search", "fn", "--kind", "keyword", "--language", "rust"],
    &["symbols", "foo"],
    &["symbols", "*", "--json"],
    &["symbols", "f*", "--kind", "function"],
    &["symbols", "S", "--language", "rust", "--limit", "1"],
    &["symbols", "nothing"],
];

fn read_all(db: &str, backend: Option<&str>) -> Vec<(bool, String, String)> {
    READS
        .iter()
        .map(|args| {
            let mut v = vec!["--db", db];
            if let Some(b) = backend {
                v.extend(["--backend", b]);
            }
            v.extend(args.iter().copied());
            run(&v)
        })
        .collect()
}

/// A raw redb file stamped with a retired v1 schema version, as the last v1
/// release would have left it (the entity tables exist, the meta table names
/// the version). Built the way `graph-store`'s `detect_tests::make` does it.
fn legacy_v1_file(path: &Path, version: u64) {
    use redb::{Database, MultimapTableDefinition, TableDefinition};
    let db = Database::create(path).unwrap();
    let wt = db.begin_write().unwrap();
    {
        let mut m = wt
            .open_table(TableDefinition::<&str, u64>::new("meta"))
            .unwrap();
        m.insert("schema_version", version).unwrap();
        m.insert("next_id", 1).unwrap();
        m.insert("symbol_index_version", 1).unwrap();
        m.insert("catalog_version", 1).unwrap();
        wt.open_table(TableDefinition::<&str, u64>::new("catalog"))
            .unwrap();
        wt.open_table(TableDefinition::<u64, &[u8]>::new("nodes"))
            .unwrap();
        wt.open_table(TableDefinition::<&str, u64>::new("names"))
            .unwrap();
        wt.open_multimap_table(MultimapTableDefinition::<u64, u64>::new("children"))
            .unwrap();
        wt.open_multimap_table(MultimapTableDefinition::<&str, u64>::new("tokens_by_text"))
            .unwrap();
        wt.open_multimap_table(MultimapTableDefinition::<&str, u64>::new("symbols_by_name"))
            .unwrap();
    }
    wt.commit().unwrap();
}

#[test]
fn a_database_opens_with_no_flag_and_with_backend_v2() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let plain = db_in(d.path(), "plain");
    let flagged = db_in(d.path(), "flagged");
    let (ok, out, e) = run(&["--db", &plain, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    assert!(out.contains("indexed"), "{out}");
    let (ok, _, e) = run(&[
        "--db",
        &flagged,
        "--backend",
        "v2",
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        &dir,
    ]);
    assert!(ok, "{e}");
    let a = read_all(&plain, None);
    assert!(a.iter().all(|r| r.0), "{a:?}");
    assert!(a[0].1.contains("o/r: 2 files"), "{}", a[0].1);
    // Same bytes on stdout and stderr, flag or not, and on either file.
    assert_eq!(a, read_all(&plain, Some("v2")));
    assert_eq!(a, read_all(&flagged, None));
    assert_eq!(a, read_all(&flagged, Some("v2")));
    // Re-indexing an unchanged directory skips every file.
    let (ok, out, e) = run(&[
        "--db", &plain, "index", "--org", "o", "--repo", "r", "--json", &dir,
    ]);
    assert!(ok, "{e}");
    let j: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(j["unchanged"], 2, "{out}");
    // index-file works too.
    let f = Path::new(&dir).join("a.rs");
    let (ok, out, e) = run(&[
        "--db",
        &plain,
        "index-file",
        "--org",
        "o2",
        "--repo",
        "r2",
        f.to_str().unwrap(),
    ]);
    assert!(ok, "{e}");
    assert!(out.contains("indexed"), "{out}");
    // The flag is hidden from help.
    let (ok, out, _) = run(&["--help"]);
    assert!(ok && !out.contains("--backend"), "{out}");
}

#[test]
fn a_v1_database_is_refused_with_a_migration_hint() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    for version in [1u64, 2] {
        let v1 = db_in(d.path(), &format!("one{version}"));
        legacy_v1_file(Path::new(&v1), version);
        let before = std::fs::read(&v1).unwrap();
        for args in [
            vec!["describe"],
            vec!["search", "bar"],
            vec!["symbols", "foo"],
            vec!["vacuum"],
            vec!["vacuum", "--compact"],
            vec!["export"],
            vec!["index", "--org", "o", "--repo", "r", dir.as_str()],
            vec!["index-file", "--org", "o", "--repo", "r", "src/a.rs"],
        ] {
            for backend in [None, Some("v2")] {
                let mut v = vec!["--db", v1.as_str()];
                if let Some(b) = backend {
                    v.extend(["--backend", b]);
                }
                v.extend(args.iter().copied());
                let o = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
                    .args(&v)
                    .current_dir(d.path())
                    .output()
                    .unwrap();
                let out = String::from_utf8_lossy(&o.stdout);
                let err = String::from_utf8_lossy(&o.stderr);
                assert!(!o.status.success(), "{args:?}: {out}");
                assert!(out.is_empty(), "{args:?}: {out}");
                for needle in [
                    "retired v1 format",
                    &format!("schema version {version}"),
                    "reads only the v2 format",
                    "Re-index",
                    "v1-last",
                    "memory-graph migrate",
                    &format!("one{version}"),
                ] {
                    assert!(
                        err.contains(needle),
                        "{args:?} {backend:?}: {needle}: {err}"
                    );
                }
            }
        }
        assert_eq!(std::fs::read(&v1).unwrap(), before, "v{version} untouched");
    }
}

#[test]
fn backend_v1_is_refused_even_without_a_database() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let db = db_in(d.path(), "none");
    for args in [
        vec!["describe"],
        vec!["index", "--org", "o", "--repo", "r", dir.as_str()],
    ] {
        let mut v = vec!["--db", db.as_str(), "--backend", "v1"];
        v.extend(args.iter().copied());
        let (ok, out, err) = run(&v);
        assert!(!ok, "{args:?}: {out}");
        assert!(out.is_empty(), "{args:?}: {out}");
        for needle in ["--backend v1", "retired", "v1-last", "Re-index"] {
            assert!(err.contains(needle), "{args:?}: {needle}: {err}");
        }
        assert!(!Path::new(&db).exists(), "nothing created");
    }
}

#[test]
fn missing_database_and_bad_backend_value() {
    let d = tempfile::tempdir().unwrap();
    let db = db_in(d.path(), "none");
    let (ok, _, err) = run(&["--db", &db, "--backend", "v2", "describe"]);
    assert!(!ok);
    assert!(err.contains("does not exist"), "{err}");
    assert!(!Path::new(&db).exists());
    let (ok, _, err) = run(&["--db", &db, "describe"]);
    assert!(!ok);
    assert!(err.contains("does not exist"), "{err}");
    let (ok, _, err) = run(&["--db", &db, "--backend", "v3", "describe"]);
    assert!(!ok);
    assert!(err.contains("invalid value 'v3'"), "{err}");
    let (ok, _, err) = run(&["--db", &db, "--backend", "v1", "describe"]);
    assert!(!ok);
    assert!(err.contains("v1-last"), "{err}");
}

#[test]
fn vacuum_command() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    // A replaced file leaves dead terms; vacuum frees them, once.
    let db = db_in(d.path(), "two");
    let (ok, _, e) = run(&["--db", &db, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let (ok, out, e) = run(&["--db", &db, "vacuum"]);
    assert!(ok, "{e}");
    assert!(
        out.starts_with("vacuum: removed 0 unused dictionary terms, kept "),
        "{out}"
    );
    assert!(!out.contains("compact:"), "{out}");
    std::fs::write(Path::new(&dir).join("b.zig"), "qux = 1\n").unwrap();
    let (ok, _, e) = run(&["--db", &db, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let (ok, out, e) = run(&["--db", &db, "vacuum"]);
    assert!(ok, "{e}");
    assert!(
        !out.starts_with("vacuum: removed 0 "),
        "dead terms expected: {out}"
    );
    let (_, out, _) = run(&["--db", &db, "vacuum"]);
    assert!(out.starts_with("vacuum: removed 0 "), "{out}");
    let (ok, out, _) = run(&["--db", &db, "search", "qux"]);
    assert!(ok && out.contains("b.zig"), "{out}");
    // --compact is always available and reports both steps.
    let (ok, out, e) = run(&["--db", &db, "vacuum", "--compact"]);
    assert!(ok, "{e}");
    assert!(out.contains("vacuum:") && out.contains("compact:"), "{out}");
    let (ok, out, _) = run(&["--db", &db, "search", "qux"]);
    assert!(ok && out.contains("b.zig"), "{out}");
    // A missing database is an error, not a new empty file.
    let none = db_in(d.path(), "none");
    let (ok, _, err) = run(&["--db", &none, "vacuum"]);
    assert!(!ok && err.contains("does not exist"), "{err}");
    assert!(!Path::new(&none).exists());
}
