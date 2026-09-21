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

#[test]
fn index_reopen_search() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let rs = d.path().join("a.rs");
    let zig = d.path().join("b.zig");
    std::fs::write(&rs, "fn foo() { foo() }\n").unwrap();
    std::fs::write(&zig, "pub fn foo() void {}\n").unwrap();
    for (o, r, f) in [("o1", "r1", &rs), ("o2", "r2", &zig)] {
        let (ok, out, err) = run(&[
            "--db",
            &db,
            "index-file",
            "--org",
            o,
            "--repo",
            r,
            f.to_str().unwrap(),
        ]);
        assert!(ok, "{out}{err}");
    }
    // Re-index leaves no duplicates.
    run(&[
        "--db",
        &db,
        "index-file",
        "--org",
        "o1",
        "--repo",
        "r1",
        rs.to_str().unwrap(),
    ]);

    let (ok, out, _) = run(&["--db", &db, "search", "foo", "--language", "rust", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let r = v["results"].as_array().unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0]["org"], "o1");
    assert_eq!(r[0]["span"]["start"], 3);
    assert_eq!(r[1]["span"]["start_col"], 12);

    let (_, out, _) = run(&["--db", &db, "search", "foo", "--grain", "org", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 2);
    assert_eq!(v["results"][0]["count"], 2);

    let (_, out, _) = run(&["--db", &db, "search", "nothing"]);
    assert!(out.is_empty());
}

#[test]
fn errors() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (ok, _, err) = run(&[
        "--db",
        &db,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        "/no/such/file",
    ]);
    assert!(!ok && err.contains("/no/such/file"));
    let bin = d.path().join("x.bin");
    std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
    let (ok, _, err) = run(&[
        "--db",
        &db,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        bin.to_str().unwrap(),
    ]);
    assert!(!ok && err.contains("UTF-8"));
}

#[test]
fn json_purity_language_case_paths_and_bom() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let f = d.path().join("a.rs");
    std::fs::write(&f, "\u{feff}foo\r\nfoo // c\r\n").unwrap();
    let p = f.to_str().unwrap();
    assert!(
        run(&[
            "--db",
            &db,
            "index-file",
            "--org",
            "o",
            "--repo",
            "r",
            "--language",
            "Zig",
            p
        ])
        .0
    );
    let (ok, out, err) = run(&["--db", &db, "search", "foo", "--language", "ZIG", "--json"]);
    assert!(ok && err.is_empty());
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap(); // stdout is exactly one JSON doc
    assert_eq!(v["query"], "foo");
    let r = v["results"].as_array().unwrap();
    assert_eq!((r.len(), r[0]["span"]["start_col"].as_u64()), (2, Some(1)));
    assert_eq!(r[1]["span"]["start_line"], 2);
    // Same file via a different spelling is not a second file.
    let alt = format!("{}/./a.rs", d.path().display());
    let (_, out, _) = run(&["--db", &db, "index-file", "--org", "o", "--repo", "r", &alt]);
    assert!(out.contains("[replaced]"), "{out}");
    // --kind + --symbol-kind validation.
    let (ok, _, err) = run(&["--db", &db, "search", "foo", "--symbol-kind", "method"]);
    assert!(!ok && err.contains("--grain symbol"));
    // Missing database.
    let (ok, _, err) = run(&["--db", "/no/such.redb", "search", "foo"]);
    assert!(!ok && err.contains("does not exist"));
}

#[test]
fn non_utf8_stores_nothing_and_lock_is_reported() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g");
    let dbs = db.to_string_lossy().into_owned();
    let bad = d.path().join("bad.txt");
    std::fs::write(&bad, [b'f', 0xff]).unwrap();
    let ok_file = d.path().join("ok.txt");
    std::fs::write(&ok_file, "x").unwrap();
    run(&[
        "--db",
        &dbs,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        ok_file.to_str().unwrap(),
    ]);
    let (ok, _, err) = run(&[
        "--db",
        &dbs,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        bad.to_str().unwrap(),
    ]);
    assert!(!ok && err.contains("UTF-8"));
    let (_, out, _) = run(&["--db", &dbs, "search", "f", "--grain", "file"]);
    assert!(out.is_empty());
    // Held lock -> clear message.
    let _held = graph_store::RedbStore::open(&db).unwrap();
    let (ok, _, err) = run(&["--db", &dbs, "search", "x"]);
    assert!(!ok && err.contains("locked"), "{err}");
    // Directory as --db.
    let (ok, _, err) = run(&[
        "--db",
        d.path().to_str().unwrap(),
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        ok_file.to_str().unwrap(),
    ]);
    assert!(!ok && err.contains("directory"));
}

