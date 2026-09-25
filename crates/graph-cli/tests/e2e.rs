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

/// Issue #69: a small ASP.NET Web Forms site indexes with symbols for every
/// shipped language.
#[test]
fn aspnet_site_symbols() {
    {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("site");
        std::fs::create_dir_all(root.join("Scripts")).unwrap();
        let files = [
            (
                "Default.aspx",
                "<%@ Page Language=\"C#\" CodeBehind=\"Default.aspx.cs\" %>\n<form id=\"f\" runat=\"server\"><asp:Button ID=\"btnGo\" runat=\"server\" /></form>\n",
            ),
            (
                "Default.aspx.cs",
                "namespace Site { public partial class DefaultPage : Page { protected void Go_Click(object s, EventArgs e) { } } }\n",
            ),
            ("Scripts/app.js", "function initApp() {}\nconst go = () => 1;\n"),
            ("about.html", "<div id=\"about\"></div>\n"),
        ];
        for (path, src) in files {
            std::fs::write(root.join(path), src).unwrap();
        }
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
        let (ok, out, err) = run(&["--db", &db, "describe", "--json"]);
        assert!(ok, "{err}");
        for lang in ["aspx", "csharp", "javascript", "html"] {
            assert!(
                out.contains(&format!("\"{lang}\"")),
                "{lang} missing: {out}"
            );
        }
        for (name, lang_kind) in [
            ("btnGo", "control"),
            ("DefaultPage", "class"),
            ("Go_Click", "method"),
            ("initApp", "function"),
            ("go", "arrow_fn"),
            ("about", "element"),
        ] {
            let (ok, out, err) = run(&["--db", &db, "symbols", "--json", name]);
            assert!(ok, "{err}");
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            let hits = v["results"].as_array().unwrap();
            assert!(
                hits.iter().any(|h| h["lang_kind"] == lang_kind),
                "{name}/{lang_kind} not in {out}"
            );
        }
    }
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
    let _held = graph_store::V2Store::open(&db).unwrap();
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

/// `--chunk-bytes` forces `index_batch` to commit many small chunks (one
/// file per chunk here); the indexed result must be the same as an
/// unchunked run. The old spelling `--v2-chunk-bytes` is a hidden alias.
#[test]
fn chunk_bytes_flag() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..5 {
        std::fs::write(root.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    let r = root.to_str().unwrap();

    let db_chunked = d.path().join("chunked").to_string_lossy().into_owned();
    let (ok, out, err) = run(&[
        "--db",
        &db_chunked,
        "--chunk-bytes",
        "1",
        "index",
        "--org",
        "o",
        "--repo",
        "p",
        r,
    ]);
    assert!(ok, "{out}{err}");

    let db_unchunked = d.path().join("unchunked").to_string_lossy().into_owned();
    let (ok, out, err) = run(&[
        "--db",
        &db_unchunked,
        "index",
        "--org",
        "o",
        "--repo",
        "p",
        r,
    ]);
    assert!(ok, "{out}{err}");

    for db in [&db_chunked, &db_unchunked] {
        let (ok, out, _) = run(&["--db", db, "search", "f3", "--json"]);
        assert!(ok);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 1, "db={db}");
    }

    // The old flag name still works, as a hidden alias.
    let db_alias = d.path().join("alias").to_string_lossy().into_owned();
    let (ok, out, err) = run(&[
        "--db",
        &db_alias,
        "--v2-chunk-bytes",
        "1",
        "index",
        "--org",
        "o",
        "--repo",
        "p",
        r,
    ]);
    assert!(ok, "{out}{err}");
    assert_eq!(
        std::fs::read(&db_alias).unwrap(),
        std::fs::read(&db_chunked).unwrap(),
        "alias and new name index identically"
    );
    let (ok, out, _) = run(&["--help"]);
    assert!(
        ok && out.contains("--chunk-bytes") && !out.contains("--v2-chunk-bytes"),
        "{out}"
    );
}

/// `--cache-bytes` sets redb's cache size; it must not change results
/// (indexing, describe, search, vacuum) and must work at any open (not just
/// indexing). The old spelling `--v2-cache-bytes` is a hidden alias.
#[test]
fn cache_bytes_flag() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..3 {
        std::fs::write(root.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    let r = root.to_str().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();

    // A small cache is still correct at index time.
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "--cache-bytes",
        "65536",
        "index",
        "--org",
        "o",
        "--repo",
        "p",
        r,
    ]);
    assert!(ok, "{out}{err}");

    // And at every later open, including a different cache size than the one used to index.
    for cache_bytes in ["1048576", "65536"] {
        let (ok, out, _) = run(&[
            "--db",
            &db,
            "--cache-bytes",
            cache_bytes,
            "search",
            "f1",
            "--json",
        ]);
        assert!(ok, "{out}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 1);

        let (ok, out, err) = run(&["--db", &db, "--cache-bytes", cache_bytes, "vacuum"]);
        assert!(ok, "{out}{err}");
    }

    // The old flag name still works, as a hidden alias.
    let (ok, out, err) = run(&["--db", &db, "--v2-cache-bytes", "65536", "describe"]);
    assert!(ok && out.contains("o/p"), "{out}{err}");
}

/// `vacuum --compact` (ADR 0003 story 3, slice 3f): works end to end and
/// does not change query results.
#[test]
fn vacuum_compact_flag() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    // Enough distinct, sizeable tokens per file that pruning almost all of
    // them frees whole pages, not just a handful of table rows (a handful
    // of dead rows can still fit in already-allocated pages, so the file
    // would not visibly shrink even though `compact` worked).
    for i in 0..60 {
        let body: String = (0..40)
            .map(|j| format!("fn f{i}_{j}_{}() {{}}\n", "x".repeat(20)))
            .collect();
        std::fs::write(root.join(format!("f{i}.rs")), body).unwrap();
    }
    let r = root.to_str().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();

    let (ok, out, err) = run(&["--db", &db, "index", "--org", "o", "--repo", "p", r]);
    assert!(ok, "{out}{err}");

    let (ok, out, err) = run(&["--db", &db, "vacuum", "--compact"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("vacuum:"), "{out}");
    assert!(out.contains("compact:"), "{out}");

    let f3_tok = format!("f3_0_{}", "x".repeat(20));
    let (ok, out, _) = run(&["--db", &db, "search", &f3_tok, "--json"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 1);

    // `--compact` after a real prune actually shrinks the file on disk, not
    // just runs without error: this is the property the whole feature
    // exists for, and a CLI-level assertion on it (not just a library-level
    // one) catches a future regression that silently reorders vacuum and
    // compact, or breaks the CLI's own stats reporting.
    for i in 1..60 {
        std::fs::remove_file(root.join(format!("f{i}.rs"))).unwrap();
    }
    let (ok, out, err) = run(&[
        "--db", &db, "index", "--org", "o", "--repo", "p", "--prune", r,
    ]);
    assert!(ok, "{out}{err}");
    let before = std::fs::metadata(&db).unwrap().len();
    let (ok, out, err) = run(&["--db", &db, "vacuum", "--compact"]);
    assert!(ok, "{out}{err}");
    let after = std::fs::metadata(&db).unwrap().len();
    assert!(after < before, "before={before} after={after} {out}");

    let f0_tok = format!("f0_0_{}", "x".repeat(20));
    let (ok, out, _) = run(&["--db", &db, "search", &f0_tok, "--json"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["results"].as_array().unwrap().len(),
        1,
        "surviving file still searchable"
    );
    let (ok, out, _) = run(&["--db", &db, "search", &f3_tok, "--json"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["results"].as_array().unwrap().len(),
        0,
        "pruned file's data is gone"
    );
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

/// Slice 3o: `describe --json` surfaces a repo's crashed/in-progress
/// chunked-ingest batch (ADR 0003 story 3, decision D3, `RepoInfo::open_batch`).
/// A real mid-batch process crash is impractical to script reliably in an
/// e2e test, so this stamps the `open_batch`/`meta.open_batch_id` marker
/// directly on the database via redb -- exercising exactly what a reader
/// observes on disk after a crash, without depending on a specific
/// crash-timing mechanism.
#[test]
fn describe_json_surfaces_a_crashed_batch() {
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

    // Before corruption: no open batch.
    let (ok, out, _) = run(&["--db", &db, "describe", "--json"]);
    assert!(ok);
    let before: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(before["repos"][0]["open_batch"], false);
    let (ok, out, _) = run(&["--db", &db, "describe"]);
    assert!(ok && !out.contains("WARNING"), "{out}");

    // Stamp the marker directly, as if a chunk committed mid-batch and the
    // process died before the final, marker-clearing chunk.
    let meta = TableDefinition::<&str, u64>::new("meta");
    let open_batch = TableDefinition::<&str, &str>::new("open_batch");
    {
        let raw = Database::open(&db).unwrap();
        let wt = raw.begin_write().unwrap();
        {
            wt.open_table(meta)
                .unwrap()
                .insert("open_batch_id", 0)
                .unwrap();
            let mut ob = wt.open_table(open_batch).unwrap();
            ob.insert("org", "o").unwrap();
            ob.insert("repo", "r").unwrap();
        }
        wt.commit().unwrap();
    }

    // After corruption: describe --json reports it, and human-readable
    // describe prints the warning.
    let (ok, out, err) = run(&["--db", &db, "describe", "--json"]);
    assert!(ok, "{err}");
    let after: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(after["repos"][0]["open_batch"], true, "{after}");
    let (ok, out, err) = run(&["--db", &db, "describe"]);
    assert!(
        ok && out.contains("WARNING") && out.contains("o/r") && out.contains("incomplete ingest"),
        "{out}{err}"
    );
}

/// `export` produces one well-formed JSON line per node through the `Store`
/// trait (not layout-specific).
#[test]
fn export_ndjson_round_trips_node_counts() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.rs");
    std::fs::write(
        &f,
        "fn foo() { bar(); }
",
    )
    .unwrap();
    {
        let db = d.path().join("g.redb");
        let (ok, out, err) = run(&[
            "--db",
            db.to_str().unwrap(),
            "index-file",
            "--org",
            "o",
            "--repo",
            "r",
            f.to_str().unwrap(),
        ]);
        assert!(ok, "{out}{err}");

        let out_path = d.path().join("g.ndjson");
        let (ok, _, err) = run(&[
            "--db",
            db.to_str().unwrap(),
            "export",
            "--out",
            out_path.to_str().unwrap(),
        ]);
        assert!(ok, "{err}");
        let text = std::fs::read_to_string(&out_path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines.is_empty());
        let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("line is not well-formed JSON: {e}: {line}");
            });
            *kinds
                .entry(v["kind"].as_str().unwrap().to_string())
                .or_default() += 1;
        }
        assert_eq!(kinds.get("org").copied().unwrap_or(0), 1, "{kinds:?}");
        assert_eq!(kinds.get("repo").copied().unwrap_or(0), 1, "{kinds:?}");
        assert_eq!(kinds.get("file").copied().unwrap_or(0), 1, "{kinds:?}");
        assert!(kinds.get("token").copied().unwrap_or(0) > 0, "{kinds:?}");
    }
}

/// Index `dir` with `--json` and the given extra flags; returns the summary
/// without `elapsed_ms`, and the stderr.
fn index_json(db: &str, dir: &str, extra: &[&str]) -> (serde_json::Value, String) {
    let mut args = vec!["--db", db, "index", "--org", "o", "--repo", "r", "--json"];
    args.extend_from_slice(extra);
    args.push(dir);
    let (ok, out, err) = run(&args);
    assert!(ok, "{out}{err}");
    let mut v: serde_json::Value = serde_json::from_str(&out).expect("stdout is pure JSON");
    v.as_object_mut().unwrap().remove("elapsed_ms");
    (v, err)
}

/// With `--deterministic` (fixed batches), parallel parsing still commits in
/// walk order, so the database file and the summary are byte-identical for
/// any `--jobs`: first index, forced re-index, unchanged.
#[test]
fn index_is_identical_for_any_jobs() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus");
    let corpus = corpus.to_str().unwrap();
    {
        let d = tempfile::tempdir().unwrap();
        let mut dbs = Vec::new();
        let mut sums = Vec::new();
        for jobs in ["1", "8"] {
            let db = d.path().join(format!("g{jobs}"));
            let db = db.to_str().unwrap();
            let (first, _) = index_json(db, corpus, &["--deterministic", "--jobs", jobs]);
            let (again, _) = index_json(
                db,
                corpus,
                &["--deterministic", "--jobs", jobs, "--reindex"],
            );
            let (skip, _) = index_json(db, corpus, &["--deterministic", "--jobs", jobs]);
            assert!(first["files"].as_u64().unwrap() > 100, "{first}");
            assert_eq!(skip["unchanged"], skip["files"], "{skip}");
            sums.push((first, again, skip));
            dbs.push(std::fs::read(db).unwrap());
        }
        assert_eq!(sums[0], sums[1], "summaries differ");
        assert!(dbs[0] == dbs[1], "database files differ");
    }
}

