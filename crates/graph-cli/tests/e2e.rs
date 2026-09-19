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
    let _held = graph_store::Store::open(&db).unwrap();
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
