//! #74 end to end: a database whose files were indexed by an extractor the
//! `memory-graph` binary does not have (a third-party `ToyLang`, registered
//! only by this test) makes `index`, `index-file` and `serve` warn, naming
//! the language and the stored extractor version.
use graph_core::{tokenizer::tokenize, Extraction, Extractor, Span, SymbolDecl, SymbolKind};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

struct Toy;

impl Extractor for Toy {
    fn language(&self) -> &str {
        "ToyLang"
    }
    fn version(&self) -> String {
        "toy-e2e-7".into()
    }
    fn extensions(&self) -> &[&str] {
        &["toy"]
    }
    fn extract(&self, source: &str) -> Extraction {
        let end = source.len() as u32;
        Extraction {
            has_errors: false,
            symbols: vec![SymbolDecl {
                owner: None,
                name: "whole".into(),
                kind: SymbolKind::Other,
                lang_kind: None,
                span: Span {
                    start: 0,
                    end,
                    start_line: 1,
                    start_col: 1,
                    end_line: 1,
                    end_col: end + 1,
                },
            }],
            tokens: tokenize(source),
        }
    }
}

/// A database with `o/r/a.toy` indexed by `Toy` (one symbol stored).
fn seed(db: &Path) {
    let s = graph_store::open_store(db, vec![Box::new(Toy)]).unwrap();
    s.index_bytes("o", "r", "a.toy", b"alpha", None).unwrap();
}

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

const WARNING: &str = "toylang file(s) were indexed with extractor `toy-e2e-7`";

#[test]
fn index_warns_about_a_missing_extractor() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    seed(&db);
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("b.txt"), "beta").unwrap();
    let o = cmd()
        .arg("--db")
        .arg(&db)
        .args(["index", "--org", "o", "--repo", "r", "--no-progress"])
        .arg(&src)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(err.contains(WARNING), "{err}");
    // Another repo has nothing from the missing extractor: no warning.
    let o = cmd()
        .arg("--db")
        .arg(&db)
        .args(["index", "--org", "o", "--repo", "other", "--no-progress"])
        .arg(&src)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && !err.contains("toylang"), "{err}");
}

#[test]
fn index_file_warns_about_a_missing_extractor() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    seed(&db);
    let f = d.path().join("c.txt");
    std::fs::write(&f, "gamma").unwrap();
    let o = cmd()
        .arg("--db")
        .arg(&db)
        .args(["index-file", "--org", "o", "--repo", "r"])
        .arg(&f)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(err.contains(WARNING), "{err}");
}

#[test]
fn serve_logs_a_missing_extractor_at_startup() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    seed(&db);
    let mut child = cmd()
        .args(["serve", "--db"])
        .arg(&db)
        .args(["--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(l);
            }
        });
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    let found = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) if l.contains(WARNING) => break true,
            Ok(l) => seen.push(l),
            Err(_) => break false,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(found, "no startup warning; output: {seen:#?}");
}