#[test]
fn rust_symbols_grains_end_to_end() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let rs = d.path().join("lib.rs");
    std::fs::write(&rs, "struct S;\nimpl S {\n    fn a(&self) { foo(); foo(); }\n    fn b(&self) { foo(); }\n}\nfn free() { foo(); }\n").unwrap();
    let zig = d.path().join("m.zig");
    std::fs::write(&zig, "pub fn main() void { foo(); }\n").unwrap();
    for (o, f) in [("o1", &rs), ("o2", &zig)] {
        let (ok, out, err) = run(&[
            "--db",
            &db,
            "index-file",
            "--org",
            o,
            "--repo",
            "r",
            f.to_str().unwrap(),
        ]);
        assert!(ok, "{out}{err}");
    }
    let rows = |args: &[&str]| -> Vec<serde_json::Value> {
        let mut a = vec!["--db", &db, "search", "foo", "--json"];
        a.extend_from_slice(args);
        let (_, out, _) = run(&a);
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert_eq!(rows(&["--grain", "token"]).len(), 5);
    let m = rows(&["--grain", "symbol", "--symbol-kind", "method"]);
    let got: Vec<_> = m
        .iter()
        .map(|r| {
            (
                r["symbol"].as_str().map(String::from),
                r["count"].as_u64().unwrap(),
            )
        })
        .collect();
    // Rows are ordered by file then offset; the roll-up row for free() (no
    // enclosing method) sits at offset 0 of its file.
    assert_eq!(got[0], (None, 1));
    assert_eq!(m[0]["no_matching_symbol"], true);
    assert_eq!(got[1], (Some("S::a".into()), 2));
    assert_eq!(got[2], (Some("S::b".into()), 1));
    assert_eq!(m[3]["no_symbols"], true);
    assert_eq!(rows(&["--grain", "file"]).len(), 2);
    assert_eq!(rows(&["--grain", "repo"]).len(), 2);
    assert_eq!(rows(&["--grain", "org"]).len(), 2);
    // Syntax error falls back and is reported.
    let bad = d.path().join("bad.rs");
    std::fs::write(&bad, "fn foo( {").unwrap();
    let (ok, out, _) = run(&[
        "--db",
        &db,
        "index-file",
        "--org",
        "o1",
        "--repo",
        "r",
        bad.to_str().unwrap(),
    ]);
    assert!(
        ok && out.contains("[has_errors]") && out.contains("symbols=0"),
        "{out}"
    );
}

