//! Three real `memory-graph serve --data-dir` processes form a cluster
//! through the CLI (ADR 0004 stage B, epic story 21):
//!
//! * node 1 `--bootstrap`, nodes 2 and 3 `--wait-for-membership` (stage B's
//!   stand-in for stage C's `--join`), joined with the preview `cluster
//!   add-learner` / `cluster promote` commands;
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
    let n2 = Node::start(2, &dir(2), "127.0.0.1:0", &["--wait-for-membership"]);
    let n3 = Node::start(3, &dir(3), "127.0.0.1:0", &["--wait-for-membership"]);
    wait_for("node 1 to elect itself", || {
        (n1.leader() == Some(1)).then_some(())
    });
    let st = n1.status().unwrap();
    let cluster_id = st["cluster_id"].as_str().unwrap().to_string();
    assert!(!cluster_id.is_empty(), "{st}");
    assert_eq!(st["role"], "leader", "{st}");
    // An uninitialized member has no leader and no cluster id yet.
    assert_eq!(n2.leader(), None);

    // Form the cluster through the preview membership commands.
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
    assert_same(
        "queries (embedded vs node 2)",
        &local_answers,
        &answers(&n2.server()),
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
    assert!(err.contains('1') && err.contains('2'), "{err}");
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
