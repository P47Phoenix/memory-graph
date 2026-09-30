//! `memory-graph serve --mcp-listen` end to end over the real binary (ADR
//! 0005 D1/D4/D5, epic story 32): the MCP endpoint on `127.0.0.1:0` answers
//! initialize, `tools/list` and `tools/call search` with exact spans;
//! `health` names it; a non-loopback bind is refused without
//! `--mcp-allow-remote`, and warned about at start with it.
use graph_server::testing::mcp_http::McpHttpClient;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

const LIB: &str =
    "pub struct Point { pub x: i32 }\n\npub fn origin() -> Point {\n    Point { x: 0 }\n}\n";

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_CONFIG")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn fill(db: &Path) {
    let s = graph_store::open_store(db, graph_cli::shipped_extractors()).unwrap();
    s.index_bytes("acme", "geo", "src/lib.rs", LIB.as_bytes(), None)
        .unwrap();
}

/// Lines of a pipe, drained by a thread.
fn lines(r: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(r).lines() {
            let Ok(l) = l else { return };
            let _ = tx.send(l);
        }
    });
    rx
}

struct Serve {
    child: Child,
    grpc: String,
    mcp: Option<SocketAddr>,
    stderr: Receiver<String>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `serve --db <db> --listen 127.0.0.1:0 <extra>`, once it prints its
/// listening line. The child is killed and waited for by `Serve`'s drop
/// (a panic before that point fails the test anyway).
#[allow(clippy::zombie_processes)]
fn serve(db: &Path, extra: &[&str]) -> Serve {
    let mut child = cmd()
        .args(["serve", "--db"])
        .arg(db)
        .args(["--listen", "127.0.0.1:0"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(child.stdout.take().unwrap());
    let stderr = lines(child.stderr.take().unwrap());
    let mut mcp = None;
    loop {
        let l = out
            .recv_timeout(Duration::from_secs(60))
            .expect("serve printed its listening line");
        // `memory-graph serve: mcp on http://<addr>/mcp`, then
        // `memory-graph serve: listening on <addr> (...)`.
        if let Some((_, rest)) = l.split_once("mcp on http://") {
            mcp = Some(rest.trim_end_matches("/mcp").parse().unwrap());
        }
        if let Some((_, rest)) = l.split_once("listening on ") {
            let grpc = rest.split_whitespace().next().unwrap().to_string();
            return Serve {
                child,
                grpc,
                mcp,
                stderr,
            };
        }
    }
}

#[test]
fn mcp_over_http_answers_with_exact_spans_and_health_names_it() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    fill(&db);
    let s = serve(&db, &["--mcp-listen", "127.0.0.1:0"]);
    let addr = s.mcp.expect("an `mcp on` line");
    assert!(addr.ip().is_loopback() && addr.port() != 0, "{addr}");

    let mut c = McpHttpClient::new(addr);
    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "describe",
            "list_repos",
            "search",
            "find_symbols",
            "file_outline",
            "file_tokens",
            "list_files"
        ]
    );
    let r = c.request(
        "tools/call",
        json!({"name": "search", "arguments": {"text": "origin", "grain": "symbol"}}),
    );
    let hit = &r["result"]["structuredContent"]["items"][0];
    assert_eq!(hit["symbol"], "origin", "{r}");
    let span = &hit["span"];
    let (start, end) = (
        span["start"].as_u64().unwrap() as usize,
        span["end"].as_u64().unwrap() as usize,
    );
    let text = &LIB[start..end];
    assert!(
        text.starts_with("pub fn origin()") && text.ends_with('}'),
        "{text:?}"
    );
    assert_eq!(span["start_line"], 3, "{span}");
    assert_eq!(r["result"]["structuredContent"]["stale_possible"], false);

    let o = cmd()
        .args(["--server", &s.grpc, "health"])
        .output()
        .unwrap();
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        ["SERVING".to_string(), format!("mcp: http://{addr}/mcp")],
        "{out}"
    );
}

#[test]
fn no_mcp_line_without_the_flag() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    fill(&db);
    let s = serve(&db, &[]);
    assert!(s.mcp.is_none());
}

#[test]
fn a_non_loopback_mcp_listen_needs_allow_remote_and_warns() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    fill(&db);
    let o = cmd()
        .args(["serve", "--db"])
        .arg(&db)
        .args(["--listen", "127.0.0.1:0", "--mcp-listen", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--mcp-allow-remote"), "{err}");
    // With the flag: it starts, and says so on stderr at every start.
    for _ in 0..2 {
        let s = serve(&db, &["--mcp-listen", "0.0.0.0:0", "--mcp-allow-remote"]);
        assert!(s.mcp.is_some());
        let warned = (0..200).any(|_| {
            s.stderr
                .recv_timeout(Duration::from_secs(30))
                .is_ok_and(|l| {
                    l.contains("WARNING: --mcp-allow-remote") && l.contains("no authentication")
                })
        });
        assert!(warned, "no --mcp-allow-remote warning on stderr");
    }
}

#[test]
fn serve_help_boxes_the_no_auth_warning() {
    let o = cmd().args(["serve", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&o.stdout);
    for part in [
        "WARNING: The MCP endpoint has no authentication. Anyone who can",
        "reach it can read every indexed source token. Keep it on loopback",
        "or behind an authenticating proxy until #105.",
        "+-----",
    ] {
        assert!(help.contains(part), "no {part:?} in serve --help:\n{help}");
    }
}