/// `--progress` draws on stderr only: stdout stays the pure JSON summary.
#[test]
fn progress_keeps_stdout_clean() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    for i in 0..20 {
        std::fs::write(root.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    std::fs::write(root.join("bin.dat"), b"a\0b").unwrap();
    let root = root.to_str().unwrap();
    let db = |n: &str| d.path().join(n).to_string_lossy().into_owned();
    let (quiet, qerr) = index_json(&db("a"), root, &["--no-progress", "-j", "3"]);
    let (shown, _) = index_json(&db("b"), root, &["--progress", "-j", "3"]);
    assert_eq!(quiet, shown);
    assert_eq!(quiet["files"], 20);
    assert!(qerr.is_empty(), "no progress output: {qerr}");
    let (ok, _, err) = run(&[
        "index",
        "--progress",
        "--no-progress",
        "--org",
        "o",
        "--repo",
        "r",
        root,
    ]);
    assert!(!ok && err.contains("cannot be used with"), "{err}");
}

/// The default (adaptive group commit: transaction sizes follow timing)
/// stores the same content as `--deterministic` whatever the thread count or
/// memory budget: the exports and summaries match exactly.
#[test]
fn adaptive_commit_stores_the_same_content() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus");
    let corpus = corpus.to_str().unwrap();
    {
        let d = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        for flags in [
            vec!["--deterministic", "-j", "1"],
            vec!["-j", "8"],
            vec!["-j", "3", "--memory", "64K"],
            // A budget below one fixed batch must not hang `--deterministic`.
            vec!["--deterministic", "-j", "2", "--memory", "64K"],
        ] {
            let db = d.path().join(format!("g{}", seen.len()));
            let db = db.to_str().unwrap();
            let (sum, _) = index_json(db, corpus, &flags);
            let (ok, export, err) = run(&["--db", db, "export"]);
            assert!(ok, "{err}");
            seen.push((flags, sum, export));
        }
        for w in seen.windows(2) {
            assert_eq!(w[0].1, w[1].1, "{:?} vs {:?} summary", w[0].0, w[1].0);
            assert!(w[0].2 == w[1].2, "{:?} vs {:?} export", w[0].0, w[1].0);
        }
    }
}

