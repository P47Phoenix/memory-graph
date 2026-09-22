//! Prune-then-vacuum churn measurement for the v2 store (ADR 0003 story 3):
//! index a directory, then repeatedly prune down to a small subset (dropping
//! most files) and vacuum, then restore the full set and vacuum, printing
//! the file size at each step. Fills the "not measured" gap left by
//! `churn.rs` (which only replaces files, never removes most of the store).
//!
//! `cargo run --release -p graph-store --example prune_churn -- <dir> [rounds]`
use graph_core::FallbackExtractor;
use graph_store::{BatchFile, IndexOptions, Store, V2Store, ORIGIN_DIRECTORY};
use std::collections::HashSet;
use std::path::Path;

fn walk(p: &Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(p).unwrap().flatten() {
        let path = e.path();
        if path.is_dir() {
            if !path.ends_with("target") && !path.ends_with(".git") {
                walk(&path, out);
            }
        } else if path.extension().is_some_and(|x| x == "rs") {
            if let Ok(s) = std::fs::read_to_string(&path) {
                out.push((path.display().to_string(), s));
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("dir");
    let rounds: usize = args.next().map_or(4, |r| r.parse().unwrap());
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    files.sort();
    let tmp = tempfile_path();
    let mut s = V2Store::open(&tmp).unwrap();
    s.register(Box::new(FallbackExtractor::new("rust")));
    let size = || std::fs::metadata(&tmp).unwrap().len() as f64 / (1 << 20) as f64;

    let batch: Vec<BatchFile<'_>> = files
        .iter()
        .map(|(p, c)| BatchFile {
            path: p,
            bytes: c.as_bytes(),
            language: Some("rust"),
            origin: Some(ORIGIN_DIRECTORY),
        })
        .collect();
    Store::index_batch(&s, "o", "r", &batch, IndexOptions { reindex: false }).unwrap();
    let all: HashSet<String> = files.iter().map(|(p, _)| p.clone()).collect();
    println!(
        "round 0 (index all {} files): {:.2} MiB",
        files.len(),
        size()
    );

    for round in 1..=rounds {
        // Prune down to one file (the sharpest case: delete almost everything).
        let keep_one: HashSet<String> = files.first().map(|(p, _)| p.clone()).into_iter().collect();
        let removed = s.prune_files("o", "r", &keep_one, false).unwrap();
        let before_vacuum = size();
        let v = s.vacuum().unwrap();
        println!(
            "round {round}: pruned {} files, file {before_vacuum:.2} MiB, after vacuum {:.2} MiB \
             (removed {} terms, kept {})",
            removed.len(),
            size(),
            v.terms_removed,
            v.terms_kept
        );

        // Restore the full set.
        Store::index_batch(&s, "o", "r", &batch, IndexOptions { reindex: false }).unwrap();
        let before_vacuum = size();
        let v = s.vacuum().unwrap();
        println!(
            "round {round}: restored to {} files, file {before_vacuum:.2} MiB, after vacuum {:.2} MiB \
             (removed {} terms, kept {})",
            all.len(),
            size(),
            v.terms_removed,
            v.terms_kept
        );
    }
    std::fs::remove_file(&tmp).ok();
}

fn tempfile_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("prune-churn-{}.redb", std::process::id()))
}