#[test]
fn index_directory() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn foo() {}\n").unwrap();
    std::fs::write(root.join("main.zig"), "pub fn foo() void {}\n").unwrap();
    std::fs::write(root.join("target/gen.rs"), "fn foo() {}\n").unwrap();
    std::fs::write(root.join("a.log"), "foo\n").unwrap();
    std::fs::write(root.join(".git/config"), "foo\n").unwrap();
    std::fs::write(root.join("img.png"), [0x89, b'P', 0, 1, 2]).unwrap();
    std::fs::write(root.join("latin1.txt"), [b'f', 0xe9]).unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let r = root.to_str().unwrap();
    let (ok, out, err) = run(&[
        "--db", &db, "index", "--org", "o", "--repo", "p", "--json", r,
    ]);
    assert!(ok, "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    // .gitignore itself is text and indexed; ignored and .git paths are not.
    assert_eq!(v["languages"]["rust"], 1);
    assert_eq!(v["languages"]["zig"], 1);
    assert_eq!(
        v["skipped_by_reason"]["binary"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        v["skipped_by_reason"]["not valid UTF-8"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(v["elapsed_ms"].is_u64() && v["symbols"].as_u64().unwrap() >= 1);
    let (_, out, _) = run(&["--db", &db, "search", "foo", "--grain", "file", "--json"]);
    let s: serde_json::Value = serde_json::from_str(&out).unwrap();
    let files: Vec<_> = s["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["main.zig", "src/lib.rs"]);
    // Re-running is idempotent.
    run(&["--db", &db, "index", "--org", "o", "--repo", "p", r]);
    let (_, out, _) = run(&["--db", &db, "search", "foo", "--grain", "file", "--json"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (ok, _, err) = run(&["--db", &db, "index", "--org", "o", "--repo", "p", "/no/dir"]);
    assert!(!ok && err.contains("not a directory"));
}

#[cfg(unix)]
mod dir_edge_cases {
    use super::run;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn json(args: &[&str]) -> serde_json::Value {
        let (ok, out, err) = run(args);
        assert!(ok, "{out}{err}");
        serde_json::from_str(out.trim()).unwrap()
    }

    #[test]
    fn symlinks_unreadable_dirs_large_and_db_file_are_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        std::fs::create_dir_all(root.join("priv")).unwrap();
        std::fs::create_dir_all(root.join("ok")).unwrap();
        std::fs::write(root.join("ok/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("priv/p.rs"), "fn p() {}\n").unwrap();
        std::fs::write(root.join("big.txt"), vec![b'x'; 5000]).unwrap();
        std::fs::write(root.join("late.bin"), [vec![b'a'; 9000], vec![0]].concat()).unwrap();
        symlink(root.join("ok/a.rs"), root.join("link.rs")).unwrap();
        symlink(&root, root.join("loop")).unwrap();
        symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        std::fs::set_permissions(root.join("priv"), std::fs::Permissions::from_mode(0o0)).unwrap();
        let db = root.join("graph.db");
        let dbs = db.to_str().unwrap();
        let r = root.to_str().unwrap();
        run(&["--db", dbs, "index", "--org", "o", "--repo", "r", r]); // creates the db inside the dir
        let v = json(&[
            "--db",
            dbs,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--json",
            "--max-file-size",
            "1000",
            r,
        ]);
        std::fs::set_permissions(root.join("priv"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let s = &v["skipped_by_reason"];
        assert_eq!(s["symlink"].as_array().unwrap().len(), 3);
        assert_eq!(s["too large"].as_array().unwrap().len(), 2, "{s}"); // big.txt, late.bin (9 KB)
        assert_eq!(s["database file"].as_array().unwrap().len(), 1);
        assert!(!s["unreadable"].as_array().unwrap().is_empty());
        assert_eq!(v["languages"]["rust"], 1);
    }

    #[test]
    fn nul_after_8k_is_binary() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("late.bin"), [vec![b'a'; 9000], vec![0]].concat()).unwrap();
        let db = d.path().join("g");
        let v = json(&[
            "--db",
            db.to_str().unwrap(),
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--json",
            root.to_str().unwrap(),
        ]);
        assert_eq!(
            v["skipped_by_reason"]["binary"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn prune_deleted_files_and_nested_gitignore_negation() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n!keep.log\n").unwrap();
        std::fs::write(root.join("sub/.gitignore"), "hidden.txt\n").unwrap();
        for f in [
            "a.rs",
            "b.rs",
            "x.log",
            "keep.log",
            "sub/hidden.txt",
            "sub/shown.txt",
        ] {
            std::fs::write(root.join(f), "foo\n").unwrap();
        }
        let db = d.path().join("g");
        let (dbs, r) = (db.to_str().unwrap(), root.to_str().unwrap());
        let v = json(&[
            "--db", dbs, "index", "--org", "o", "--repo", "r", "--json", r,
        ]);
        assert_eq!(v["files"], 6); // a.rs b.rs keep.log shown.txt + two .gitignore files
        std::fs::remove_file(root.join("b.rs")).unwrap();
        // Without --prune the deleted file stays.
        json(&[
            "--db", dbs, "index", "--org", "o", "--repo", "r", "--json", r,
        ]);
        let (_, out, _) = run(&["--db", dbs, "search", "foo", "--grain", "file", "--json"]);
        assert!(out.contains("b.rs"));
        let v = json(&[
            "--db", dbs, "index", "--org", "o", "--repo", "r", "--json", "--prune", r,
        ]);
        assert_eq!(v["pruned"], serde_json::json!(["b.rs"]));
        let (_, out, _) = run(&["--db", dbs, "search", "foo", "--grain", "file", "--json"]);
        assert!(
            !out.contains("b.rs")
                && out.contains("a.rs")
                && out.contains("keep.log")
                && !out.contains("x.log")
        );
        assert!(!out.contains("hidden.txt") && out.contains("shown.txt"));
    }

    #[test]
    fn empty_org_rejected_and_text_summary_lists_skips() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("x.bin"), [0u8, 1]).unwrap();
        let db = d.path().join("g");
        let dbs = db.to_str().unwrap();
        let (ok, _, err) = run(&[
            "--db",
            dbs,
            "index",
            "--org",
            "",
            "--repo",
            "r",
            root.to_str().unwrap(),
        ]);
        assert!(!ok && err.contains("must not be empty"));
        let (ok, out, _) = run(&[
            "--db",
            dbs,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            root.to_str().unwrap(),
        ]);
        assert!(
            ok && out.contains("skipped (binary): 1") && out.contains("x.bin"),
            "{out}"
        );
    }
}

mod prune_and_limits {
    use super::run;

    fn idx(db: &str, repo: &str, extra: &[&str], dir: &std::path::Path) -> (bool, String, String) {
        let mut a = vec!["--db", db, "index", "--org", "o", "--repo", repo];
        a.extend_from_slice(extra);
        a.push(dir.to_str().unwrap());
        run(&a)
    }

    fn files_of(db: &str, repo: &str) -> Vec<String> {
        let (_, out, _) = run(&[
            "--db", db, "search", "foo", "--grain", "file", "--repo", repo, "--json",
        ]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["file"].as_str().unwrap().to_string())
            .collect()
    }

    fn setup(names: &[&str]) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        for n in names {
            std::fs::write(root.join(n), "foo\n").unwrap();
        }
        let db = d.path().join("g").to_string_lossy().into_owned();
        (d, root, db)
    }

    #[test]
    fn prune_refused_on_empty_walk_unless_forced() {
        let (_d, root, db) = setup(&["a.txt", "b.txt"]);
        assert!(idx(&db, "r", &[], &root).0);
        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        let (ok, _, err) = idx(&db, "r", &["--prune"], &root);
        assert!(
            !ok && err.contains("--prune refused") && err.contains("--force"),
            "{err}"
        );
        assert_eq!(files_of(&db, "r"), ["a.txt", "b.txt"]);
        let (ok, out, err) = idx(&db, "r", &["--prune", "--force", "--json"], &root);
        assert!(ok, "{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["pruned"], serde_json::json!(["a.txt", "b.txt"]));
        assert!(files_of(&db, "r").is_empty());
        // Nothing to lose: an empty walk is fine without --force.
        assert!(idx(&db, "r", &["--prune"], &root).0);
        // --force only makes sense with --prune.
        let (ok, _, err) = idx(&db, "r", &["--force"], &root);
        assert!(!ok && err.contains("--prune"), "{err}");
        // --reindex is not --force: it never bypasses the empty-run guard.
        assert!(idx(&db, "r", &[], &root).0);
    }

    #[test]
    fn reindex_prune_on_empty_dir_is_still_refused() {
        let (_d, root, db) = setup(&["a.txt"]);
        assert!(idx(&db, "r", &[], &root).0);
        std::fs::remove_file(root.join("a.txt")).unwrap();
        let (ok, _, err) = idx(&db, "r", &["--reindex", "--prune"], &root);
        assert!(!ok && err.contains("--prune refused"), "{err}");
        assert_eq!(files_of(&db, "r"), ["a.txt"]);
    }

    #[test]
    fn prune_is_scoped_per_repo_and_spares_index_file_files() {
        let (d, root, db) = setup(&["a.txt", "b.txt"]);
        assert!(idx(&db, "r1", &[], &root).0);
        assert!(idx(&db, "r2", &[], &root).0);
        let single = d.path().join("s.txt");
        std::fs::write(&single, "foo\n").unwrap();
        let (ok, o, e) = run(&[
            "--db",
            &db,
            "index-file",
            "--org",
            "o",
            "--repo",
            "r1",
            "--language",
            "text",
            single.to_str().unwrap(),
        ]);
        assert!(ok, "{o}{e}");
        // b.txt re-added via index-file (same stored path) loses its directory mark.
        let b = root.join("b.txt");
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_memory-graph"))
            .current_dir(&root)
            .args([
                "--db",
                &db,
                "index-file",
                "--org",
                "o",
                "--repo",
                "r1",
                "b.txt",
            ])
            .output()
            .unwrap();
        assert!(o.status.success());
        std::fs::remove_file(&b).unwrap();
        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::fs::write(root.join("c.txt"), "foo\n").unwrap();
        let (ok, out, err) = idx(&db, "r1", &["--prune", "--json"], &root);
        assert!(ok, "{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["pruned"], serde_json::json!(["a.txt"]));
        let f = files_of(&db, "r1");
        assert!(f.contains(&"c.txt".to_string()) && !f.contains(&"a.txt".to_string()));
        assert_eq!(f.len(), 3, "{f:?}"); // b.txt (unmarked), c.txt, absolute s.txt
        assert_eq!(files_of(&db, "r2"), ["a.txt", "b.txt"]);
    }

    #[test]
    fn prune_after_file_becomes_binary_or_ignored_and_text_output_lists_paths() {
        let (_d, root, db) = setup(&["a.txt", "b.txt", "c.txt"]);
        assert!(idx(&db, "r", &[], &root).0);
        std::fs::write(root.join("a.txt"), [b'f', 0, 1]).unwrap();
        std::fs::write(root.join(".gitignore"), "b.txt\n").unwrap(); // no .git dir needed
        let (ok, out, err) = idx(&db, "r", &["--prune"], &root);
        assert!(ok, "{err}");
        assert!(
            out.contains("pruned=2") && out.contains("    a.txt") && out.contains("    b.txt"),
            "{out}"
        );
        assert_eq!(files_of(&db, "r"), ["c.txt"]);
    }

    #[test]
    fn prune_text_output_caps_at_twenty() {
        let (_d, root, db) = setup(&["keep.txt"]);
        for i in 0..25 {
            std::fs::write(root.join(format!("f{i:02}.txt")), "foo\n").unwrap();
        }
        assert!(idx(&db, "r", &[], &root).0);
        for i in 0..25 {
            std::fs::remove_file(root.join(format!("f{i:02}.txt"))).unwrap();
        }
        let (ok, out, _) = idx(&db, "r", &["--prune"], &root);
        assert!(ok && out.contains("... and 5 more"), "{out}");
    }

    #[test]
    fn max_file_size_boundary_and_zero_rejected() {
        let (_d, root, db) = setup(&[]);
        std::fs::write(root.join("exact.txt"), "12345").unwrap();
        std::fs::write(root.join("over.txt"), "123456").unwrap();
        let (ok, out, err) = idx(&db, "r", &["--max-file-size", "5", "--json"], &root);
        assert!(ok, "{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["files"], 1);
        assert_eq!(
            v["skipped_by_reason"]["too large"],
            serde_json::json!(["over.txt"])
        );
        let (ok, _, err) = idx(&db, "r", &["--max-file-size", "0"], &root);
        assert!(!ok && err.contains("--max-file-size"), "{err}");
    }

    #[test]
    fn json_keys_and_clean_stderr() {
        let (_d, root, db) = setup(&["a.txt"]);
        let (ok, out, err) = idx(&db, "r", &["--json", "--prune"], &root);
        assert!(ok && err.is_empty(), "{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(v["pruned"].as_array().unwrap().is_empty());
        assert!(v["skipped_by_reason"].is_object());
    }

    #[test]
    fn db_in_missing_directory_names_the_path() {
        let (d, root, _) = setup(&["a.txt"]);
        let db = d.path().join("missing").join("g");
        let (ok, _, err) = idx(db.to_str().unwrap(), "r", &[], &root);
        assert!(!ok && err.contains("missing"), "{err}");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{symlink, PermissionsExt};

        #[test]
        fn prune_skipped_and_data_kept_when_subdir_unreadable() {
            let (_d, root, db) = setup(&["a.txt", "gone.txt"]);
            std::fs::create_dir(root.join("priv")).unwrap();
            std::fs::write(root.join("priv/p.txt"), "foo\n").unwrap();
            assert!(idx(&db, "r", &[], &root).0);
            std::fs::remove_file(root.join("gone.txt")).unwrap();
            std::fs::set_permissions(root.join("priv"), std::fs::Permissions::from_mode(0o0))
                .unwrap();
            let unreadable = std::fs::read_dir(root.join("priv")).is_err();
            let (ok, out, err) = idx(&db, "r", &["--prune", "--json"], &root);
            std::fs::set_permissions(root.join("priv"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            if !unreadable {
                return; // running as root: permissions are not enforced
            }
            assert!(ok, "{err}");
            assert!(err.contains("--prune skipped"), "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert!(v["pruned"].as_array().unwrap().is_empty());
            assert_eq!(files_of(&db, "r"), ["a.txt", "gone.txt", "priv/p.txt"]);
        }

        #[test]
        fn non_utf8_filename_is_skipped() {
            let (_d, root, db) = setup(&["ok.txt"]);
            let bad = root.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
            if std::fs::write(&bad, "foo\n").is_err() {
                return; // filesystem refuses such names
            }
            let (ok, out, err) = idx(&db, "r", &["--json"], &root);
            assert!(ok, "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(v["files"], 1);
            assert_eq!(
                v["skipped_by_reason"]["non-UTF-8 path"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }

        #[test]
        fn symlinked_root_is_indexed_without_bogus_skip() {
            let (d, root, db) = setup(&["a.txt"]);
            let link = d.path().join("link");
            symlink(&root, &link).unwrap();
            let (ok, out, err) = idx(&db, "r", &["--json"], &link);
            assert!(ok, "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(v["files"], 1);
            assert!(
                v["skipped_by_reason"].as_object().unwrap().is_empty(),
                "{v}"
            );
        }

        #[test]
        fn fifo_is_skipped_as_not_a_regular_file() {
            let (_d, root, db) = setup(&["a.txt"]);
            let made = std::process::Command::new("mkfifo")
                .arg(root.join("pipe"))
                .status()
                .is_ok_and(|s| s.success());
            if !made {
                return;
            }
            let (ok, out, err) = idx(&db, "r", &["--json"], &root);
            assert!(ok, "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(
                v["skipped_by_reason"]["not a regular file"],
                serde_json::json!(["pipe"])
            );
            assert_eq!(v["files"], 1);
        }

        #[test]
        fn db_file_inside_dir_is_skipped_via_hard_link_too() {
            let (_d, root, _) = setup(&["a.txt"]);
            let db = root.join("g.redb");
            let dbs = db.to_str().unwrap();
            assert!(idx(dbs, "r", &[], &root).0);
            std::fs::hard_link(&db, root.join("alias.redb")).unwrap();
            let (ok, out, err) = idx(dbs, "r", &["--json"], &root);
            assert!(ok, "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(
                v["skipped_by_reason"]["database file"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}

#[test]
fn symbols_command() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("p");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "fn parse() {}\nstruct S;\nimpl S {\n    fn parse(&self) {}\n    fn parser(&self) {}\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("other.py"), "def parse(): pass\n").unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        root.to_str().unwrap(),
    ]);
    assert!(ok, "{out}{err}");
    let q = |args: &[&str]| -> Vec<serde_json::Value> {
        let mut a = vec!["--db", &db, "symbols", "--json"];
        a.extend_from_slice(args);
        let (ok, out, err) = run(&a);
        assert!(ok && err.is_empty(), "{err}");
        serde_json::from_str::<serde_json::Value>(out.trim()).unwrap()["results"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert_eq!(q(&["parse"]).len(), 2); // Python's `def parse` has no extractor, so only Rust symbols
    let m = q(&["parse", "--kind", "method"]);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["qualified"], "S::parse");
    assert_eq!(m[0]["lang_kind"], "fn");
    assert_eq!(m[0]["file"], "src/lib.rs");
    assert_eq!(q(&["pars*"]).len(), 3);
    assert_eq!(q(&["pars*", "--language", "python"]).len(), 0);
    assert_eq!(
        q(&["pars*", "--file", "src/lib.rs", "--repo", "r"]).len(),
        3
    );
    let (ok, out, _) = run(&["--db", &db, "symbols", "parser"]);
    assert!(
        ok && out.contains("src/lib.rs:5:")
            && out.contains("method (fn)")
            && out.contains("S::parser"),
        "{out}"
    );
    let (ok, _, err) = run(&["--db", "/no/such.redb", "symbols", "x"]);
    assert!(!ok && err.contains("does not exist"));
}

#[test]
fn polyglot_repo_is_described_and_filters_are_validated() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("mono");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("lib.rs"), "struct Widget;\ntrait Draw { fn draw(&self); }\nenum E { A }\nimpl Widget { fn new() -> Self { Widget } }\n").unwrap();
    std::fs::write(
        root.join("bin/tool"),
        "#!/usr/bin/env python3\nprint('hi')\n",
    )
    .unwrap();
    std::fs::write(root.join("build.zig"), "pub fn main() void {}\n").unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "mono",
        root.to_str().unwrap(),
    ]);
    assert!(ok, "{out}{err}");
    // Languages were detected (shebang, filename, extension); nothing was dictated.
    let (ok, out, _) = run(&["--db", &db, "describe", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let langs = &v["repos"][0]["languages"];
    for l in ["rust", "python", "zig", "make"] {
        assert!(langs[l]["files"].as_u64().unwrap() >= 1, "{l}: {langs}");
    }
    let kinds = langs["rust"]["symbol_kinds"].as_object().unwrap();
    for k in [
        "type/struct",
        "type/trait",
        "type/enum",
        "other/impl",
        "method/fn",
    ] {
        assert!(kinds.contains_key(k), "{k} in {kinds:?}");
    }
    assert!(langs["zig"]["symbols"] == 0);
    // --kind accepts generic and language-specific names.
    let count = |args: &[&str]| -> usize {
        let mut a = vec!["--db", &db, "symbols", "--json"];
        a.extend_from_slice(args);
        let (ok, out, err) = run(&a);
        assert!(ok, "{err}");
        serde_json::from_str::<serde_json::Value>(out.trim()).unwrap()["results"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(count(&["*", "--kind", "trait"]), 1);
    assert_eq!(count(&["*", "--kind", "type"]), 3); // struct, trait, enum
    assert_eq!(count(&["W*", "--kind", "struct", "--language", "rust"]), 1);
    // Unknown values fail loudly and list what exists.
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--kind", "class"]);
    assert!(
        !ok && err.contains("kinds present") && err.contains("struct"),
        "{err}"
    );
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--language", "cobol"]);
    assert!(
        !ok && err.contains("languages present") && err.contains("python"),
        "{err}"
    );
    let (ok, _, err) = run(&["--db", &db, "search", "x", "--language", "cobol"]);
    assert!(!ok && err.contains("languages present"));
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--repo", "nope"]);
    assert!(!ok && err.contains("describe"));
    // A kind that exists, but not for the chosen language, is rejected too.
    let (ok, _, _) = run(&[
        "--db",
        &db,
        "symbols",
        "*",
        "--kind",
        "struct",
        "--language",
        "zig",
    ]);
    assert!(!ok);
    let (ok, out, _) = run(&["--db", &db, "describe"]);
    assert!(
        ok && out.contains("o/mono") && out.contains("python") && out.contains("type/struct"),
        "{out}"
    );
}

fn indexed_db(d: &tempfile::TempDir) -> String {
    let db = d.path().join("g").to_string_lossy().into_owned();
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for i in 0..30 {
        std::fs::write(src.join(format!("f{i:02}.rs")), "fn foo() { foo(); }\n").unwrap();
    }
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        src.to_str().unwrap(),
    ]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("files=30"), "{out}");
    db
}

#[test]
fn limit_flags_and_pattern_errors() {
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(&d);
    let (ok, out, _) = run(&["--db", &db, "symbols", "foo", "--limit", "3", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let r = v["results"].as_array().unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0]["file"], "f00.rs");
    let (ok, out, _) = run(&["--db", &db, "search", "foo", "--limit", "5", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 5);
    let (ok, _, err) = run(&["--db", &db, "symbols", "foo", "--limit", "0"]);
    assert!(!ok && err.contains("limit"), "{err}");
    let (ok, _, err) = run(&["--db", &db, "symbols", "**"]);
    assert!(!ok && err.contains("ambiguous"), "{err}");
    let (ok, _, err) = run(&["--db", &db, "symbols", ""]);
    assert!(!ok && err.contains("empty"), "{err}");
    let (ok, _, err) = run(&["--db", &db, "symbols", "foo", "--org", ""]);
    assert!(!ok && err.contains("empty"), "{err}");
    // `other` is a valid generic kind even when nothing of that kind exists.
    let (ok, _, err) = run(&["--db", &db, "symbols", "foo", "--kind", "other"]);
    assert!(ok, "{err}");
}

#[cfg(unix)]
#[test]
fn broken_pipe_is_not_a_panic() {
    use std::process::Stdio;
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(&d);
    let mut c = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args(["--db", &db, "symbols", "*"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(c.stdout.take()); // close the read end immediately
    let o = c.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert!(o.status.success(), "{err}");
}

fn write_rust_repo(d: &tempfile::TempDir) -> std::path::PathBuf {
    let src = d.path().join("rs");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(
        src.join("a.rs"),
        "struct S;\nimpl S {\n    fn m(&self) {}\n}\nfn foo() {}\nfn foobar() {}\n",
    )
    .unwrap();
    std::fs::write(src.join("b.rs"), "fn foo() { foo(); }\n").unwrap();
    src
}

fn symbols_json(db: &str, extra: &[&str]) -> Vec<serde_json::Value> {
    let mut a = vec!["--db", db, "symbols"];
    a.extend_from_slice(extra);
    a.push("--json");
    let (ok, out, err) = run(&a);
    assert!(ok, "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    v["results"].as_array().unwrap().clone()
}

#[test]
fn old_database_without_symbol_index_version_is_upgraded_via_cli() {
    use redb::{Database, MultimapTableDefinition, TableDefinition};
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (ok, o, e) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        src.to_str().unwrap(),
    ]);
    assert!(ok, "{o}{e}");
    {
        // Simulate a database written before the symbol index existed.
        let raw = Database::open(&db).unwrap();
        let wt = raw.begin_write().unwrap();
        wt.open_table(TableDefinition::<&str, u64>::new("meta"))
            .unwrap()
            .remove("symbol_index_version")
            .unwrap();
        wt.delete_multimap_table(MultimapTableDefinition::<&str, u64>::new("symbols_by_name"))
            .unwrap();
        wt.commit().unwrap();
    }
    let r = symbols_json(&db, &["foo"]);
    assert_eq!(r.len(), 2, "{r:?}");
    // The stamp is written back, so the next open is a no-op.
    let raw = Database::open(&db).unwrap();
    let rt = raw.begin_read().unwrap();
    let v = rt
        .open_table(TableDefinition::<&str, u64>::new("meta"))
        .unwrap()
        .get("symbol_index_version")
        .unwrap()
        .map(|v| v.value());
    assert_eq!(v, Some(graph_store::SYMBOL_INDEX_VERSION));
}

#[test]
fn symbols_trailing_star_and_escaped_star_via_cli() {
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    let db = d.path().join("g").to_string_lossy().into_owned();
    assert!(
        run(&[
            "--db",
            &db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            src.to_str().unwrap()
        ])
        .0
    );
    let names = |p: &str| -> Vec<String> {
        symbols_json(&db, &[p])
            .iter()
            .map(|x| x["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names("foo*"), ["foo", "foobar", "foo"]);
    assert_eq!(names("foo"), ["foo", "foo"]);
    // `foo\*` is the literal name `foo*`: nothing is indexed under it.
    assert!(names("foo\\*").is_empty());
    // Documented edges.
    let (ok, _, err) = run(&["--db", &db, "symbols", "foo\\**"]);
    assert!(!ok && err.contains("ambiguous"), "{err}");
    assert!(names("foo\\").is_empty());
}

#[test]
fn kind_other_and_case_insensitive_kinds_match_real_symbols() {
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    let db = d.path().join("g").to_string_lossy().into_owned();
    assert!(
        run(&[
            "--db",
            &db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            src.to_str().unwrap()
        ])
        .0
    );
    // The `impl S` block is generic kind `other`, language kind `impl`.
    for k in ["other", "Other", "OTHER", "impl", "IMPL"] {
        let r = symbols_json(&db, &["*", "--kind", k]);
        assert_eq!(r.len(), 1, "{k}: {r:?}");
        assert_eq!(r[0]["lang_kind"], "impl");
    }
    let r = symbols_json(&db, &["*", "--kind", "Function", "--language", "RUST"]);
    assert_eq!(r.len(), 3, "{r:?}");
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--kind", "nosuchkind"]);
    assert!(!ok && err.contains("nosuchkind"), "{err}");
    // Search's --symbol-kind is case-insensitive too.
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "search",
        "S",
        "--grain",
        "symbol",
        "--symbol-kind",
        "IMPL",
        "--json",
    ]);
    assert!(ok, "{out}{err}");
}

#[test]
fn help_distinguishes_token_class_from_symbol_kind_and_documents_pattern_edges() {
    let (ok, out, _) = run(&["search", "--help"]);
    assert!(ok);
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("Token class (NOT a symbol kind"), "{out}");
    assert!(flat.contains("symbol kind"), "{out}");
    let (ok, out, _) = run(&["symbols", "--help"]);
    assert!(ok);
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("token class"), "{out}");
    assert!(flat.contains("case-insensitive"), "{out}");
    assert!(flat.contains("rejected as ambiguous"), "{out}");
    assert!(flat.contains("trailing backslash"), "{out}");
}

#[test]
fn directory_batch_ingest_matches_individual_index_file() {
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    std::fs::write(src.join("c.txt"), "hello foo world\n").unwrap();
    let db_a = d.path().join("a").to_string_lossy().into_owned();
    let db_b = d.path().join("b").to_string_lossy().into_owned();
    assert!(
        run(&[
            "--db",
            &db_a,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            src.to_str().unwrap()
        ])
        .0
    );
    for f in ["a.rs", "b.rs", "c.txt"] {
        let p = src.join(f);
        // index-file stores the path as given; run from the directory for relative paths.
        let o = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
            .current_dir(&src)
            .args(["--db", &db_b, "index-file", "--org", "o", "--repo", "r", f])
            .output()
            .unwrap();
        assert!(o.status.success(), "{p:?}");
    }
    let dump = |db: &str| {
        let syms = symbols_json(db, &["*"]);
        let (_, toks, _) = run(&["--db", db, "search", "foo", "--json"]);
        let (_, desc, _) = run(&["--db", db, "describe", "--json"]);
        (syms, toks, desc)
    };
    assert_eq!(dump(&db_a), dump(&db_b));
}

#[cfg(unix)]
#[test]
fn closed_stdout_is_not_a_panic_for_index_and_index_file() {
    use std::process::Stdio;
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    let db = d.path().join("g").to_string_lossy().into_owned();
    let file = src.join("a.rs");
    let cases: [Vec<&str>; 3] = [
        vec![
            "--db",
            &db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            src.to_str().unwrap(),
        ],
        vec![
            "--db",
            &db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--json",
            src.to_str().unwrap(),
        ],
        vec![
            "--db",
            &db,
            "index-file",
            "--org",
            "o",
            "--repo",
            "r",
            file.to_str().unwrap(),
        ],
    ];
    for args in cases {
        let mut c = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(c.stdout.take());
        let o = c.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert!(o.status.success(), "{args:?}: {err}");
    }
}

mod unchanged_files {
    use super::run;

    fn idx(db: &str, extra: &[&str], dir: &std::path::Path) -> serde_json::Value {
        let mut a = vec!["--db", db, "index", "--org", "o", "--repo", "r", "--json"];
        a.extend_from_slice(extra);
        a.push(dir.to_str().unwrap());
        let (ok, out, err) = run(&a);
        assert!(ok, "{out}{err}");
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn index_reports_unchanged_and_reindexes_only_what_changed() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g");
        let db = db.to_str().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir(&src).unwrap();
        for i in 0..5 {
            std::fs::write(src.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
        }
        let v = idx(db, &[], &src);
        assert_eq!(
            (v["files"].as_u64(), v["unchanged"].as_u64()),
            (Some(5), Some(0))
        );
        let v = idx(db, &["--prune"], &src);
        assert_eq!(
            (v["files"].as_u64(), v["unchanged"].as_u64()),
            (Some(5), Some(5))
        );
        assert_eq!(v["pruned"], serde_json::json!([]));
        // Text summary carries the count too.
        let (ok, out, _) = run(&[
            "--db",
            db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            src.to_str().unwrap(),
        ]);
        assert!(ok && out.contains("files=5 unchanged=5"), "{out}");
        // Edit one file, delete another: one re-indexed, others skipped, prune still works.
        std::fs::write(src.join("f0.rs"), "fn changed() {}\n").unwrap();
        std::fs::remove_file(src.join("f4.rs")).unwrap();
        let v = idx(db, &["--prune"], &src);
        assert_eq!(
            (v["files"].as_u64(), v["unchanged"].as_u64()),
            (Some(4), Some(3))
        );
        assert_eq!(v["pruned"], serde_json::json!(["f4.rs"]));
        let (ok, out, _) = run(&["--db", db, "symbols", "changed", "--json"]);
        assert!(ok && out.contains("changed"), "{out}");
        let (_, out, _) = run(&["--db", db, "symbols", "f0", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["results"],
            serde_json::json!([]),
            "old symbol gone: {out}"
        );
        // --force --prune alone does not re-index.
        let v = idx(db, &["--prune", "--force"], &src);
        assert_eq!(
            (v["files"].as_u64(), v["unchanged"].as_u64()),
            (Some(4), Some(4))
        );
        // --reindex re-indexes everything.
        let v = idx(db, &["--reindex"], &src);
        assert_eq!(
            (v["files"].as_u64(), v["unchanged"].as_u64()),
            (Some(4), Some(0))
        );
    }

    #[test]
    fn index_file_reports_unchanged_and_reindex_reindexes() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g");
        let db = db.to_str().unwrap();
        let f = d.path().join("a.rs");
        std::fs::write(&f, "fn a() {}\n").unwrap();
        let go = |extra: &[&str]| {
            let mut a = vec!["--db", db, "index-file", "--org", "o", "--repo", "r"];
            a.extend_from_slice(extra);
            a.push(f.to_str().unwrap());
            let (ok, out, err) = run(&a);
            assert!(ok, "{out}{err}");
            out
        };
        let out = go(&[]);
        assert!(!out.contains("[unchanged]"), "{out}");
        let out = go(&[]);
        assert!(
            out.contains("[unchanged]") && out.contains("symbols=0"),
            "{out}"
        );
        let out = go(&["--reindex"]);
        assert!(
            out.contains("[replaced]") && !out.contains("[unchanged]"),
            "{out}"
        );
        assert!(out.contains("symbols=1"), "{out}");
    }

    #[test]
    fn index_and_index_file_share_extractors_so_files_stay_unchanged() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g");
        let db = db.to_str().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}\n").unwrap();
        // index-file stores the path as given (known issue), so run it from inside `src`.
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_memory-graph"))
            .current_dir(&src)
            .args([
                "--db",
                db,
                "index-file",
                "--org",
                "o",
                "--repo",
                "r",
                "a.rs",
            ])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success() && out.contains("symbols=1"), "{out}");
        // Both paths register the Rust extractor: the directory run sees an identical fingerprint.
        let v = idx(db, &[], &src);
        assert_eq!(v["unchanged"].as_u64(), Some(1), "{v}");
        let (_, out, _) = run(&["--db", db, "symbols", "a", "--json"]);
        assert!(out.contains("\"a\""), "{out}");
    }
}

#[test]
fn filter_validation_follows_the_catalog_across_prune_and_reindex() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("lib.rs"), "struct Widget;\nfn new() {}\n").unwrap();
    std::fs::write(root.join("tool.py"), "def go():\n    pass\n").unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let r = root.to_str().unwrap();
    let index = |extra: &[&str]| {
        let mut a = vec!["--db", &db, "index", "--org", "o", "--repo", "r"];
        a.extend_from_slice(extra);
        a.push(r);
        let (ok, out, err) = run(&a);
        assert!(ok, "{out}{err}");
    };
    index(&[]);
    // No filters: nothing to validate, queries just run.
    let (ok, out, _) = run(&["--db", &db, "symbols", "new"]);
    assert!(ok && out.contains("new"), "{out}");
    let (ok, _, _) = run(&["--db", &db, "search", "Widget"]);
    assert!(ok);
    // With filters: values come from what is indexed.
    let (ok, _, _) = run(&["--db", &db, "symbols", "*", "--language", "python"]);
    assert!(ok);
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--kind", "bogus"]);
    assert!(!ok && err.contains("kind"), "{err}");
    // Removing the python file (prune) removes the language from validation.
    std::fs::remove_file(root.join("tool.py")).unwrap();
    index(&["--prune"]);
    let (ok, _, err) = run(&["--db", &db, "symbols", "*", "--language", "python"]);
    assert!(!ok && err.contains("languages present"), "{err}");
    // Re-index (forced) keeps the counts stable.
    let (_, before, _) = run(&["--db", &db, "describe", "--json"]);
    index(&["--reindex"]);
    let (_, after, _) = run(&["--db", &db, "describe", "--json"]);
    assert_eq!(before, after);
}

#[test]
fn v1_database_without_catalog_is_upgraded_via_cli() {
    use redb::{Database, TableDefinition};
    let d = tempfile::tempdir().unwrap();
    let src = write_rust_repo(&d);
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (ok, o, e) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        src.to_str().unwrap(),
    ]);
    assert!(ok, "{o}{e}");
    let (_, expected, _) = run(&["--db", &db, "describe", "--json"]);
    let meta = TableDefinition::<&str, u64>::new("meta");
    {
        // Simulate a database written by the previous release.
        let raw = Database::open(&db).unwrap();
        let wt = raw.begin_write().unwrap();
        {
            let mut m = wt.open_table(meta).unwrap();
            m.insert("schema_version", 1).unwrap();
            m.remove("catalog_version").unwrap();
        }
        wt.delete_table(TableDefinition::<&str, u64>::new("catalog"))
            .unwrap();
        wt.commit().unwrap();
    }
    let (ok, out, err) = run(&["--db", &db, "describe", "--json"]);
    assert!(ok, "{err}");
    assert_eq!(out, expected);
    let raw = Database::open(&db).unwrap();
    let v = raw
        .begin_read()
        .unwrap()
        .open_table(meta)
        .unwrap()
        .get("schema_version")
        .unwrap()
        .map(|v| v.value());
    assert_eq!(v, Some(graph_store::SCHEMA_VERSION));
}
