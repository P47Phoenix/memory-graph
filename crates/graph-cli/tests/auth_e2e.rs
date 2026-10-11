//! ADR 0012 end to end with the real binary: `auth new-token`, a server
//! with `--auth-tokens`, the client's `--token-file` and environment
//! variables, exit codes 8 and 9, `--server https://` refused, the
//! `--insecure-transport` guard, and `mcp` over stdio with the target's
//! credentials. No output ever contains a token.
mod common;

use common::readiness::{start_serve, StartOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    for k in [
        "MEMORY_GRAPH_SERVER",
        "MEMORY_GRAPH_READ",
        "MEMORY_GRAPH_WRITE_DEADLINE",
        "MEMORY_GRAPH_READ_DEADLINE",
        "MEMORY_GRAPH_TOKEN",
        "MEMORY_GRAPH_TOKEN_FILE",
        "MEMORY_GRAPH_WORKER_THREADS",
        "TOKIO_WORKER_THREADS",
        "MEMORY_GRAPH_CONFIG",
    ] {
        c.env_remove(k);
    }
    c.env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `auth new-token`: the token on stdout, the stanza appended to `file`.
fn new_token(file: &Path, id: &str, role: &str) -> String {
    let o = cmd()
        .args(["auth", "new-token", "--id", id, "--role", role, "--append"])
        .arg(file)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let token = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert!(token.starts_with("mgt_") && token.len() == 47, "{token}");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!stderr.contains(&token[4..]), "{stderr}");
    assert!(!std::fs::read_to_string(file).unwrap().contains(&token[4..]));
    token
}

fn assert_no_token(o: &Output, tokens: &[&str]) {
    let t = text(o);
    for tok in tokens {
        assert!(!t.contains(&tok[4..]), "output contains a token: {t}");
    }
}

#[test]
fn tokens_roles_and_exit_codes() {
    let d = tempfile::tempdir().unwrap();
    let tokens = d.path().join("tokens.toml");
    let write = new_token(&tokens, "ci", "write");
    let read = new_token(&tokens, "bot", "read");
    let write_file = d.path().join("w.token");
    let read_file = d.path().join("r.token");
    std::fs::write(&write_file, format!("{write}\n")).unwrap();
    std::fs::write(&read_file, format!("{read}\n")).unwrap();
    let wrong_file = d.path().join("x.token");
    let o = cmd()
        .args([
            "auth",
            "new-token",
            "--id",
            "x",
            "--role",
            "read",
            "--token-out",
        ])
        .arg(&wrong_file)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    assert!(o.stdout.is_empty(), "--token-out: nothing on stdout");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [&tokens, &wrong_file] {
            let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}: {mode:o}", f.display());
        }
    }

    let db = d.path().join("g.redb");
    let mut c = cmd();
    c.args(["serve", "--db"])
        .arg(&db)
        .arg("--auth-tokens")
        .arg(&tokens)
        .args(["--listen", "127.0.0.1:0"]);
    let mut s = start_serve(c, StartOptions::default());
    let addr = s.addr.clone();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "fn alpha() {}\n").unwrap();
    let all = [write.as_str(), read.as_str()];

    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut c = cmd();
        c.args(["--server", &addr])
            .args(args)
            .envs(env.iter().copied());
        let o = c.output().unwrap();
        assert_no_token(&o, &all);
        o
    };
    let src_s = src.to_str().unwrap();
    let index = ["index", "--org", "o", "--repo", "r", src_s];

    // No token: exit 8 with a hint.
    let o = run(&["describe"], &[]);
    assert_eq!(o.status.code(), Some(8), "{}", text(&o));
    assert!(text(&o).contains("--token-file"), "{}", text(&o));
    // A token the server does not know: 8.
    let o = run(
        &["--token-file", wrong_file.to_str().unwrap(), "describe"],
        &[],
    );
    assert_eq!(o.status.code(), Some(8), "{}", text(&o));
    // A read token writing: 9.
    let mut a = vec!["--token-file", read_file.to_str().unwrap()];
    a.extend_from_slice(&index);
    let o = run(&a, &[]);
    assert_eq!(o.status.code(), Some(9), "{}", text(&o));
    // A write token from MEMORY_GRAPH_TOKEN_FILE indexes; a read token from
    // MEMORY_GRAPH_TOKEN reads.
    let o = run(
        &index,
        &[("MEMORY_GRAPH_TOKEN_FILE", write_file.to_str().unwrap())],
    );
    assert!(o.status.success(), "{}", text(&o));
    let o = run(&["describe", "--json"], &[("MEMORY_GRAPH_TOKEN", &read)]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("\"o\""), "{}", text(&o));
    // --token-file beats the environment.
    let o = run(
        &["--token-file", read_file.to_str().unwrap(), "describe"],
        &[("MEMORY_GRAPH_TOKEN", "garbage")],
    );
    assert!(o.status.success(), "{}", text(&o));
    // A malformed MEMORY_GRAPH_TOKEN is refused without echoing it.
    let o = run(
        &["describe"],
        &[("MEMORY_GRAPH_TOKEN", "mgt_not-a-real-token-value")],
    );
    assert!(!o.status.success());
    assert!(!text(&o).contains("not-a-real-token-value"), "{}", text(&o));
    // `mcp` over stdio uses the same credentials: refused without, serves with.
    let o = run(&["mcp"], &[]);
    assert_eq!(o.status.code(), Some(8), "{}", text(&o));
    let mut child = cmd()
        .args([
            "--server",
            &addr,
            "--token-file",
            read_file.to_str().unwrap(),
            "mcp",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0],
                           "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})
        )
        .unwrap();
    }
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("\"result\""),
        "{}",
        text(&o)
    );

    let _ = s.child.kill();
    let _ = s.child.wait();
}

#[test]
fn https_servers_are_refused() {
    let o = cmd()
        .args(["--server", "https://example.invalid:443", "describe"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(text(&o).contains("TLS"), "{}", text(&o));
}

#[test]
fn auth_needs_loopback_unless_insecure_transport() {
    let d = tempfile::tempdir().unwrap();
    let tokens = d.path().join("tokens.toml");
    new_token(&tokens, "ci", "write");
    let serve = |listen: &str, extra: &[&str]| {
        let mut c = cmd();
        c.args(["serve", "--db"])
            .arg(d.path().join(format!("g{}.redb", extra.len())))
            .arg("--auth-tokens")
            .arg(&tokens)
            .args(["--listen", listen])
            .args(extra);
        c
    };
    let o = serve("0.0.0.0:0", &[]).output().unwrap();
    assert!(!o.status.success());
    assert!(text(&o).contains("--insecure-transport"), "{}", text(&o));
    let o = serve("127.0.0.1:0", &["--metrics-listen", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(text(&o).contains("--metrics-listen"), "{}", text(&o));
    // A wildcard bind starts with the flag (the listening line is enough:
    // the readiness probe would dial the wildcard address).
    let mut child = serve("0.0.0.0:0", &["--insecure-transport"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if line.contains("listening on") {
                let _ = tx.send(line);
                return;
            }
        }
    });
    let line = rx.recv_timeout(std::time::Duration::from_secs(120));
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.is_ok(), "--insecure-transport did not start");
}