/// `--stats` adds a per-stage table on stderr (a `stats` object with
/// `--json`), and `--trace` writes a valid Chrome trace.
#[test]
fn stats_and_trace() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    for i in 0..30 {
        std::fs::write(root.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    let root = root.to_str().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let trace = d.path().join("t.json").to_string_lossy().into_owned();
    let (sum, _) = index_json(&db, root, &["--stats", "--trace", &trace]);
    let stages = sum["stats"]["stages"].as_array().unwrap();
    let names: Vec<_> = stages
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["walk", "parse", "commit"]);
    assert_eq!(stages[0]["items"], 30);
    assert_eq!(stages[1]["items"], 30);
    assert_eq!(stages[2]["items"], 30);
    let t: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let evs = t["traceEvents"].as_array().unwrap();
    assert!(evs
        .iter()
        .any(|e| e["name"].as_str().unwrap_or("").starts_with("parsing f")));
    assert!(evs.iter().any(|e| e["name"]
        .as_str()
        .unwrap_or("")
        .starts_with("committing txn")));
    let (ok, out, err) = run(&[
        "--db",
        &db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--stats",
        "--reindex",
        root,
    ]);
    assert!(ok, "{err}");
    assert!(out.starts_with("indexed o/r"), "stdout unchanged: {out}");
    assert!(
        err.contains("stage") && err.contains("commit") && err.contains("sizing:"),
        "{err}"
    );
}

