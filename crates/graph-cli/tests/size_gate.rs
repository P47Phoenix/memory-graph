//! On-disk size gate (the reason v1 was retired, ADR 0003 D5): a fresh
//! `memory-graph index` of a 10 GB tree reached 420 GB on the old per-node
//! format. This pins the database size relative to the source it holds and
//! to the tokens it stores, on the vendored corpus indexed twice (two repos,
//! so the dictionary is shared and the postings double), before and after
//! `vacuum --compact`. The thresholds are about 1.5x the measured values
//! (10.2x source and 70 B/token on this corpus, before and after compaction:
//! a fresh index has nothing to reclaim, and this small a corpus pays more
//! per token in page and dictionary overhead than the 40 B/token measured
//! at 10 M tokens in `docs/spikes/v2-checkpoint.md`), so a layout change
//! that doubles the footprint fails here, not on a user's disk. The
//! measured numbers are printed and put in every assertion.
mod common;

use common::readiness::{start_serve, ServeProcess, StartOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

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

/// `(files, bytes)` of every regular file under `root` (minus `.git`) whose
/// slash-separated relative path is not in `skip`.
fn source_size(root: &Path, skip: &std::collections::HashSet<String>) -> (usize, u64) {
    fn walk(
        root: &Path,
        dir: &Path,
        skip: &std::collections::HashSet<String>,
        acc: &mut (usize, u64),
    ) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if name != ".git" {
                    walk(root, &p, skip, acc);
                }
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if !skip.contains(&rel) {
                    acc.0 += 1;
                    acc.1 += p.metadata().unwrap().len();
                }
            }
        }
    }
    let mut acc = (0, 0);
    walk(root, root, skip, &mut acc);
    acc
}

#[test]
fn database_size_stays_within_bounds_of_source_and_tokens() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let db = db.to_str().unwrap();
    let corpus = corpus_dir();
    let corpus_s = corpus.to_str().unwrap();
    let mut tokens = 0u64;
    let mut source_bytes = 0u64;
    for repo in ["r1", "r2"] {
        let (ok, out, err) = run(&[
            "--db",
            db,
            "index",
            "--org",
            "o",
            "--repo",
            repo,
            "--json",
            "--no-progress",
            corpus_s,
        ]);
        assert!(ok, "{out}{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["failed"], 0, "{out}");
        tokens += v["tokens"].as_u64().unwrap();
        let skipped: std::collections::HashSet<String> = v["skipped_by_reason"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|paths| paths.as_array().unwrap().iter())
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        let (files, bytes) = source_size(&corpus, &skipped);
        assert_eq!(
            files as u64,
            v["files"].as_u64().unwrap(),
            "{repo}: the size measurement must cover exactly the indexed files ({out})"
        );
        source_bytes += bytes;
    }
    assert!(tokens > 100_000, "corpus is not tiny: {tokens} tokens");
    let db_bytes = std::fs::metadata(db).unwrap().len();
    let per_source = db_bytes as f64 / source_bytes as f64;
    let per_token = db_bytes as f64 / tokens as f64;
    println!(
        "size gate: db={db_bytes} B, source={source_bytes} B x2 repos, tokens={tokens}: \
         {per_source:.2}x source, {per_token:.1} B/token"
    );
    assert!(
        per_source <= 13.0,
        "database is {per_source:.2}x its source ({db_bytes} B for {source_bytes} B); limit 13.0x"
    );
    assert!(
        per_token <= 90.0,
        "database is {per_token:.1} B/token ({db_bytes} B for {tokens} tokens); limit 90 B/token"
    );

    let (ok, out, err) = run(&["--db", db, "vacuum", "--compact"]);
    assert!(ok, "{out}{err}");
    let db_bytes = std::fs::metadata(db).unwrap().len();
    let per_source = db_bytes as f64 / source_bytes as f64;
    let per_token = db_bytes as f64 / tokens as f64;
    println!(
        "size gate after compact: db={db_bytes} B: {per_source:.2}x source, {per_token:.1} B/token"
    );
    assert!(
        per_source <= 13.0,
        "compacted database is {per_source:.2}x its source ({db_bytes} B for {source_bytes} B); limit 13.0x"
    );
    assert!(
        per_token <= 90.0,
        "compacted database is {per_token:.1} B/token ({db_bytes} B for {tokens} tokens); limit 90 B/token"
    );
    // Still readable after compaction.
    let (ok, out, _) = run(&["--db", db, "describe"]);
    assert!(ok && out.contains("o/r1") && out.contains("o/r2"), "{out}");

    // ADR 0010 D4 / story 57: the folded symbol index costs about what
    // `sym_idx` does (one key per distinct folded name, the same ids), and
    // a small share of the file. Bounds about 1.5x the measured values.
    let store = graph_store::V2Store::open(db).unwrap();
    let (idx, fold) = store.symbol_index_bytes().unwrap();
    drop(store);
    let db_bytes = std::fs::metadata(db).unwrap().len();
    let fold_share = fold as f64 / db_bytes as f64;
    let fold_per_idx = fold as f64 / idx as f64;
    println!(
        "size gate sym_fold: {fold} B ({:.2}% of the file, {fold_per_idx:.2}x sym_idx at {idx} B)",
        fold_share * 100.0
    );
    assert!(fold > 0 && idx > 0, "both symbol indexes hold data");
    assert!(
        fold_per_idx <= SYM_FOLD_PER_SYM_IDX_MAX,
        "sym_fold is {fold_per_idx:.2}x sym_idx ({fold} B vs {idx} B); limit {SYM_FOLD_PER_SYM_IDX_MAX}x"
    );
    assert!(
        fold_share <= SYM_FOLD_SHARE_MAX,
        "sym_fold is {:.2}% of the file ({fold} B of {db_bytes} B); limit {:.1}%",
        fold_share * 100.0,
        SYM_FOLD_SHARE_MAX * 100.0
    );
}

