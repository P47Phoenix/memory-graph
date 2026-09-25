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
}
