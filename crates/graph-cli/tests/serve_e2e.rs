//! End to end over the real binary (ADR 0004 stage A, epic story 20):
//! `memory-graph serve` in one process, the CLI with `--server` in others.
//!
//! * The vendored corpus indexed and queried through a server answers
//!   byte for byte what an embedded run answers (timing normalised).
//! * Reads from a second process are served while an index run writes.
//! * The served file, reopened embedded after the server stops, answers
//!   exactly what the server answered.
//! * Target selection errors, the embedded-only flag refusals, `health`
//!   and `cluster leader` exit codes, the `Locked` message, and graceful
//!   shutdown removing the LOCK sidecar (Admin.Shutdown; SIGTERM on unix).
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

/// The corpus repos, in name order (each is indexed as one repo).
fn corpus_repos() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(corpus())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A `memory-graph` command with the target environment cleared (so the
/// developer's own MEMORY_GRAPH_SERVER cannot leak in) and a short lock
/// wait.
fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_WRITE_DEADLINE")
        .env_remove("MEMORY_GRAPH_READ_DEADLINE")
        .env_remove("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER")
        .env_remove("MEMORY_GRAPH_TESTING_WITHHOLD_LEADER")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn run(args: &[&str]) -> Output {
    cmd().args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `args` must succeed; its stdout.
fn ok(args: &[&str]) -> String {
    let o = run(args);
    assert!(
        o.status.success(),
        "{args:?} failed ({:?}):\n{}{}",
        o.status.code(),
        stdout(&o),
        stderr(&o)
    );
    stdout(&o)
}

/// Drop what differs between two runs of the same work: elapsed times,
/// and the `stale_possible` a server adds to JSON read output (checked on
/// its own in `json_reads_carry_stale_possible_and_server_takes_a_list`).
fn normalize(s: &str) -> String {
    let mut s = s
        .replace(",\"stale_possible\":false", "")
        .replace("\"stale_possible\":false,", "");
    for key in ["elapsed=", "\"elapsed_ms\":"] {
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(i) = rest.find(key) {
            out.push_str(&rest[..i + key.len()]);
            out.push('N');
            rest = rest[i + key.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
        }
        out.push_str(rest);
        s = out;
    }
    s
}

/// A running `memory-graph serve` on a free port (`--listen 127.0.0.1:0`;
/// the bound address is read from its "listening on" line).
struct Server {
    child: Child,
    addr: String,
    db: PathBuf,
    /// Every later stdout line of the server (the reader thread keeps
    /// draining the pipe so the server never blocks on, or fails, a print).
    lines: std::sync::mpsc::Receiver<String>,
}

impl Server {
    fn start(db: &Path) -> Server {
        Self::start_env(db, &[])
    }

    fn start_env(db: &Path, env: &[(&str, &str)]) -> Server {
        let mut c = cmd();
        c.envs(env.iter().copied()).args(["serve", "--db"]).arg(db);
        Self::spawn(c, db)
    }

    /// `c` (a `serve` command with its target flags: `--db <db>`, or
    /// `--data-dir ...` whose store is `db`), listening on a free port.
    fn spawn(mut c: Command, db: &Path) -> Server {
        let mut child = c
            .arg("--listen")
            .arg("127.0.0.1:0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            // Nobody listens any more; keep draining.
                            continue;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let line = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("serve printed its listening line");
        let addr = line
            .split("listening on ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        Server {
            child,
            addr,
            db: db.to_path_buf(),
            lines: rx,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `Admin.Shutdown`, then wait for the process to exit.
    fn shutdown(mut self) {
        let s = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&self.addr))
            .expect("connect for shutdown");
        s.admin_shutdown(Duration::from_secs(10))
            .expect("Admin.Shutdown");
        drop(s);
        self.wait_exit();
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                assert!(st.success(), "serve exited with {st:?}");
                return;
            }
            assert!(Instant::now() < deadline, "serve did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut p = self.db.as_os_str().to_owned();
        p.push(".LOCK");
        PathBuf::from(p)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The read commands compared between targets, as argument lists (the
/// target flags are added by the caller).
fn queries() -> Vec<Vec<&'static str>> {
    vec![
        vec!["describe"],
        vec!["describe", "--json"],
        vec!["search", "Subscribe"],
        vec!["search", "Subscribe", "--json"],
        vec!["search", "return", "--grain", "method", "--limit", "50"],
        vec!["search", "string", "--grain", "file", "--json"],
        vec![
            "search", "Handle", "--grain", "class", "--offset", "3", "--limit", "7",
        ],
        vec!["search", "//", "--kind", "comment", "--grain", "repo"],
        vec!["symbols", "*", "--limit", "300"],
        vec!["symbols", "Get*", "--json"],
        vec![
            "symbols",
            "*",
            "--kind",
            "method",
            "--language",
            "csharp",
            "--offset",
            "10",
            "--limit",
            "25",
        ],
        vec!["export"],
    ]
}

/// Run every query against `target` (`["--db", path]` or `["--server",
/// addr]`), returning the normalised outputs.
fn answers(target: &[&str]) -> Vec<String> {
    queries()
        .iter()
        .map(|q| {
            let mut a: Vec<&str> = target.to_vec();
            a.extend(q);
            normalize(&ok(&a))
        })
        .collect()
}

fn index_corpus(target: &[&str]) -> Vec<String> {
    let root = corpus();
    corpus_repos()
        .iter()
        .map(|repo| {
            let dir = root.join(repo);
            let dir = dir.to_str().unwrap();
            let mut a: Vec<&str> = target.to_vec();
            a.extend([
                "index",
                "--org",
                "corpus",
                "--repo",
                repo,
                "--no-progress",
                dir,
            ]);
            normalize(&ok(&a))
        })
        .collect()
}

fn assert_same(what: &str, a: &[String], b: &[String]) {
    assert_eq!(a.len(), b.len(), "{what}: different number of answers");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert!(
            x == y,
            "{what}: answer {i} differs\n--- first ---\n{}\n--- second ---\n{}",
            x.chars().take(3000).collect::<String>(),
            y.chars().take(3000).collect::<String>()
        );
    }
}

/// The corpus through a server answers what an embedded run answers; the
/// served file reopened embedded after a graceful shutdown answers the
/// same again, and the LOCK sidecar is gone.
#[test]
fn served_corpus_matches_embedded_and_survives_reopen() {
    let d = tempfile::tempdir().unwrap();
    let local = d.path().join("local.redb");
    let served = d.path().join("served.redb");
    let local_s = local.to_str().unwrap();
    let served_s = served.to_str().unwrap();

    let local_index = index_corpus(&["--db", local_s]);
    let local_answers = answers(&["--db", local_s]);
    // The comparison is only worth something if the answers are not empty.
    assert!(
        local_answers[0].contains("corpus/anyhow"),
        "{}",
        local_answers[0]
    );
    assert!(local_answers.iter().all(|a| !a.trim().is_empty()));

    let server = Server::start(&served);
    assert!(server.lock_path().is_file(), "serve writes <db>.LOCK");
    let addr = server.addr.clone();
    let remote_index = index_corpus(&["--server", &addr]);
    let remote_answers = answers(&["--server", &addr]);
    // Linearizable reads answer the same on a single node.
    let lin = answers(&["--server", &addr, "--read", "linearizable"]);
    assert_same(
        "index output (embedded vs --server)",
        &local_index,
        &remote_index,
    );
    assert_same(
        "queries (embedded vs --server)",
        &local_answers,
        &remote_answers,
    );
    assert_same("queries (local vs linearizable)", &remote_answers, &lin);
    // A re-run is all unchanged through the server too.
    let again = ok(&[
        "--server",
        &addr,
        "index",
        "--org",
        "corpus",
        "--repo",
        "anyhow",
        "--no-progress",
        corpus().join("anyhow").to_str().unwrap(),
    ]);
    assert!(again.contains("unchanged="), "{again}");

    let lock = server.lock_path();
    server.shutdown();
    assert!(
        !lock.exists(),
        "a graceful shutdown removes the LOCK sidecar"
    );

    let reopened = answers(&["--db", served_s]);
    assert_same(
        "queries (--server vs reopened embedded)",
        &remote_answers,
        &reopened,
    );
}

/// A second process reads while an index run is writing; every read
/// succeeds and the final state is complete.
#[test]
fn reads_are_served_during_an_index_run() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let server = Server::start(&db);
    let addr = server.addr.clone();
    let root = corpus();
    let mut writer = cmd()
        .args([
            "--server",
            &addr,
            "index",
            "--org",
            "corpus",
            "--repo",
            "all",
            "--no-progress",
        ])
        .arg(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reads = 0;
    loop {
        let done = writer.try_wait().unwrap().is_some();
        let o = run(&["--server", &addr, "describe", "--json"]);
        assert!(o.status.success(), "read during index: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
        assert!(v["repos"].is_array());
        reads += 1;
        if done {
            break;
        }
    }
    let out = writer.wait_with_output().unwrap();
    assert!(out.status.success(), "index: {}", stderr(&out));
    assert!(reads >= 1);
    let files: u64 = serde_json::from_str::<serde_json::Value>(&stdout(&out))
        .ok()
        .and_then(|v| v["files"].as_u64())
        .unwrap_or_else(|| {
            let s = stdout(&out);
            s.split("files=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap()
        });
    let desc: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &addr, "describe", "--json"])).unwrap();
    assert_eq!(desc["repos"][0]["files"].as_u64(), Some(files));
    server.shutdown();
}

/// `--db` with a server (flag or env) is an error naming both; `--read`
/// needs a server; the embedded-only flags are refused with a server.
/// None of this needs a server to be running.
#[test]
fn target_selection_errors() {
    let o = run(&["--db", "x.redb", "--server", "127.0.0.1:1", "describe"]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(
        e.contains("--db `x.redb`") && e.contains("--server 127.0.0.1:1"),
        "{e}"
    );

    let o = cmd()
        .env("MEMORY_GRAPH_SERVER", "127.0.0.1:2")
        .args(["--db", "x.redb", "describe"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(
        e.contains("--db `x.redb`") && e.contains("MEMORY_GRAPH_SERVER=127.0.0.1:2"),
        "{e}"
    );

    let o = run(&["--read", "linearizable", "describe"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("--read applies only with --server"),
        "{}",
        stderr(&o)
    );

    let o = run(&["--server", "127.0.0.1:1", "--chunk-bytes", "5", "describe"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("--chunk-bytes does not apply with --server")
            && stderr(&o).contains("8 MiB"),
        "{}",
        stderr(&o)
    );
    let d = tempfile::tempdir().unwrap();
    let o = run(&[
        "--server",
        "127.0.0.1:1",
        "--cache-bytes",
        "5",
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        d.path().to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("pass it to `memory-graph serve`"),
        "{}",
        stderr(&o)
    );

    // `health` and `cluster` need a server.
    let o = run(&["health"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("health needs --server"),
        "{}",
        stderr(&o)
    );
    // `serve` takes --db, not --server.
    let o = run(&["--server", "127.0.0.1:1", "serve"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("serve takes --db"), "{}", stderr(&o));
}

/// `health` exits 1 before the server is up and 0 once it serves (and
/// `--ready` once a leader is known); `cluster leader` names node 1;
/// `cluster status --json` and `sysinfo --server` describe the node.
#[test]
fn health_cluster_and_sysinfo() {
    // A port nothing listens on: bind, note, release.
    let free = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let o = run(&["--server", &free, "health"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "NOT_SERVING");

    let d = tempfile::tempdir().unwrap();
    let server = Server::start(&d.path().join("g.redb"));
    let addr = server.addr.clone();
    assert_eq!(ok(&["--server", &addr, "health"]).trim(), "SERVING");
    // The ready service follows the leader, which a single node elects at
    // once; allow it a moment.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let o = run(&["--server", &addr, "health", "--ready"]);
        if o.status.success() {
            break;
        }
        assert!(Instant::now() < deadline, "never ready: {}", stdout(&o));
        std::thread::sleep(Duration::from_millis(100));
    }
    let leader = ok(&["--server", &addr, "cluster", "leader"]);
    assert_eq!(leader.trim(), format!("1 {addr}"));
    let st: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &addr, "cluster", "status", "--json"])).unwrap();
    assert_eq!(st["node_id"], 1);
    assert_eq!(st["leader_id"], 1);
    assert_eq!(st["state"], "Leader");
    let text = ok(&["--server", &addr, "sysinfo"]);
    assert!(
        text.starts_with(&format!("server {addr} node 1 (leader: 1)\n")),
        "{text}"
    );
    assert!(text.contains("cpus:"), "{text}");
    let j: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &addr, "sysinfo", "--json"])).unwrap();
    assert_eq!(j["node_id"], 1);
    assert!(j["report"].is_object() && j["report"].get("text").is_none());
    // vacuum through the server, with the server's own compact.
    let v = ok(&["--server", &addr, "vacuum", "--compact"]);
    assert!(
        v.contains("vacuum: removed") && v.contains("compact: "),
        "{v}"
    );
    server.shutdown();
    let o = run(&["--server", &addr, "health"]);
    assert_eq!(o.status.code(), Some(1), "after shutdown: {}", stdout(&o));
}

/// An embedded open of a served file waits a little, then names the
/// server's pid and address; a second `serve` on the file says the same.
#[test]
fn locked_file_names_the_server() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let server = Server::start(&db);
    let db_s = db.to_str().unwrap();
    let t0 = Instant::now();
    let o = run(&["--db", db_s, "describe"]);
    assert_eq!(o.status.code(), Some(1));
    // MEMORY_GRAPH_LOCK_WAIT_MS=300: it retried, then gave up.
    assert!(t0.elapsed() >= Duration::from_millis(250));
    let e = stderr(&o);
    let want = format!(
        "is locked by pid {} (memory-graph serve on {}); use --server {} or stop it",
        server.pid(),
        server.addr,
        server.addr
    );
    assert!(e.contains(&want), "{e}\nwanted: {want}");

    let o = run(&["serve", "--db", db_s, "--listen", "127.0.0.1:0"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains(&want), "{}", stderr(&o));
    server.shutdown();
    // Free again: the embedded open works.
    ok(&["--db", db_s, "describe"]);
}

/// SIGTERM stops the server gracefully too: exit 0, LOCK sidecar gone.
#[cfg(unix)]
#[test]
fn sigterm_shuts_down_gracefully() {
    let d = tempfile::tempdir().unwrap();
    let mut server = Server::start(&d.path().join("g.redb"));
    let lock = server.lock_path();
    assert!(lock.is_file());
    let st = Command::new("kill")
        .args(["-TERM", &server.pid().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    server.wait_exit();
    assert!(!lock.exists(), "SIGTERM removes the LOCK sidecar");
}

/// Exit codes 3, 4 and 5 through the real binary, against an in-process
/// server whose test hooks withhold the leader or answer `Hello` with a
/// foreign protocol version.
#[test]
fn exit_codes_3_4_5_end_to_end() {
    use graph_server::testing::TestServer;
    let d = tempfile::tempdir().unwrap();
    let no_leader = TestServer::start_with(&d.path().join("a.redb"), vec![], |c| {
        c.testing.withhold_leader = true;
    });
    let addr = no_leader.endpoint();
    // 3: `cluster leader` finds none.
    let o = run(&["--server", &addr, "cluster", "leader"]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    // 4: a write gets no leader within its deadline.
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), "alpha beta").unwrap();
    let t0 = Instant::now();
    let o = run(&[
        "--server",
        &addr,
        "--write-deadline",
        "500ms",
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--no-progress",
        src.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "it retried until its --write-deadline first"
    );
    assert!(
        stderr(&o).contains(&format!("server {addr}")),
        "{}",
        stderr(&o)
    );
    // Reads still work without a leader (LOCAL).
    ok(&["--server", &addr, "describe"]);
    drop(no_leader);

    // 5: the server speaks another protocol version.
    let foreign = TestServer::start_with(&d.path().join("b.redb"), vec![], |c| {
        c.testing.hello_protocol_version = Some(99);
    });
    let o = run(&["--server", &foreign.endpoint(), "describe"]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(stderr(&o).contains("protocol version 99"), "{}", stderr(&o));
}

/// The server dies in the middle of an index run: the client does not
/// call that a protocol error (exit 5); it retries the write until its
/// deadline and then fails naming the server and the lost connection.
///
/// Deterministic: the server's test hook
/// (`MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER=0`) parks every write
/// proposal, so the run's `Index` call is in flight and can never finish;
/// the test waits for the server's "writes stalled" line, then kills it.
#[test]
fn server_killed_mid_index_is_a_lost_connection_not_a_protocol_error() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), "alpha beta").unwrap();
    std::fs::write(src.join("b.rs"), "fn b() {}").unwrap();
    let mut server = Server::start_env(
        &d.path().join("g.redb"),
        &[("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER", "0")],
    );
    let addr = server.addr.clone();
    let writer = cmd()
        .args([
            "--server",
            &addr,
            "--write-deadline",
            "500ms",
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--no-progress",
        ])
        .arg(&src)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let line = server
        .lines
        .recv_timeout(Duration::from_secs(60))
        .expect("the server reached its write stall");
    assert!(line.contains("writes stalled after 0"), "{line}");
    // Only this test's own child process.
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let t0 = Instant::now();
    let out = writer.wait_with_output().unwrap();
    let code = out.status.code();
    let err = stderr(&out);
    assert_ne!(code, Some(5), "not a protocol error: {err}");
    assert_ne!(code, Some(0), "the run cannot have succeeded: {err}");
    assert!(
        err.contains(&format!("server {addr}")) && err.contains("connection lost"),
        "the message names the server and the lost connection: {err}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "--write-deadline 500ms bounds the retries"
    );
}

/// Stage D (ADR 0004 D8): `--json` read output over a server carries
/// `stale_possible` (false on a single node, which always leads), an
/// embedded run's does not; `--server` and `MEMORY_GRAPH_SERVER` take a
/// comma-separated list and skip a dead endpoint.
#[test]
fn json_reads_carry_stale_possible_and_server_takes_a_list() {
    let d = tempfile::tempdir().unwrap();
    let embedded = d.path().join("e.redb");
    let src = d.path().join("a.rs");
    std::fs::write(&src, "fn a() {}\n").unwrap();
    let src = src.to_str().unwrap();
    let e = embedded.to_str().unwrap();
    ok(&["--db", e, "index-file", "--org", "o", "--repo", "r", src]);
    for q in [
        vec!["describe", "--json"],
        vec!["search", "a", "--json"],
        vec!["symbols", "a", "--json"],
    ] {
        let mut a = vec!["--db", e];
        a.extend(&q);
        let j: serde_json::Value = serde_json::from_str(&ok(&a)).unwrap();
        assert!(j.get("stale_possible").is_none(), "embedded {q:?}: {j}");
    }
    let server = Server::start(&d.path().join("s.redb"));
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let list = format!("{dead}, {}", server.addr);
    ok(&[
        "--server",
        &list,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        src,
    ]);
    for q in [
        vec!["describe", "--json"],
        vec!["search", "a", "--json"],
        vec!["symbols", "a", "--json"],
    ] {
        let mut a = vec!["--server", list.as_str()];
        a.extend(&q);
        let j: serde_json::Value = serde_json::from_str(&ok(&a)).unwrap();
        assert_eq!(j["stale_possible"], false, "server {q:?}: {j}");
    }
    let o = cmd()
        .env("MEMORY_GRAPH_SERVER", &list)
        .args(["describe", "--json", "--read", "linearizable"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["repos"][0]["repo"], "r");
    assert_eq!(j["stale_possible"], false);
    let o = run(&["--server", " , ", "describe"]);
    assert!(!o.status.success());
    server.shutdown();
}

/// Stage D review (QA 2): a linearizable read that finds no leader fails
/// with exit code 4, bounded by `--read-deadline`, and explains itself as a
/// read (never the write's "not acknowledged ... rerunning it is safe");
/// a local read on the same node still answers, flagged `stale_possible`.
/// Deterministic: the server's test hook
/// (`MEMORY_GRAPH_TESTING_WITHHOLD_LEADER`) makes it know no leader.
#[test]
fn leaderless_linearizable_read_is_a_bounded_read_failure() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let src = d.path().join("a.rs");
    std::fs::write(&src, "fn a() {}\n").unwrap();
    let db_s = db.to_str().unwrap();
    ok(&[
        "--db",
        db_s,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        src.to_str().unwrap(),
    ]);
    let server = Server::start_env(&db, &[("MEMORY_GRAPH_TESTING_WITHHOLD_LEADER", "1")]);
    let t0 = Instant::now();
    let o = run(&[
        "--server",
        &server.addr,
        "--read",
        "linearizable",
        "--read-deadline",
        "300ms",
        "describe",
    ]);
    let took = t0.elapsed();
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(4), "{err}");
    assert!(
        err.contains("linearizable read found no leader") && err.contains("--read-deadline"),
        "{err}"
    );
    assert!(
        !err.contains("not acknowledged"),
        "a read, not a write: {err}"
    );
    assert!(
        took < Duration::from_secs(4),
        "--read-deadline 300ms bounds the read (default 5 s): {took:?}"
    );
    // The same bound from the environment.
    let o = cmd()
        .env("MEMORY_GRAPH_READ_DEADLINE", "300ms")
        .args([
            "--server",
            &server.addr,
            "--read",
            "linearizable",
            "describe",
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    let j: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &server.addr, "describe", "--json"])).unwrap();
    assert_eq!(j["repos"][0]["repo"], "r");
    assert_eq!(j["stale_possible"], true, "no leader known: {j}");
    server.shutdown();
}

/// Idle wakeups of a served node (issue #205): an idle `serve` must not
/// wake on its own more than a few times a second. On Docker Desktop for
/// macOS every wakeup costs a VM exit, billed to the VM rather than the
/// container, so the wakeup rate is the number to guard, not CPU time.
///
/// A wakeup is a context switch, voluntary or not, of any of the server's
/// threads (`/proc/<pid>/task/*/status`), the same count
/// `scripts/idle-cpu.sh` reports for a container. See
/// `docs/spikes/idle-cpu.md` for the measurements behind the limits.
#[cfg(target_os = "linux")]
mod idle {
    use super::*;
    use std::collections::HashMap;

    /// How long a fresh server is left to settle (election, readiness,
    /// first metrics) before the window starts.
    const SETTLE: Duration = Duration::from_secs(2);
    /// The measured window.
    const WINDOW: Duration = Duration::from_secs(10);
    /// How often `/proc` is sampled within the window. Reading `/proc`
    /// does not wake the server; sampling often only keeps the switches of
    /// a thread that exits mid-window (a tokio blocking-pool thread, say)
    /// from being lost.
    const SAMPLE: Duration = Duration::from_millis(200);

    /// Voluntary plus nonvoluntary context switches of each live thread of
    /// `pid`, by thread id.
    fn switches(pid: u32) -> HashMap<u64, u64> {
        let mut m = HashMap::new();
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return m;
        };
        for e in dir.flatten() {
            let Ok(tid) = e.file_name().to_string_lossy().parse::<u64>() else {
                continue;
            };
            // The thread may have exited since the listing.
            let Ok(status) = std::fs::read_to_string(e.path().join("status")) else {
                continue;
            };
            let n = status
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("voluntary_ctxt_switches:")
                        .or_else(|| l.strip_prefix("nonvoluntary_ctxt_switches:"))
                })
                .filter_map(|v| v.trim().parse::<u64>().ok())
                .sum();
            m.insert(tid, n);
        }
        m
    }

    /// Wakeups per second of `pid` over [`WINDOW`], and the most threads
    /// seen. A thread already there at the start counts from its first
    /// sample, one born in the window counts in full, and one that exits
    /// counts up to its last sample.
    fn wakeups_per_s(pid: u32) -> (f64, usize) {
        let first = switches(pid);
        let mut last = first.clone();
        let mut threads = first.len();
        let start = Instant::now();
        while start.elapsed() < WINDOW {
            std::thread::sleep(SAMPLE);
            let now = switches(pid);
            threads = threads.max(now.len());
            last.extend(now);
        }
        let secs = start.elapsed().as_secs_f64();
        let total: u64 = last
            .iter()
            .map(|(tid, n)| n.saturating_sub(first.get(tid).copied().unwrap_or(0)))
            .sum();
        (total as f64 / secs, threads)
    }

    fn check(what: &str, server: Server, max_per_s: f64) {
        std::thread::sleep(SETTLE);
        let (rate, threads) = wakeups_per_s(server.pid());
        println!(
            "idle {what}: {rate:.1} wakeups/s over {}s, {threads} threads (limit {max_per_s})",
            WINDOW.as_secs()
        );
        assert!(
            rate < max_per_s,
            "idle {what} woke {rate:.1} times/s ({threads} threads), limit {max_per_s} (issue #205)"
        );
        server.shutdown();
    }

    /// `serve --db`. Measured in Docker on 32 vCPUs (60 s windows): about
    /// 40 wakeups/s before issue #205 (a 75 ms Raft tick fanned out to the
    /// readiness and snapshot-policy loops on every tick), 2.9/s after,
    /// all of it openraft's tick timer (every 750 ms with the 500 ms
    /// heartbeat). This test measured 3.0/s, and 50.5/s on the code before
    /// the fix (Linux container, same machine). 8/s is well over twice the
    /// floor, room for a busy CI runner, and far under the old rate.
    #[test]
    fn idle_serve_db_barely_wakes() {
        let d = tempfile::tempdir().unwrap();
        let server = Server::start(&d.path().join("g.redb"));
        check("serve --db", server, 8.0);
    }

    /// `serve --data-dir --bootstrap`, one node. Measured as above: 8.3/s
    /// before issue #205, 5.5/s after (the cluster's 250 ms heartbeat makes
    /// the tick timer wake every 375 ms); this test measured 5.7/s, and
    /// 10.6/s before the fix. That difference is too small to pin on a
    /// shared runner without flakes, so 12/s guards against the gross
    /// regressions (a sole voter ticking again with a per-tick fan-out, or
    /// a short heartbeat: tens of wakeups per second), not the last 3/s.
    #[test]
    fn idle_serve_data_dir_barely_wakes() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("node");
        let mut c = cmd();
        c.arg("serve")
            .arg("--data-dir")
            .arg(&dir)
            .args(["--bootstrap", "--node-id", "1"])
            // The test machine's free space is not what is being tested.
            .args(["--min-free-disk", "1"]);
        let server = Server::spawn(c, &dir.join("graph.redb"));
        check("serve --data-dir", server, 12.0);
    }
}