/// `sym_fold` bytes per `sym_idx` byte (ADR 0010 D4; measured 0.99x on
/// this corpus, 2026-10-09).
const SYM_FOLD_PER_SYM_IDX_MAX: f64 = 1.5;
/// `sym_fold`'s share of the compacted file (measured 1.27%).
const SYM_FOLD_SHARE_MAX: f64 = 0.02;

/// #90: a full `--reindex` leaves the file about twice its live data (every
/// replacement is written before the old pages can be reused, and redb grows
/// a file under 4 GiB by doubling it). The run says so and names
/// `vacuum --compact`, which brings the file back within 1.2x of a fresh
/// index; an unchanged re-run (nothing replaced) prints no hint.
#[test]
fn reindex_hints_at_compact_and_compact_restores_the_size() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let db = db.to_str().unwrap();
    let corpus = corpus_dir();
    let corpus_s = corpus.to_str().unwrap();
    let index = |extra: &[&str]| {
        let mut args = vec![
            "--db",
            db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--no-progress",
        ];
        args.extend_from_slice(extra);
        args.push(corpus_s);
        let (ok, out, err) = run(&args);
        assert!(ok, "{out}{err}");
        err
    };
    // `--reindex` into an empty database replaces nothing: no hint, though
    // the file measures ~1.65x its live data (compact cannot beat that).
    let err = index(&["--reindex"]);
    assert!(!err.contains("vacuum --compact"), "fresh --reindex: {err}");
    let fresh = std::fs::metadata(db).unwrap().len();
    let err = index(&[]);
    assert!(!err.contains("vacuum --compact"), "unchanged rerun: {err}");
    let err = index(&["--reindex"]);
    let reindexed = std::fs::metadata(db).unwrap().len();
    println!("reindex: fresh={fresh} B, after --reindex={reindexed} B");
    assert!(
        err.contains("hint:") && err.contains("memory-graph vacuum --compact"),
        "--reindex grew the file {fresh} -> {reindexed} B without a hint: {err}"
    );
    let (ok, out, err) = run(&["--db", db, "vacuum", "--compact"]);
    assert!(ok, "{out}{err}");
    let compacted = std::fs::metadata(db).unwrap().len();
    println!("reindex: after vacuum --compact={compacted} B");
    assert!(
        compacted as f64 <= 1.2 * fresh as f64,
        "vacuum --compact left {compacted} B, fresh index was {fresh} B (limit 1.2x)"
    );
    // A `--reindex` that replaces nothing (an empty tree) gives no hint on
    // the compacted file; a real one does again.
    let empty = d.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let (ok, out, err) = run(&[
        "--db",
        db,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--no-progress",
        "--reindex",
        empty.to_str().unwrap(),
    ]);
    assert!(ok, "{out}{err}");
    assert!(!err.contains("vacuum --compact"), "no-op --reindex: {err}");
    let err = index(&["--reindex"]);
    assert!(
        err.contains("vacuum --compact"),
        "reindex after compact: {err}"
    );
}

