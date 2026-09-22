//! Churn measurement for the v2 store (ADR 0003 story 3): index a directory of
//! sources, then repeatedly replace every file with slightly different content,
//! vacuuming after each round, and print the file size each time.
//!
//! `cargo run --release -p graph-store --example churn -- <dir> [rounds]`
use graph_core::FallbackExtractor;
use graph_store::{BatchFile, IndexOptions, Store, V2Store};
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
    let rounds: usize = args.next().map_or(6, |r| r.parse().unwrap());
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    files.sort();
    let tmp = tempfile_path();
    let mut s = V2Store::open(&tmp).unwrap();
    s.register(Box::new(FallbackExtractor::new("rust")));
    let size = || std::fs::metadata(&tmp).unwrap().len() as f64 / (1 << 20) as f64;
    let mut toks = 0usize;
    for round in 0..=rounds {
        // Round 0 is the initial index; later rounds change every file.
        let srcs: Vec<String> = files
            .iter()
            .map(|(_, c)| format!("{c}\n// churn round {round} {}\n", "x".repeat(round * 3)))
            .collect();
        let batch: Vec<BatchFile<'_>> = srcs
            .iter()
            .zip(&files)
            .map(|(c, (p, _))| BatchFile {
                path: p,
                bytes: c.as_bytes(),
                language: Some("rust"),
                origin: None,
            })
            .collect();
        let t = std::time::Instant::now();
        let r = Store::index_batch(&s, "o", "r", &batch, IndexOptions { reindex: true }).unwrap();
        toks = r.iter().map(|x| x.as_ref().unwrap().tokens).sum();
        let before = size();
        let v = s.vacuum().unwrap();
        println!(
            "round {round}: {} files {toks} tokens, index {:.2}s, file {before:.2} MiB, \
             after vacuum {:.2} MiB (removed {} terms, kept {})",
            files.len(),
            t.elapsed().as_secs_f64(),
            size(),
            v.terms_removed,
            v.terms_kept
        );
    }
    println!("tokens per pass {toks}");
    std::fs::remove_file(&tmp).ok();
}

fn tempfile_path() -> std::path::PathBuf {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    Path::new(&base).join(format!("churn-{}.redb", std::process::id()))
}
