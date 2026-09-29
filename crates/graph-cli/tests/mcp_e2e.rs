//! `memory-graph mcp` end to end over stdio with the real binary (ADR 0005
//! test plan 4, epic story 31): initialize, initialized, `tools/list`,
//! `tools/call search` on the vendored corpus with exact spans, equal to
//! `search --json`; against `--db` and against a `serve --server`. Stdout
//! carries only protocol messages; logs go to stderr.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

fn corpus_repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus/anyhow")
}

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_READ_DEADLINE")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn ok(args: &[&str]) -> String {
    let o = cmd().args(args).output().unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

/// A running `memory-graph mcp` with piped stdio.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn start(target: &[&str]) -> Mcp {
        let mut child = cmd()
            .args(target)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, v: &Value) {
        let w = self.stdin.as_mut().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "mcp closed stdout");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let r = self.recv();
        assert_eq!(r["id"], id, "{r}");
        r.get("result")
            .unwrap_or_else(|| panic!("{method}: {r}"))
            .clone()
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert_eq!(r["isError"], false, "{name}: {r}");
        let sc = r["structuredContent"].clone();
        let schema = graph_mcp::tools::output_schema(name).unwrap();
        graph_mcp::schema::validate(&schema, &sc).unwrap();
        sc
    }

    /// Close stdin; the process must exit 0. Its stdout after the last
    /// reply must be empty, and its stderr (logs) is returned.
    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.stdout, &mut rest).unwrap();
        assert_eq!(rest, "", "nothing but replies on stdout");
        let out = self.child.wait_with_output().unwrap();
        assert!(out.status.success(), "mcp exited with {:?}", out.status);
        String::from_utf8(out.stderr).unwrap()
    }
}

/// The byte offset of 1-based `line`, `col` (in Unicode scalar values).
fn offset_of(src: &str, line: u64, col: u64) -> usize {
    let start = src
        .split_inclusive('\n')
        .take(line as usize - 1)
        .map(str::len)
        .sum::<usize>();
    start
        + src[start..]
            .chars()
            .take(col as usize - 1)
            .map(char::len_utf8)
            .sum::<usize>()
}

/// initialize, initialized, tools/list, then `search` at the token and the
/// symbol grain; checked for exact spans and against `search --json`.
fn session(target: &[&str]) -> String {
    // The CLI's answers first: an embedded `mcp` holds the file lock.
    let cli = |extra: &[&str]| -> Value {
        let mut a: Vec<&str> = target.to_vec();
        a.extend(extra);
        a.push("--json");
        serde_json::from_str(&ok(&a)).unwrap()
    };
    let search = ["search", "Error", "--org", "corpus", "--repo", "anyhow"];
    let cli_tokens = cli(&[&search[..], &["--grain", "token", "--limit", "500"]].concat());
    let cli_symbols = cli(&[
        &search[..],
        &["--grain", "symbol", "--limit", "7", "--offset", "3"],
    ]
    .concat());
    let cli_find = cli(&["symbols", "Error*", "--limit", "500"]);
    let cli_describe = cli(&["describe"]);
    let mut m = Mcp::start(target);
    let init = m.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "mcp_e2e", "version": "0"}}),
    );
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "memory-graph");
    m.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = m.request("tools/list", json!({}));
    assert_eq!(tools["tools"].as_array().unwrap().len(), 7);

    // Token grain: every hit's span is exactly the text, in bytes and in
    // line/column.
    let page = m.tool(
        "search",
        json!({"text": "Error", "grain": "token", "org": "corpus", "repo": "anyhow", "limit": 500}),
    );
    let items = page["items"].as_array().unwrap();
    assert!(items.len() > 20, "{} hits", items.len());
    for h in items {
        let file = h["file"].as_str().unwrap();
        let src = std::fs::read_to_string(corpus_repo().join(file)).unwrap();
        let sp = &h["span"];
        let (s, e) = (
            sp["start"].as_u64().unwrap() as usize,
            sp["end"].as_u64().unwrap() as usize,
        );
        assert_eq!(&src[s..e], "Error", "{h}");
        assert_eq!(
            offset_of(
                &src,
                sp["start_line"].as_u64().unwrap(),
                sp["start_col"].as_u64().unwrap()
            ),
            s
        );
        assert_eq!(
            offset_of(
                &src,
                sp["end_line"].as_u64().unwrap(),
                sp["end_col"].as_u64().unwrap()
            ),
            e
        );
    }
    // The same rows `search --json` prints, page by page.
    assert_eq!(page["items"], cli_tokens["results"]);
    let sym = m.tool(
        "search",
        json!({"text": "Error", "org": "corpus", "repo": "anyhow", "limit": 7, "offset": 3}),
    );
    assert_eq!(sym["items"], cli_symbols["results"]);
    assert_eq!(sym["next_offset"], 10);
    assert_eq!(sym["stale_possible"], false);
    // find_symbols and describe equal their --json twins.
    let f = m.tool("find_symbols", json!({"pattern": "Error*", "limit": 500}));
    assert_eq!(f["items"], cli_find["results"]);
    let d = m.tool("describe", json!({}));
    assert_eq!(d["repos"], cli_describe["repos"]);
    m.finish()
}

fn indexed_db(dir: &Path) -> PathBuf {
    let db = dir.join("g.redb");
    let dbs = db.to_str().unwrap();
    ok(&[
        "--db",
        dbs,
        "index",
        "--org",
        "corpus",
        "--repo",
        "anyhow",
        corpus_repo().to_str().unwrap(),
    ]);
    db
}

#[test]
fn mcp_over_stdio_with_db() {
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(d.path());
    let logs = session(&["--db", db.to_str().unwrap()]);
    assert!(
        logs.contains("memory-graph mcp"),
        "logs go to stderr: {logs}"
    );
}

#[test]
fn mcp_over_stdio_with_server() {
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(d.path());
    let mut serve = cmd()
        .args(["serve", "--db"])
        .arg(&db)
        .args(["--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = serve.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("listening line");
    let addr = line
        .split("listening on ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .to_string();
    let result = std::panic::catch_unwind(|| session(&["--server", &addr]));
    let _ = serve.kill();
    let _ = serve.wait();
    let logs = result.unwrap_or_else(|p| std::panic::resume_unwind(p));
    assert!(logs.contains(&addr), "{logs}");
}

#[test]
fn mcp_refuses_a_missing_database_on_stderr() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("none.redb");
    let o = cmd()
        .args(["--db", db.to_str().unwrap(), "mcp"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty(), "stdout stays protocol-only");
    assert!(String::from_utf8_lossy(&o.stderr).contains("does not exist"));
}