/// #90: `index --reindex --compact` compacts the file after a run that
/// replaced files, so it ends within 1.2x of a fresh index (no separate
/// `vacuum --compact`), and prints no hint (there is nothing left to do).
#[test]
fn reindex_with_compact_ends_within_bounds_of_a_fresh_index() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let db = db.to_str().unwrap();
    let corpus = corpus_dir();
    let corpus_s = corpus.to_str().unwrap();
    let index = |extra: &[&str]| {
        let mut args = vec![
            "--db",
            db,
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--no-progress",
        ];
        args.extend_from_slice(extra);
        args.push(corpus_s);
        let (ok, out, err) = run(&args);
        assert!(ok, "{out}{err}");
        (out, err)
    };
    index(&[]);
    let fresh = std::fs::metadata(db).unwrap().len();
    let (out, err) = index(&["--reindex", "--compact"]);
    let compacted = std::fs::metadata(db).unwrap().len();
    println!("reindex --compact: fresh={fresh} B, after={compacted} B");
    assert!(out.contains("compact: "), "no compact line: {out}{err}");
    assert!(!err.contains("hint:"), "hint despite --compact: {err}");
    assert!(
        compacted as f64 <= 1.2 * fresh as f64,
        "--reindex --compact left {compacted} B, fresh index was {fresh} B (limit 1.2x)"
    );
}

/// The Raft log (ADR 0004 D7): every write is a log entry that carries the
/// source bytes, so before a snapshot `raft.redb` holds the source again
/// (and more: redb rounds large values up). After `cluster snapshot`
/// purges the log (`--log-keep-entries 0`), the log store compacts the
/// file, which must end at most 1.5x the source indexed, or a node's disk
/// keeps the high-water mark of every log between two snapshots (redb
/// reuses freed pages but never shrinks the file on its own).
#[test]
fn raft_log_after_snapshot_and_purge_stays_within_bounds_of_source() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("node");
    let mut serve = Command::new(env!("CARGO_BIN_EXE_memory-graph"));
    serve
        .env_remove("MEMORY_GRAPH_SERVER")
        .arg("serve")
        .arg("--data-dir")
        .arg(&dir)
        .args([
            "--bootstrap",
            "--node-id",
            "1",
            "--listen",
            "127.0.0.1:0",
            "--log-keep-entries",
            "0",
            "--min-free-disk",
            "1",
        ]);
    let ServeProcess { child, addr, .. } = start_serve(serve, StartOptions::default());
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let mut child = Kill(child);

    // Three passes (three repos) so the ratio measures the log, not redb's
    // fixed floor: an empty redb file is already 1.6 MB, about one pass of
    // this corpus (one pass measured 1.58x, all of it that floor plus one
    // growth region).
    let corpus = corpus_dir();
    let mut source_bytes = 0;
    for repo in ["r1", "r2", "r3"] {
        let (ok, out, err) = run(&[
            "--server",
            &addr,
            "index",
            "--org",
            "o",
            "--repo",
            repo,
            "--json",
            "--no-progress",
            corpus.to_str().unwrap(),
        ]);
        assert!(ok, "{out}{err}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let skipped: std::collections::HashSet<String> = v["skipped_by_reason"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|paths| paths.as_array().unwrap().iter())
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        source_bytes += source_size(&corpus, &skipped).1;
    }
    let log = dir.join("raft.redb");
    let before = std::fs::metadata(&log).unwrap().len();

    let (ok, out, err) = run(&["--server", &addr, "cluster", "snapshot", "--json"]);
    assert!(ok, "{out}{err}");
    let snap: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let snap_index = snap["last_applied_index"].as_u64().unwrap();
    // The purge runs after the snapshot is built; wait for it (bounded).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let (ok, out, _) = run(&["--server", &addr, "cluster", "status", "--json"]);
        let st: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
        if ok && st["purged_index"].as_u64().unwrap_or(0) >= snap_index {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the log was not purged up to the snapshot at {snap_index}: {out}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let s = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&addr)).unwrap();
    s.admin_shutdown(std::time::Duration::from_secs(10))
        .unwrap();
    drop(s);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.0.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "serve did not stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let after = std::fs::metadata(&log).unwrap().len();
    let ratio = after as f64 / source_bytes as f64;
    println!(
        "raft size gate: source={source_bytes} B, raft.redb before snapshot={before} B ({:.2}x), \
         after snapshot at {snap_index} + purge={after} B ({ratio:.2}x)",
        before as f64 / source_bytes as f64
    );
    assert!(
        after < before,
        "the purge after the snapshot did not shrink raft.redb ({before} B -> {after} B)"
    );
    assert!(
        ratio <= 1.5,
        "raft.redb is {ratio:.2}x its source after a snapshot and purge ({after} B for \
         {source_bytes} B); limit 1.5x"
    );
}
