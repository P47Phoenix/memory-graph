//! `--backend` selection (ADR 0003 story 8): v1 stays the default and its
//! behaviour is unchanged, v2 is opt-in, and the database's schema version
//! validates the choice with a clear error on a mismatch.
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

#[test]
fn v1_output_is_identical_with_and_without_the_flag() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let plain = db_in(d.path(), "plain");
    let flagged = db_in(d.path(), "flagged");
    let (ok, _, e) = run(&["--db", &plain, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let (ok, _, e) = run(&[
        "--db",
        &flagged,
        "--backend",
        "v1",
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
    assert_eq!(a, read_all(&plain, Some("v1")));
    assert_eq!(a, read_all(&flagged, None));
}

#[test]
fn v2_matches_v1_output_on_every_read() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let v1 = db_in(d.path(), "one");
    let v2 = db_in(d.path(), "two");
    let (ok, _, e) = run(&["--db", &v1, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let (ok, out, e) = run(&[
        "--db",
        &v2,
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
    assert!(out.contains("indexed"), "{out}");
    assert_eq!(read_all(&v1, None), read_all(&v2, Some("v2")));
    // Re-indexing an unchanged directory into v2 skips every file.
    let (ok, out, e) = run(&[
        "--db",
        &v2,
        "--backend",
        "v2",
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--json",
        &dir,
    ]);
    assert!(ok, "{e}");
    let j: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(j["unchanged"], 2, "{out}");
    // index-file works on v2 too.
    let f = Path::new(&dir).join("a.rs");
    let (ok, out, e) = run(&[
        "--db",
        &v2,
        "--backend",
        "v2",
        "index-file",
        "--org",
        "o2",
        "--repo",
        "r2",
        f.to_str().unwrap(),
    ]);
    assert!(ok, "{e}");
    assert!(out.contains("indexed"), "{out}");
}

#[test]
fn a_v2_database_needs_the_flag() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let v2 = db_in(d.path(), "two");
    let (ok, _, e) = run(&[
        "--db",
        &v2,
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
    let before = std::fs::read(&v2).unwrap();
    for args in [
        vec!["describe"],
        vec!["search", "bar"],
        vec!["symbols", "foo"],
        vec!["vacuum"],
        vec!["index", "--org", "o", "--repo", "r", dir.as_str()],
    ] {
        let mut v = vec!["--db", v2.as_str()];
        v.extend(args.iter().copied());
        let (ok, out, err) = run(&v);
        assert!(!ok, "{args:?}: {out}");
        assert!(out.is_empty(), "{args:?}: {out}");
        assert!(
            err.contains("is a v2 database") && err.contains("pass `--backend v2`"),
            "{args:?}: {err}"
        );
    }
    // Explicit v1 is refused the same way, and nothing was written.
    let (ok, _, err) = run(&["--db", &v2, "--backend", "v1", "describe"]);
    assert!(!ok);
    assert!(err.contains("is a v2 database"), "{err}");
    assert!(
        err.contains("drop --backend or pass `--backend v2`"),
        "{err}"
    );
    assert_eq!(std::fs::read(&v2).unwrap(), before);
}

#[test]
fn a_v1_database_is_refused_by_backend_v2() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    let v1 = db_in(d.path(), "one");
    let (ok, _, e) = run(&["--db", &v1, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let before = std::fs::read(&v1).unwrap();
    for args in [
        vec!["describe"],
        vec!["search", "bar"],
        vec!["symbols", "foo"],
        vec!["vacuum"],
        vec!["index", "--org", "o", "--repo", "r", dir.as_str()],
    ] {
        let mut v = vec!["--db", v1.as_str(), "--backend", "v2"];
        v.extend(args.iter().copied());
        let (ok, out, err) = run(&v);
        assert!(!ok, "{args:?}: {out}");
        assert!(
            err.contains("is a v1 database (schema version 2)")
                && err.contains("the v2 backend was selected"),
            "{args:?}: {err}"
        );
    }
    assert_eq!(std::fs::read(&v1).unwrap(), before);
}

#[test]
fn missing_database_and_bad_backend_value() {
    let d = tempfile::tempdir().unwrap();
    let db = db_in(d.path(), "none");
    let (ok, _, err) = run(&["--db", &db, "--backend", "v2", "describe"]);
    assert!(!ok);
    assert!(err.contains("does not exist"), "{err}");
    assert!(!Path::new(&db).exists());
    let (ok, _, err) = run(&["--db", &db, "--backend", "v3", "describe"]);
    assert!(!ok);
    assert!(err.contains("invalid value 'v3'"), "{err}");
}

#[test]
fn vacuum_command() {
    let d = tempfile::tempdir().unwrap();
    let dir = corpus(d.path());
    // v2: a replaced file leaves dead terms; vacuum frees them, once.
    let v2 = db_in(d.path(), "two");
    let (ok, _, e) = run(&[
        "--db",
        &v2,
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
    let (ok, out, e) = run(&["--db", &v2, "--backend", "v2", "vacuum"]);
    assert!(ok, "{e}");
    assert!(
        out.starts_with("vacuum: removed 0 unused dictionary terms, kept "),
        "{out}"
    );
    std::fs::write(Path::new(&dir).join("b.zig"), "qux = 1\n").unwrap();
    let (ok, _, e) = run(&[
        "--db",
        &v2,
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
    let (ok, out, e) = run(&["--db", &v2, "--backend", "v2", "vacuum"]);
    assert!(ok, "{e}");
    assert!(
        !out.starts_with("vacuum: removed 0 "),
        "dead terms expected: {out}"
    );
    let (_, out, _) = run(&["--db", &v2, "--backend", "v2", "vacuum"]);
    assert!(out.starts_with("vacuum: removed 0 "), "{out}");
    let (ok, out, _) = run(&["--db", &v2, "--backend", "v2", "search", "qux"]);
    assert!(ok && out.contains("b.zig"), "{out}");
    // v1: a no-op that says so and leaves the file alone.
    let v1 = db_in(d.path(), "one");
    let (ok, _, e) = run(&["--db", &v1, "index", "--org", "o", "--repo", "r", &dir]);
    assert!(ok, "{e}");
    let before = std::fs::read(&v1).unwrap();
    let (ok, out, e) = run(&["--db", &v1, "vacuum"]);
    assert!(ok, "{e}");
    assert_eq!(
        out,
        "vacuum: nothing to do (a v1 database has no dictionary)\n"
    );
    assert_eq!(std::fs::read(&v1).unwrap(), before);
    // A missing database is an error, not a new empty file.
    let none = db_in(d.path(), "none");
    let (ok, _, err) = run(&["--db", &none, "vacuum"]);
    assert!(!ok && err.contains("does not exist"), "{err}");
}
