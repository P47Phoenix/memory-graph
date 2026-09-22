//! Compaction measurement for the v2 store (ADR 0003 story 3, slice 3f):
//! index a directory, prune down to one file, vacuum (which never shrinks
//! the file on its own -- see `prune_churn.rs`), then `compact`, printing
//! the file size at each step. Closes the loop on the `churn`/`prune_churn`
//! measurements that showed the file never shrinks without a rebuild.
//!
//! `cargo run --release -p graph-store --example compact_churn -- <dir>`
use graph_core::{normalize_path, FallbackExtractor};
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
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    files.sort();
    let tmp = tempfile_path();
    let mut s = V2Store::open(&tmp).unwrap();
    s.register(Box::new(FallbackExtractor::new("rust")));
    let size = || std::fs::metadata(&tmp).unwrap().len() as f64 / (1 << 20) as f64;
    let bytes = || std::fs::metadata(&tmp).unwrap().len();

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
    println!(
        "index all {} files: {:.2} MiB ({} bytes)",
        files.len(),
        size(),
        bytes()
    );

    let keep_one: HashSet<String> = files
        .first()
        .map(|(p, _)| normalize_path(p))
        .into_iter()
        .collect();
    let removed = s.prune_files("o", "r", &keep_one, false).unwrap();
    println!(
        "pruned {} of {} files: {:.2} MiB ({} bytes)",
        removed.len(),
        files.len(),
        size(),
        bytes()
    );

    let v = s.vacuum().unwrap();
    println!(
        "vacuum: {:.2} MiB ({} bytes), removed {} terms, kept {}",
        size(),
        bytes(),
        v.terms_removed,
        v.terms_kept
    );

    let before = bytes();
    let (s, cst) = s.compact().unwrap();
    println!(
        "compact: {} bytes -> {} bytes ({:.1}x smaller)",
        cst.before_bytes,
        cst.after_bytes,
        cst.before_bytes as f64 / cst.after_bytes.max(1) as f64
    );
    assert_eq!(before, cst.before_bytes);
    drop(s);

    std::fs::remove_file(&tmp).ok();
}

fn tempfile_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("compact-churn-{}.redb", std::process::id()))
}
