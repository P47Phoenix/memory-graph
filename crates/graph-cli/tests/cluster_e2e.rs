//! Three real `memory-graph serve --data-dir` processes form a cluster
//! through the CLI (ADR 0004 stages B and C, epic stories 21 and 22):
//!
//! * node 1 `--bootstrap`, nodes 2 and 3 `--join <node 1> --standby`
//!   (learners), made voters with `cluster add-learner` (idempotent for a
//!   learner already there) and `cluster promote`;
//! * the vendored corpus indexed through node 1 with `--server`, and node 3
//!   (a follower, local reads) answering byte for byte what an embedded run
//!   answers;
//! * the leader stopped with `Admin.Shutdown`: a survivor reports a new
//!   leader through `cluster leader`, a write through it succeeds, and the
//!   old leader restarted from its data directory (no flags) catches up and
//!   answers the same;
//! * on unix, the new leader stopped with SIGTERM: it exits cleanly and the
//!   last two nodes elect a leader again.
//!
//! Every wait polls a condition with a hard deadline and says what it was
//! waiting for; nothing sleeps and hopes.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

/// Hard limit for every wait (an election, a catch-up, a process exit).
const WAIT: Duration = Duration::from_secs(90);

/// Raft timing for the test cluster: short enough that an election after
/// the leader stops takes about a second, long enough that a busy CI
/// machine does not call spurious ones (they would be harmless anyway).
const RAFT_TIMING: [&str; 6] = [
    "--heartbeat-interval",
    "100",
    "--election-timeout-min",
    "500",
    "--election-timeout-max",
    "1000",
];

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

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

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_WRITE_DEADLINE")
        .env_remove("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn run(args: &[&str]) -> Output {
    cmd().args(args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn ok(args: &[&str]) -> String {
    let o = run(args);
    assert!(
        o.status.success(),
        "{args:?} failed ({:?}):\n{}{}",
        o.status.code(),
        text(&o.stdout),
        text(&o.stderr)
    );
    text(&o.stdout)
}

/// Drop what differs between two runs of the same work: elapsed times.
fn normalize(s: &str) -> String {
    let mut s = s.to_string();
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

/// Poll `probe` until it returns `Some`, failing with `what` after [`WAIT`].
fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {WAIT:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One `memory-graph serve --data-dir` process.
struct Node {
    id: u64,
    dir: PathBuf,
    child: Option<Child>,
    addr: String,
}

impl Node {
    /// Start node `id` on `listen` with `extra` flags; returns once it
    /// printed its listening line.
    fn start(id: u64, dir: &Path, listen: &str, extra: &[&str]) -> Node {
        let mut c = cmd();
        c.arg("serve")
            .arg("--data-dir")
            .arg(dir)
            .args(["--node-id", &id.to_string(), "--listen", listen])
            .args(RAFT_TIMING)
            // The test machine's free space is not what is being tested.
            .args(["--min-free-disk", "1"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = c.spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Keep draining so the server never blocks on a print.
            for line in BufReader::new(out).lines() {
                let Ok(l) = line else { return };
                let _ = tx.send(l);
            }
        });
        let line = match rx.recv_timeout(WAIT) {
            Ok(l) => l,
            Err(e) => {
                let st = child.try_wait();
                let _ = child.kill();
                panic!("node {id} printed no listening line ({e}); exit: {st:?}");
            }
        };
        let addr = line
            .split("listening on ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        Node {
            id,
            dir: dir.to_path_buf(),
            child: Some(child),
            addr,
        }
    }

    /// Restart from the data directory with no init flag (a plain
    /// restart), on the same address: the advertised address is part of
    /// the node's identity (`node.json`) and the peers' membership.
    fn restart(&mut self, extra: &[&str]) {
        assert!(self.child.is_none(), "node {} is still running", self.id);
        // The port was just released; a platform may hold it briefly.
        let listen = self.addr.clone();
        let deadline = Instant::now() + WAIT;
        loop {
            let mut c = cmd();
            let mut child = c
                .arg("serve")
                .arg("--data-dir")
                .arg(&self.dir)
                .args(["--listen", &listen])
                .args(RAFT_TIMING)
                .args(["--min-free-disk", "1"])
                .args(extra)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let out = child.stdout.take().unwrap();
            let mut lines = BufReader::new(out).lines();
            match lines.next() {
                Some(Ok(l)) if l.contains("listening on") => {
                    std::thread::spawn(move || lines.for_each(drop));
                    let err = child.stderr.take().unwrap();
                    std::thread::spawn(move || {
                        for l in BufReader::new(err).lines().map_while(Result::ok) {
                            eprintln!("{l}");
                        }
                    });
                    self.child = Some(child);
                    return;
                }
                _ => {
                    let o = child.wait_with_output().unwrap();
                    let err = text(&o.stderr);
                    assert!(
                        Instant::now() < deadline
                            && (err.contains("in use") || err.contains("10048")),
                        "node {} did not restart on {listen}: {err}",
                        self.id
                    );
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// `Admin.Shutdown`, then wait for a clean exit.
    fn shutdown(&mut self) {
        let s = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&self.addr))
            .expect("connect for shutdown");
        s.admin_shutdown(Duration::from_secs(10))
            .expect("Admin.Shutdown");
        drop(s);
        self.wait_exit();
    }

    fn wait_exit(&mut self) {
        let mut child = self.child.take().expect("running");
        let id = self.id;
        let st = wait_for(&format!("node {id} to exit"), || child.try_wait().unwrap());
        assert!(st.success(), "node {id} exited with {st:?}");
    }

    fn server(&self) -> [&str; 2] {
        ["--server", &self.addr]
    }

    /// `cluster status --json`, or `None` while the node does not answer.
    fn status(&self) -> Option<serde_json::Value> {
        let o = run(&["--server", &self.addr, "cluster", "status", "--json"]);
        if !o.status.success() {
            return None;
        }
        serde_json::from_slice(&o.stdout).ok()
    }

    fn applied(&self) -> u64 {
        self.status()
            .and_then(|s| s["applied_index"].as_u64())
            .unwrap_or(0)
    }

    /// `cluster leader` on this node: `Some(id)` once one is known.
    fn leader(&self) -> Option<u64> {
        let o = run(&["--server", &self.addr, "cluster", "leader", "--json"]);
        if !o.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).ok()?;
        v["leader_id"].as_u64()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// The queries compared between an embedded run and a follower.
fn queries() -> Vec<Vec<&'static str>> {
    vec![
        vec!["describe"],
        vec!["describe", "--json"],
        vec!["search", "Subscribe", "--json"],
        vec!["search", "return", "--grain", "method", "--limit", "50"],
        vec!["search", "string", "--grain", "file", "--json"],
        vec!["symbols", "*", "--limit", "300"],
        vec!["symbols", "Get*", "--json"],
        vec!["export"],
    ]
}

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
            let mut a: Vec<&str> = target.to_vec();
            a.extend([
                "index",
                "--org",
                "corpus",
                "--repo",
                repo,
                "--no-progress",
                dir.to_str().unwrap(),
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

/// Wait until `node` has applied at least `index`.
fn wait_applied(node: &Node, index: u64) {
    wait_for(
        &format!("node {} to apply log index {index}", node.id),
        || (node.applied() >= index).then_some(()),
    );
}

/// The leader's committed index right now.
fn committed(leader: &Node) -> u64 {
    leader.status().expect("leader status")["committed_index"]
        .as_u64()
        .unwrap()
}

#[test]
fn three_processes_form_replicate_fail_over_and_catch_up() {
    let t0 = Instant::now();
    let d = tempfile::tempdir().unwrap();
    let local = d.path().join("local.redb");
    let local_s = local.to_str().unwrap();
    let local_index = index_corpus(&["--db", local_s]);
    let local_answers = answers(&["--db", local_s]);
    assert!(
        local_answers[0].contains("corpus/anyhow"),
        "{}",
        local_answers[0]
    );

    let dir = |i: u64| d.path().join(format!("n{i}"));
    let mut n1 = Node::start(1, &dir(1), "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to elect itself", || {
        (n1.leader() == Some(1)).then_some(())
    });
    let st = n1.status().unwrap();
    let cluster_id = st["cluster_id"].as_str().unwrap().to_string();
    assert!(!cluster_id.is_empty(), "{st}");
    assert_eq!(st["role"], "leader", "{st}");
    let n2 = Node::start(
        2,
        &dir(2),
        "127.0.0.1:0",
        &["--join", &n1.addr, "--standby"],
    );
    let n3 = Node::start(
        3,
        &dir(3),
        "127.0.0.1:0",
        &["--join", &n1.addr, "--standby"],
    );
    // A joined standby is a learner of the cluster.
    assert_eq!(
        n2.status().unwrap()["cluster_id"].as_str(),
        Some(cluster_id.as_str())
    );

    // Make them voters through the membership commands.
    for n in [&n2, &n3] {
        let id = n.id.to_string();
        let added = ok(&["--server", &n1.addr, "cluster", "add-learner", &id, &n.addr]);
        assert!(added.contains("added as a learner"), "{added}");
        let promoted = ok(&["--server", &n1.addr, "cluster", "promote", &id]);
        assert!(promoted.contains("is now a voter"), "{promoted}");
    }
    let st = n1.status().unwrap();
    let voters: Vec<u64> = st["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "voter")
        .map(|m| m["node_id"].as_u64().unwrap())
        .collect();
    assert_eq!(voters, [1, 2, 3], "{st}");
    // Text status names every member and, on the leader, their lag.
    let text_status = ok(&["--server", &n1.addr, "cluster", "status"]);
    for n in [&n1, &n2, &n3] {
        assert!(text_status.contains(&n.addr), "{text_status}");
    }
    assert!(text_status.contains("lag"), "{text_status}");

    // Index through node 1, read from node 3 (a follower, local reads).
    let remote_index = index_corpus(&n1.server());
    assert_same(
        "index output (embedded vs node 1)",
        &local_index,
        &remote_index,
    );
    let target = committed(&n1);
    wait_applied(&n3, target);
    let st3 = n3.status().unwrap();
    assert_eq!(st3["role"], "follower", "{st3}");
    assert_eq!(
        st3["cluster_id"].as_str(),
        Some(cluster_id.as_str()),
        "{st3}"
    );
    assert_same(
        "queries (embedded vs follower node 3)",
        &local_answers,
        &answers(&n3.server()),
    );
    eprintln!("cluster formed and corpus replicated at {:?}", t0.elapsed());

    // Stop the leader; a survivor names a new one.
    n1.shutdown();
    let new_leader = wait_for("a new leader among nodes 2 and 3", || {
        n2.leader().filter(|&l| l == 2 || l == 3)
    });
    assert_eq!(
        wait_for("node 3 to agree on the leader", || n3.leader()),
        new_leader
    );
    let leader_out = ok(&["--server", &n3.addr, "cluster", "leader"]);
    let (l, other) = if new_leader == 2 {
        (&n2, &n3)
    } else {
        (&n3, &n2)
    };
    assert!(
        leader_out.starts_with(&format!("{new_leader} {}", l.addr)),
        "{leader_out}"
    );

    // Write through the new leader, then read it from the other survivor.
    let extra = d.path().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(
        extra.join("after_failover.rs"),
        "pub fn written_after_failover() -> u32 { 7 }\n",
    )
    .unwrap();
    let extra_s = extra.to_str().unwrap();
    let index_extra = |target: &[&str]| {
        let mut a = target.to_vec();
        a.extend([
            "index",
            "--org",
            "corpus",
            "--repo",
            "extra",
            "--no-progress",
            extra_s,
        ]);
        normalize(&ok(&a))
    };
    assert_eq!(index_extra(&l.server()), index_extra(&["--db", local_s]));
    let target = committed(l);
    wait_applied(other, target);
    let hit = ok(&["--server", &other.addr, "search", "written_after_failover"]);
    assert!(hit.contains("after_failover.rs"), "{hit}");

    // The old leader restarts (no flags) and catches up.
    n1.restart(&[]);
    wait_applied(&n1, target);
    let st1 = wait_for("node 1 to rejoin as a follower", || {
        n1.status().filter(|s| s["role"] == "follower")
    });
    assert_eq!(
        st1["cluster_id"].as_str(),
        Some(cluster_id.as_str()),
        "{st1}"
    );
    let local_answers = answers(&["--db", local_s]);
    assert_same(
        "queries (embedded vs restarted node 1)",
        &local_answers,
        &answers(&n1.server()),
    );
    // Node 3 already matched in full; node 2 by its catalog (kept short:
    // every query is a process start).
    assert_eq!(
        normalize(&ok(&["--server", &n2.addr, "describe", "--json"])),
        local_answers[1],
        "describe --json (embedded vs node 2)"
    );

    // unix: SIGTERM the current leader; it exits cleanly and the two
    // remaining nodes still form a majority and elect a leader.
    #[cfg(unix)]
    {
        let (mut stopped, rest) = if new_leader == 2 {
            (n2, [&n1, &n3])
        } else {
            (n3, [&n1, &n2])
        };
        let pid = stopped.child.as_ref().unwrap().id().to_string();
        let st = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(st.success(), "kill -TERM {pid}");
        stopped.wait_exit();
        let again = wait_for("a leader after SIGTERM", || {
            rest[0].leader().filter(|&l| l != stopped.id)
        });
        assert!(again == 1 || rest.iter().any(|n| n.id == again), "{again}");
    }
    eprintln!("cluster e2e done in {:?}", t0.elapsed());
}

/// `serve --data-dir` refusals that need no cluster: an empty directory
/// without `--bootstrap`, `--restore` without `--bootstrap`, a different
/// `--node-id` on restart.
#[test]
fn data_dir_refusals() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("n");
    let o = run(&[
        "serve",
        "--data-dir",
        dir.to_str().unwrap(),
        "--node-id",
        "1",
        "--listen",
        "127.0.0.1:0",
    ]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(
        err.contains("not initialized") && err.contains("--bootstrap"),
        "{err}"
    );

    let o = run(&["serve", "--data-dir", "x", "--restore", "snap.redb"]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("--bootstrap"),
        "{}",
        text(&o.stderr)
    );

    // --bootstrap names what to do with a data directory: without one it
    // is refused, not ignored.
    let o = run(&["--db", "a.redb", "serve", "--bootstrap"]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("--data-dir"),
        "{}",
        text(&o.stderr)
    );

    let o = run(&["--db", "a.redb", "serve", "--data-dir", "x"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("exclusive"), "{}", text(&o.stderr));

    // Bootstrap once, stop, then a different --node-id is refused and the
    // same one (or none) restarts; --bootstrap again is a plain restart.
    let mut n = Node::start(1, &dir, "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to elect itself", || {
        (n.leader() == Some(1)).then_some(())
    });
    let id = n.status().unwrap()["cluster_id"]
        .as_str()
        .unwrap()
        .to_string();
    n.shutdown();
    let o = run(&[
        "serve",
        "--data-dir",
        dir.to_str().unwrap(),
        "--node-id",
        "2",
        "--listen",
        &n.addr,
    ]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(
        err.contains("wrong node")
            && err.contains("belongs to node 1")
            && err.contains("--node-id 2"),
        "{err}"
    );
    n.restart(&["--bootstrap"]);
    wait_for("the restarted node to lead", || {
        (n.leader() == Some(1)).then_some(())
    });
    assert_eq!(
        n.status().unwrap()["cluster_id"].as_str(),
        Some(id.as_str()),
        "--bootstrap on an initialized directory keeps the cluster"
    );
    n.shutdown();
}

/// The voters `cluster members --json` on `n` lists (empty while it does
/// not answer).
fn voters(n: &Node) -> Vec<u64> {
    let o = run(&["--server", &n.addr, "cluster", "members", "--json"]);
    if !o.status.success() {
        return Vec::new();
    }
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
    v["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m["role"] == "voter")
                .filter_map(|m| m["node_id"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// Run the CLI and wait at most [`WAIT`] for it to exit (a `serve` that
/// should have been refused must not hang the test); kills only the
/// process it spawned.
fn run_bounded(args: &[&str]) -> Output {
    let mut child = cmd()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let o = child.wait_with_output().unwrap();
            panic!(
                "{args:?} did not exit within {WAIT:?}:\n{}{}",
                text(&o.stdout),
                text(&o.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stage C through the CLI (epic story 22): nodes 2 and 3 `--join
/// --auto-promote` and become voters; `cluster members` from any node;
/// an index sent to a follower is forwarded and readable on the third
/// node; `cluster remove` refuses the leader and 3 -> 2 without `--force`
/// (exit 1, the reason on stderr); `cluster transfer-leader` moves
/// leadership; `--force` removes; a data directory of another cluster
/// joining exits with code 6.
#[test]
fn join_forward_remove_transfer_and_wrong_cluster() {
    let t0 = Instant::now();
    let d = tempfile::tempdir().unwrap();
    let dir = |i: u64| d.path().join(format!("n{i}"));
    let n1 = Node::start(1, &dir(1), "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to elect itself", || {
        (n1.leader() == Some(1)).then_some(())
    });
    let join = ["--join", n1.addr.as_str(), "--auto-promote"];
    let n2 = Node::start(2, &dir(2), "127.0.0.1:0", &join);
    let n3 = Node::start(3, &dir(3), "127.0.0.1:0", &join);
    wait_for("nodes 2 and 3 to be auto-promoted", || {
        (voters(&n1) == [1, 2, 3]).then_some(())
    });
    let members = ok(&["--server", &n3.addr, "cluster", "members"]);
    for want in ["1 voter", "2 voter", "3 voter", "(leader)"] {
        assert!(members.contains(want), "{want}: {members}");
    }
    eprintln!("joined and promoted at {:?}", t0.elapsed());

    // Index through node 2 (a follower): forwarded to node 1.
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn forwarded_marker() -> u8 { 1 }\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn second() {}\n").unwrap();
    let index = |target: &[&str]| {
        let mut a = target.to_vec();
        a.extend([
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--no-progress",
            src.to_str().unwrap(),
        ]);
        normalize(&ok(&a))
    };
    let local = d.path().join("local.redb");
    assert_eq!(
        index(&n2.server()),
        index(&["--db", local.to_str().unwrap()])
    );
    let st2 = n2.status().unwrap();
    assert!(
        st2["writes_forwarded_total"].as_u64().unwrap() >= 1,
        "{st2}"
    );
    wait_applied(&n3, committed(&n1));
    let hit = ok(&["--server", &n3.addr, "search", "forwarded_marker"]);
    assert!(hit.contains("a.rs"), "{hit}");
    // `index --json` says whether the connected node forwarded the writes.
    for (i, (node, forwarded)) in [(&n2, true), (&n1, false)].into_iter().enumerate() {
        std::fs::write(
            src.join(format!("json{i}.rs")),
            format!("pub fn j{i}() {{}}\n"),
        )
        .unwrap();
        let mut a = node.server().to_vec();
        a.extend([
            "index",
            "--org",
            "o",
            "--repo",
            "r",
            "--no-progress",
            "--json",
            src.to_str().unwrap(),
        ]);
        let v: serde_json::Value = serde_json::from_str(&ok(&a)).unwrap();
        assert_eq!(
            v["forwarded_to_leader"], forwarded,
            "node {}: {v}",
            node.addr
        );
    }

    // Remove guards: refused with the reason, exit code 1.
    let o = run(&["--server", &n2.addr, "cluster", "remove", "1"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("transfer leadership first"),
        "{}",
        text(&o.stderr)
    );
    let o = run(&["--server", &n3.addr, "cluster", "remove", "3"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--force"), "{}", text(&o.stderr));
    assert_eq!(voters(&n1), [1, 2, 3]);

    // Transfer leadership to node 2 (asked through node 3), then remove
    // node 3 with --force.
    let moved = ok(&["--server", &n3.addr, "cluster", "transfer-leader", "2"]);
    assert!(moved.contains("node 2 is now the leader"), "{moved}");
    wait_for("node 1 to follow node 2", || {
        (n1.leader() == Some(2)).then_some(())
    });
    let removed = ok(&["--server", &n1.addr, "cluster", "remove", "3", "--force"]);
    assert!(removed.contains("was removed"), "{removed}");
    wait_for("two voters", || (voters(&n2) == [1, 2]).then_some(()));
    eprintln!("guards, transfer and remove done at {:?}", t0.elapsed());

    // Another cluster's data directory joining: exit code 6.
    let mut other = Node::start(4, &dir(4), "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 4 to elect itself", || {
        (other.leader() == Some(4)).then_some(())
    });
    other.shutdown();
    let dir4 = dir(4);
    let mut args = vec![
        "serve",
        "--data-dir",
        dir4.to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
        "--min-free-disk",
        "1",
        "--join",
        &n1.addr,
        "--auto-promote",
    ];
    args.extend(RAFT_TIMING);
    let o = run_bounded(&args);
    assert_eq!(o.status.code(), Some(6), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("wrong cluster"),
        "{}",
        text(&o.stderr)
    );
    drop((n2, n3));
    eprintln!("cluster stage C e2e done in {:?}", t0.elapsed());
}

/// The numbers in `docs/spikes/raft-replication.md`. Not a gate (timings
/// depend on the machine): run it with
/// `cargo test --release -p graph-cli --test cluster_e2e measure_replication -- --ignored --nocapture`.
#[test]
#[ignore = "measurement for docs/spikes/raft-replication.md; run with --release --ignored"]
fn measure_replication() {
    use graph_store::Store;
    use redb::ReadableTable;
    let d = tempfile::tempdir().unwrap();
    let source: u64 = {
        fn walk(p: &Path) -> u64 {
            std::fs::read_dir(p)
                .unwrap()
                .flatten()
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        if e.file_name() == ".git" {
                            0
                        } else {
                            walk(&p)
                        }
                    } else {
                        p.metadata().unwrap().len()
                    }
                })
                .sum()
        }
        walk(&corpus())
    };
    println!("corpus source bytes (all files): {source}");
    let secs = |t: Instant| t.elapsed().as_secs_f64();

    // Embedded ingest (best of 3; the first also warms the file cache).
    let mut embedded = f64::MAX;
    for i in 0..3 {
        let db = d.path().join(format!("embedded{i}.redb"));
        let t = Instant::now();
        index_corpus(&["--db", db.to_str().unwrap()]);
        embedded = embedded.min(secs(t));
    }
    println!("embedded corpus ingest: {embedded:.3} s (best of 3)");

    // One node: ingest, then the log's contents and size.
    let one = d.path().join("one");
    let mut n = Node::start(
        1,
        &one,
        "127.0.0.1:0",
        &["--bootstrap", "--log-keep-entries", "0"],
    );
    wait_for("node 1 to lead", || (n.leader() == Some(1)).then_some(()));
    let t = Instant::now();
    index_corpus(&n.server());
    let single = secs(t);
    println!(
        "1-node corpus ingest via --server: {single:.3} s ({:.0}% of embedded speed)",
        100.0 * embedded / single
    );
    // Snapshot build (through the RPC, without download).
    let t = Instant::now();
    let snap = ok(&["--server", &n.addr, "cluster", "snapshot", "--json"]);
    println!("snapshot build: {:.3} s ({snap})", secs(t));
    let st = n.status().unwrap();
    println!(
        "after snapshot+purge: log_bytes={} store_bytes={}",
        st["log_bytes"], st["store_bytes"]
    );
    // Install: a new member after the purge catches up by snapshot only.
    let two = d.path().join("two");
    let target = n.applied();
    let t = Instant::now();
    let n2 = Node::start(2, &two, "127.0.0.1:0", &["--join", &n.addr, "--standby"]);
    let added = secs(t);
    wait_applied(&n2, target);
    let st2 = n2.status().unwrap();
    println!(
        "snapshot transfer + install on a new learner: joined after {added:.3} s, \
         applied {target} after {:.3} s (learner snapshot_index={}, store_bytes={})",
        secs(t),
        st2["snapshot_index"],
        st2["store_bytes"]
    );
    drop(n2);
    n.shutdown();

    // Log contents: a fresh node, ingest, stop, read raft.redb directly.
    let raw = d.path().join("raw");
    let mut n = Node::start(1, &raw, "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to lead", || (n.leader() == Some(1)).then_some(()));
    index_corpus(&n.server());
    n.shutdown();
    let log = raw.join("raft.redb");
    let file = std::fs::metadata(&log).unwrap().len();
    let db = redb::Database::open(&log).unwrap();
    let rt = db.begin_read().unwrap();
    let t: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("raft_log");
    let table = rt.open_table(t).unwrap();
    let (mut entries, mut normal, mut bytes) = (0u64, 0u64, 0u64);
    for row in table.iter().unwrap() {
        let (_, v) = row.unwrap();
        entries += 1;
        if v.value()[0] == 1 {
            normal += 1;
        }
        bytes += v.value().len() as u64;
    }
    println!(
        "log after corpus ingest: {entries} entries ({normal} writes), {bytes} B encoded \
         ({} B framing = 25 B/entry, {} B payload, {:.2}x source), raft.redb {file} B ({:.2}x source)",
        25 * entries,
        bytes - 25 * entries,
        (bytes - 25 * entries) as f64 / source as f64,
        file as f64 / source as f64
    );
    drop(table);
    drop(rt);
    drop(db);

    // Three nodes: ingest through the leader.
    let dirs: Vec<PathBuf> = (1..=3).map(|i| d.path().join(format!("c{i}"))).collect();
    let n1 = Node::start(1, &dirs[0], "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to lead", || (n1.leader() == Some(1)).then_some(()));
    let join = ["--join", n1.addr.as_str(), "--auto-promote"];
    let n2 = Node::start(2, &dirs[1], "127.0.0.1:0", &join);
    let n3 = Node::start(3, &dirs[2], "127.0.0.1:0", &join);
    wait_for("three voters", || (voters(&n1).len() == 3).then_some(()));
    let _ = (&n2, &n3);
    let t = Instant::now();
    index_corpus(&n1.server());
    let triple = secs(t);
    println!(
        "3-node corpus ingest via the leader: {triple:.3} s ({:.0}% of embedded speed; ADR trigger < 50%)",
        100.0 * embedded / triple
    );

    // fsync throughput: one log entry per call, small and large payloads.
    let throughput = |addr: &str, label: &str| {
        let s = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(addr)).unwrap();
        let small = b"x".repeat(1024);
        let n = 300;
        let t = Instant::now();
        for i in 0..n {
            s.index_bytes("m", label, &format!("s{i}.txt"), &small, Some("text"))
                .unwrap();
        }
        let e = secs(t);
        println!("{label}: {n} x 1 KiB writes: {:.0} entries/s", n as f64 / e);
        // One 1 MiB token: the store's per-token work stays small, so this
        // measures the log (replication + fsync), not the tokenizer.
        let big = b"y".repeat(1 << 20);
        let n = 20;
        let t = Instant::now();
        for i in 0..n {
            s.index_bytes("m", label, &format!("b{i}.txt"), &big, Some("text"))
                .unwrap();
        }
        let e = secs(t);
        println!(
            "{label}: {n} x 1 MiB writes: {:.1} entries/s, {:.1} MB/s",
            n as f64 / e,
            (n as f64 * big.len() as f64) / e / 1e6
        );
    };
    throughput(&n1.addr, "three-nodes");
    let mut n = Node::start(1, &d.path().join("tp"), "127.0.0.1:0", &["--bootstrap"]);
    wait_for("node 1 to lead", || (n.leader() == Some(1)).then_some(()));
    throughput(&n.addr, "one-node");
    n.shutdown();
}
