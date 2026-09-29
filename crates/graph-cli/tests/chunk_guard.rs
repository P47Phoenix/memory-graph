//! `--deterministic` needs one fixed batch per transaction, so a chunk size
//! smaller than a batch is refused up front (a file larger than a batch is
//! a batch of its own, so no file term is needed).
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

fn index(db: &std::path::Path, root: &std::path::Path, flags: &[&str]) -> std::process::Output {
    let mut args = vec!["--db", db.to_str().unwrap()];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["--org", "o", "--repo", "r", root.to_str().unwrap()]);
    Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args(&args)
        .output()
        .unwrap()
}

fn summary(o: &std::process::Output) -> serde_json::Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// #88: a fixed batch is held until it commits, so `--deterministic` with a
/// fixed `--memory` below one batch is refused with the reason, instead of
/// the budget being silently raised to a batch.
#[test]
fn deterministic_refuses_memory_below_one_batch() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let db = d.path().join("g");
    let o = index(&db, &root, &["index", "--deterministic", "--memory", "16K"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        !o.status.success()
            && err.contains("--deterministic needs --memory of at least 33554432")
            && err.contains("one fixed batch"),
        "{err}"
    );
    assert!(!db.exists(), "nothing created");
    // One batch is enough, and a share of free memory is never refused.
    for m in ["32M", "10%"] {
        let o = index(
            &db,
            &root,
            &["index", "--deterministic", "--memory", m, "--json"],
        );
        assert_eq!(summary(&o)["files"], 1, "--memory {m}");
    }
}

/// #82: a fixed batch closes at 32 MiB, not only at 256 files: three 20 MiB
/// files are three batches (each one transaction under the 64 MiB chunk),
/// where a file-count-only batch would be one.
#[test]
fn deterministic_batches_close_on_bytes() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    let mut big = vec![b'z'; 20 << 20];
    big.push(b'\n');
    for n in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(n), &big).unwrap();
    }
    let db = d.path().join("g");
    let o = index(
        &db,
        &root,
        &["index", "--deterministic", "--stats", "--json"],
    );
    let v = summary(&o);
    assert_eq!(v["files"], 3, "{v}");
    assert_eq!(v["stats"]["transactions"], 3, "{v}");
}

/// #89: `--stats` `transactions` counts redb commits, not `index_prepared`
/// calls: with `--chunk-bytes 1` every stored file commits on its own, plus
/// the closing commit of each call, so there are more commits than files.
#[test]
fn stats_transactions_count_real_commits() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir(&root).unwrap();
    for i in 0..10 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("word{i}\n")).unwrap();
    }
    let db = d.path().join("g");
    let o = index(
        &db,
        &root,
        &["--chunk-bytes", "1", "index", "--stats", "--json"],
    );
    let v = summary(&o);
    assert_eq!(v["files"], 10, "{v}");
    let t = v["stats"]["transactions"].as_u64().unwrap();
    assert!(t > 10, "one commit per stored file plus one per call: {t}");
    // A re-run stores nothing: one (closing) commit per call, no more.
    let o = index(
        &db,
        &root,
        &["--chunk-bytes", "1", "index", "--stats", "--json"],
    );
    let v = summary(&o);
    assert_eq!(v["unchanged"], 10, "{v}");
    let t = v["stats"]["transactions"].as_u64().unwrap();
    assert!((1..=10).contains(&t), "{t}");
}