/// Source bytes in flight never exceed `--memory` (unless one file alone is
/// bigger): held bytes stay counted until their transaction commits.
#[test]
fn memory_budget_bounds_bytes_in_flight() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    for i in 0..200 {
        let body = format!("fn f{i}() {{ {} }}\n", "let x = 1; ".repeat(200));
        std::fs::write(root.join(format!("f{i:03}.rs")), body).unwrap();
    }
    let root = root.to_str().unwrap();
    for mode in [&["-j", "8"][..], &["--deterministic", "-j", "8"][..]] {
        let db = d.path().join(format!("g{}", mode.len()));
        let mut flags = vec!["--stats", "--memory", "16K"];
        flags.extend_from_slice(mode);
        let (sum, _) = index_json(db.to_str().unwrap(), root, &flags);
        assert_eq!(sum["files"], 200, "{sum}");
        let peak = sum["stats"]["peak_in_flight"].as_u64().unwrap();
        let cap = sum["stats"]["memory_budget"].as_u64().unwrap();
        assert!(peak <= cap, "{mode:?}: peak {peak} > budget {cap}");
        if mode.len() == 2 {
            assert_eq!(cap, 16 * 1024, "adaptive keeps the requested budget");
            assert!(sum["stats"]["transactions"].as_u64().unwrap() > 1);
        }
    }
}

