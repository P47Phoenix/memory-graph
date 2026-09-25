//! `--deterministic` needs one fixed batch per transaction, so a chunk size
//! smaller than a batch plus one file is refused up front.
use std::process::Command;

#[test]
fn deterministic_refuses_small_chunks() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let db = d.path().join("g");
    let o = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--chunk-bytes",
            "1048576",
            "index",
            "--deterministic",
            "--org",
            "o",
            "--repo",
            "r",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        !o.status.success() && err.contains("--deterministic needs --chunk-bytes"),
        "{err}"
    );
    assert!(!db.exists(), "nothing created");
}
