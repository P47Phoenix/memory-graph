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