/// `--memory 25%` (also via `MEMORY_GRAPH_MEMORY`) budgets a share of the
/// free memory above the 20% kept for the OS, re-sampled during the run,
/// and `--stats` reports it.
#[test]
fn memory_share_follows_free_ram() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    for i in 0..40 {
        std::fs::write(root.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    let root = root.to_str().unwrap();
    let db = d.path().join("g").to_string_lossy().into_owned();
    let (sum, _) = index_json(&db, root, &["--stats", "--memory", "10%"]);
    let m = &sum["stats"]["memory"];
    let total = m["total"].as_u64();
    if let (Some(total), Some(avail)) = (total, m["available"].as_u64()) {
        let cap = m["budget_max"].as_u64().unwrap();
        // At most 10% of what was free above the reserve, divided by the
        // measured growth per source byte, plus the floor.
        // The estimate starts at INITIAL_EXPANSION and settles at the
        // measured value (a running average, so it can wobble a little on
        // the way), which puts the cap under the bound at the lower of the
        // two.
        let growth = m["expansion"].as_f64().unwrap();
        assert!((1.0..=64.0).contains(&growth), "{m}");
        let growth = growth.min(graph_cli::sysinfo::INITIAL_EXPANSION);
        let bound = (((avail + sum["stats"]["peak_in_flight"].as_u64().unwrap())
            .saturating_sub(total / 5)) as f64
            / 10.0
            / growth) as u64;
        // `available` is the end-of-run sample: allow free memory to have
        // moved by a quarter meanwhile (other tests run alongside).
        assert!(
            cap <= bound.max(256 << 20) * 5 / 4,
            "cap {cap} bound {bound}: {m}"
        );
        let reason = m["reason"].as_str().unwrap();
        // A loaded machine may already be under pressure; then the reason
        // says so and the cap only shrinks.
        if m["pressure_episodes"] == 0 {
            assert!(cap >= (256 << 20).min(total / 2), "{m}");
            assert!(reason.contains("10%"), "{m}");
        } else {
            assert!(reason.contains("pressure"), "{m}");
        }
    } else {
        assert!(m["reason"].as_str().unwrap().contains("unknown"), "{m}");
    }
    // Everything in flight was released by the end.
    assert_eq!(m["footprint_in_flight"], 0, "{m}");
    // The environment variable is the default for --memory.
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .env("MEMORY_GRAPH_MEMORY", "16K")
        .args([
            "--db",
            &db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--json",
            "--stats",
            "--reindex",
            root,
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["stats"]["memory_budget"], 16 * 1024, "{v}");
    assert_eq!(v["stats"]["memory"]["reason"], "fixed by --memory");
    let (ok, _, err) = run(&[
        "index", "--memory", "150%", "--org", "o", "--repo", "r", root,
    ]);
    assert!(!ok && err.contains("at most 100"), "{err}");
}
