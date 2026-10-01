//! #74, #165 end to end over the real binary: a repo whose stored symbols
//! came from an extractor the parsing store lacks (here a toy language no
//! build ships, registered only by this test) is refused by `index` /
//! `index --reindex` / `index-file` unless `--force`, embedded and through
//! `--server`, with the same message; the gap is reported over `--server`
//! (the `Store.ExtractorGaps` RPC); and `serve` logs it at startup.
use graph_core::tokenizer::tokenize;
use graph_core::{Extraction, Extractor, SymbolDecl, SymbolKind};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

/// A language no shipped build has: one symbol over the first token.
struct Toy;

impl Extractor for Toy {
    fn language(&self) -> &str {
        "toylang"
    }
    fn version(&self) -> String {
        "toy-7".into()
    }
    fn extensions(&self) -> &[&str] {
        &["toy"]
    }
    fn extract(&self, source: &str) -> Extraction {
        let tokens = tokenize(source);
        let span = tokens[0].span;
        Extraction {
            has_errors: false,
            symbols: vec![SymbolDecl {
                name: tokens[0].text.clone(),
                kind: SymbolKind::Function,
                lang_kind: Some("toy_fn".into()),
                span,
                owner: None,
            }],
            tokens,
        }
    }
}

/// A database with `o/r` holding `a.toy` indexed by [`Toy`] (one symbol).
fn seeded_db(db: &Path) {
    let s = graph_store::open_store(db, vec![Box::new(Toy)]).unwrap();
    s.index_bytes("o", "r", "a.toy", b"alpha beta\n", None)
        .unwrap();
    s.index_bytes("o", "other", "b.txt", b"gamma\n", None)
        .unwrap();
}

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_WRITE_DEADLINE")
        .env_remove("MEMORY_GRAPH_READ_DEADLINE")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn run(args: &[&str]) -> Output {
    cmd().args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(db: &Path) -> Server {
        let mut child = cmd()
            .args(["serve", "--db"])
            .arg(db)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(l);
            }
        });
        let line: String = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("serve printed its listening line");
        let addr = line
            .split("listening on ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        Server { child, addr }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(s) =
            graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&self.addr))
        {
            let _ = s.admin_shutdown(Duration::from_secs(10));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The symbols named `alpha` (the toy symbol), as `symbols --json` rows.
fn alpha_symbols(target: &[&str]) -> usize {
    let mut a = target.to_vec();
    a.extend(["symbols", "alpha", "--json"]);
    let o = run(&a);
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    v["results"].as_array().map_or(0, Vec::len)
}

/// Drop what legitimately differs between an embedded and a served run.
fn refusal_lines(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr)
        .lines()
        .filter(|l| l.contains("refusing") || l.contains("toylang") || l.contains("--force"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every refusal, then `--force`, on one target; returns the refusal text.
fn refuse_then_force(target: &[&str], src: &Path) -> String {
    let dir = src.to_str().unwrap();
    let file = src.join("a.toy");
    let file = file.to_str().unwrap();
    let idx = |extra: &[&str]| {
        let mut a = target.to_vec();
        a.extend(["index", "--org", "o", "--repo", "r"]);
        a.extend_from_slice(extra);
        a.push(dir);
        run(&a)
    };
    let mut first = String::new();
    for extra in [&[][..], &["--reindex"], &["--prune"]] {
        let o = idx(extra);
        let t = text(&o);
        assert!(!o.status.success(), "{extra:?} was not refused: {t}");
        assert!(
            t.contains("refusing to index o/r")
                && t.contains("toylang")
                && t.contains("toy-7")
                && t.contains("--force"),
            "{extra:?}: {t}"
        );
        if first.is_empty() {
            first = refusal_lines(&o);
        }
        // Nothing was written: the symbol is still there.
        assert_eq!(alpha_symbols(target), 1, "{extra:?}");
    }
    let mut a = target.to_vec();
    a.extend(["index-file", "--org", "o", "--repo", "r", file]);
    let o = run(&a);
    assert!(!o.status.success(), "index-file was not refused");
    assert!(text(&o).contains("refusing to index o/r"), "{}", text(&o));
    assert_eq!(alpha_symbols(target), 1);

    // Another repo has no gap: not refused.
    let mut a = target.to_vec();
    a.extend(["index", "--org", "o", "--repo", "other", dir]);
    let o = run(&a);
    assert!(o.status.success(), "{}", text(&o));

    // --force indexes anyway, warning; the symbols are gone, and so is the
    // gap, so a later run is not refused.
    let o = idx(&["--force", "--reindex"]);
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    assert!(
        t.contains("warning:") && t.contains("toylang") && t.contains("--force"),
        "{t}"
    );
    assert_eq!(alpha_symbols(target), 0);
    let o = idx(&[]);
    assert!(o.status.success(), "{}", text(&o));
    first
}

fn source(root: &Path) -> std::path::PathBuf {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.toy"), b"alpha beta\n").unwrap();
    src
}

#[test]
fn missing_extractor_is_refused_unless_forced_embedded_and_over_server() {
    let d = tempfile::tempdir().unwrap();
    let src = source(d.path());

    let emb = d.path().join("e.redb");
    seeded_db(&emb);
    let emb_s = emb.to_str().unwrap().to_string();
    let embedded = refuse_then_force(&["--db", &emb_s], &src);

    let srv_db = d.path().join("s.redb");
    seeded_db(&srv_db);
    let server = Server::start(&srv_db);
    // The gap is visible to a client through the RPC.
    let remote =
        graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&server.addr)).unwrap();
    let gaps = graph_store::Store::extractor_gaps(&remote, Some("o"), Some("r")).unwrap();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(
        (gaps[0].language.as_str(), gaps[0].stored_version.as_str()),
        ("toylang", "toy-7")
    );
    assert!(
        graph_store::Store::extractor_gaps(&remote, Some("o"), Some("other"))
            .unwrap()
            .is_empty()
    );
    drop(remote);
    let served = refuse_then_force(&["--server", &server.addr], &src);
    assert_eq!(
        embedded, served,
        "the refusal reads the same on both targets"
    );
}

/// #74: `serve` also logs the gap at startup (unchanged by #165).
#[test]
fn serve_logs_a_missing_extractor_at_startup() {
    const WARNING: &str = "toylang file(s) were indexed with extractor `toy-7`";
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    seeded_db(&db);
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
