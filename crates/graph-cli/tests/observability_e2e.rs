//! Stage E (ADR 0004 D10, epic story 24) against the real `serve` binary:
//!
//! * `--log-format json`: every stderr line is one JSON object with
//!   `timestamp`, `level`, `target`, `message`; RPCs and applies log inside
//!   `rpc` / `apply` spans with their fields; stdout carries only the start
//!   lines (`metrics on ...`, `listening on ...`).
//! * `--metrics-listen`: `/metrics` parsed with a small hand-written
//!   Prometheus text (0.0.4) parser: every metric name present with sane
//!   values after a write, histogram buckets monotonic; `Admin.Metrics`
//!   serves the same families; unknown paths are 404.
//! * `health` / `health --ready` exit codes 0 and 1.
//! * `--node-id-from-hostname` and `--bootstrap-or-join` (StatefulSet
//!   identity): ordinal 0 bootstraps as node 1, ordinal 1 joins as node 2.
//!
//! Every wait has a hard deadline; only processes this test spawned are
//! stopped (Admin.Shutdown, or kill on drop).
mod common;

use common::readiness::{start_serve, StartOptions};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");
const WAIT: Duration = Duration::from_secs(60);

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_WRITE_DEADLINE")
        .env_remove("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER")
        .env_remove("MEMORY_GRAPH_LOG");
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

/// A `serve` this test started; its stdout and stderr are drained by
/// threads into channels.
struct Serve {
    child: Child,
    addr: String,
    metrics: Option<String>,
    stdout: Vec<String>,
    stderr: Receiver<String>,
}

impl Serve {
    fn start(args: &[&str], env: &[(&str, &str)]) -> Serve {
        let mut c = cmd();
        c.arg("serve").args(args).envs(env.iter().copied());
        let options = StartOptions {
            echo_stderr: false,
            ..StartOptions::default()
        };
        let started = start_serve(c, options);
        Serve {
            child: started.child,
            addr: started.addr,
            metrics: started.metrics.map(|m| m.to_string()),
            stdout: started.start_lines,
            stderr: started.stderr,
        }
    }

    /// Admin.Shutdown, wait for a clean exit, return every stderr line.
    fn stop(mut self) -> Vec<String> {
        ok(&["--server", &self.addr, "cluster", "status"]);
        let s = graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&self.addr))
            .expect("connect for shutdown");
        s.admin_shutdown(Duration::from_secs(10))
            .expect("Admin.Shutdown");
        drop(s);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                assert!(st.success(), "serve exited with {st:?}");
                break;
            }
            assert!(Instant::now() < deadline, "serve did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
        // The pipe is closed now: the drain thread ends, the channel with it.
        self.stderr.iter().collect()
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// One HTTP/1.1 GET; `(status code, body)`.
fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").expect("a response head");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    if code == 200 {
        assert!(
            head.contains("text/plain; version=0.0.4"),
            "content type: {head}"
        );
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
            .expect("a content length");
        assert_eq!(len, body.len(), "Content-Length matches the body");
    }
    (code, body.to_string())
}

/// One sample of the Prometheus text format: name, labels, value.
#[derive(Debug, Clone)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}

/// A small parser of the text exposition format 0.0.4: `# HELP` / `# TYPE`
/// lines (TYPE recorded per family), samples `name{l="v",...} value`,
/// escaped label values (`\\`, `\"`, `\n`). Panics on anything else.
fn parse_prometheus(text: &str) -> (BTreeMap<String, String>, Vec<Sample>) {
    let mut types = BTreeMap::new();
    let mut samples = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect("TYPE name kind");
            assert!(
                ["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind),
                "{line}"
            );
            assert!(
                types.insert(name.to_string(), kind.to_string()).is_none(),
                "TYPE twice: {line}"
            );
            continue;
        }
        if line.starts_with("# HELP ") || line.starts_with('#') {
            continue;
        }
        let name_end = line
            .find(['{', ' '])
            .unwrap_or_else(|| panic!("no value: {line}"));
        let name = &line[..name_end];
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
            "metric name {name:?}"
        );
        let mut rest = &line[name_end..];
        let mut labels = BTreeMap::new();
        if let Some(r) = rest.strip_prefix('{') {
            let (l, used) = parse_labels(r, line);
            labels = l;
            rest = &r[used..];
        }
        let value = rest.trim();
        let value: f64 = match value {
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            v => v
                .parse()
                .unwrap_or_else(|_| panic!("value {v:?} in {line}")),
        };
        samples.push(Sample {
            name: name.to_string(),
            labels,
            value,
        });
    }
    (types, samples)
}

/// The labels after the opening brace, up to and including the closing
/// one: `(labels, bytes used)`.
fn parse_labels(tail: &str, line: &str) -> (BTreeMap<String, String>, usize) {
    let mut labels = BTreeMap::new();
    let chars: Vec<char> = tail.chars().collect();
    let mut k = 0;
    loop {
        if chars[k] == '}' {
            k += 1;
            break;
        }
        let mut name = String::new();
        while chars[k] != '=' {
            name.push(chars[k]);
            k += 1;
        }
        assert_eq!(chars[k + 1], '"', "{line}");
        k += 2;
        let mut value = String::new();
        loop {
            match chars[k] {
                '\\' => {
                    value.push(if chars[k + 1] == 'n' {
                        '\n'
                    } else {
                        chars[k + 1]
                    });
                    k += 2;
                }
                '"' => {
                    k += 1;
                    break;
                }
                c => {
                    value.push(c);
                    k += 1;
                }
            }
        }
        labels.insert(name, value);
        match chars[k] {
            ',' => k += 1,
            '}' => {}
            c => panic!("unexpected {c:?} in {line}"),
        }
    }
    let used = chars[..k].iter().map(|c| c.len_utf8()).sum();
    (labels, used)
}

fn one(samples: &[Sample], name: &str) -> f64 {
    let v: Vec<&Sample> = samples.iter().filter(|s| s.name == name).collect();
    assert_eq!(v.len(), 1, "{name}: {v:?}");
    v[0].value
}

/// Every histogram series: buckets non-decreasing in `le`, `+Inf` equal to
/// `_count`, `_sum` >= 0.
/// A series' labels without `le`, sorted.
type Labels = Vec<(String, String)>;

fn check_histograms(types: &BTreeMap<String, String>, samples: &[Sample]) {
    for (family, _) in types.iter().filter(|(_, k)| *k == "histogram") {
        let mut series: BTreeMap<Labels, Vec<(f64, f64)>> = BTreeMap::new();
        for s in samples
            .iter()
            .filter(|s| s.name == format!("{family}_bucket"))
        {
            let le = s.labels["le"].as_str();
            let le: f64 = if le == "+Inf" {
                f64::INFINITY
            } else {
                le.parse().unwrap()
            };
            let key: Labels = s
                .labels
                .iter()
                .filter(|(k, _)| *k != "le")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            series.entry(key).or_default().push((le, s.value));
        }
        for (key, buckets) in &series {
            assert!(buckets.len() >= 2, "{family}{key:?}: buckets");
            assert!(
                buckets
                    .windows(2)
                    .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1),
                "{family}{key:?}: buckets not monotonic: {buckets:?}"
            );
            let labels: BTreeMap<String, String> = key.iter().cloned().collect();
            let count = samples
                .iter()
                .find(|s| s.name == format!("{family}_count") && s.labels == labels)
                .unwrap_or_else(|| panic!("{family}{key:?}: no _count"));
            let sum = samples
                .iter()
                .find(|s| s.name == format!("{family}_sum") && s.labels == labels)
                .unwrap_or_else(|| panic!("{family}{key:?}: no _sum"));
            assert_eq!(buckets.last().unwrap().1, count.value, "{family}: +Inf");
            assert!(sum.value >= 0.0);
            let first = buckets.first().unwrap().0;
            assert_eq!(first, 0.0005, "{family}: buckets start at 0.5 ms");
            assert_eq!(buckets[buckets.len() - 2].0, 10.0, "{family}: up to 10 s");
        }
    }
}

use graph_server::observe::METRIC_NAMES;

/// The sample of `name` whose `kind` label is `kind`.
fn by_kind(samples: &[Sample], name: &str, kind: &str) -> f64 {
    let v: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.name == name && s.labels.get("kind").map(String::as_str) == Some(kind))
        .collect();
    assert_eq!(v.len(), 1, "{name}{{kind={kind}}}: {v:?}");
    v[0].value
}

fn src_file(dir: &Path) -> String {
    let f = dir.join("lib.rs");
    std::fs::write(&f, "pub fn observed() -> u32 { 1 }\n").unwrap();
    f.to_str().unwrap().to_string()
}

#[test]
fn json_logs_metrics_and_health_exit_codes() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let s = Serve::start(
        &[
            "--db",
            db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--metrics-listen",
            "127.0.0.1:0",
            "--log-format",
            "json",
        ],
        // The level through the environment variable.
        &[("MEMORY_GRAPH_LOG", "debug")],
    );
    let metrics = s.metrics.clone().expect("a metrics line on stdout");
    let f = src_file(d.path());
    ok(&[
        "--server",
        &s.addr,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        &f,
    ]);
    assert!(ok(&["--server", &s.addr, "search", "observed"]).contains("observed"));

    // Health: 0 while serving, --ready too (a one-node leader).
    assert_eq!(run(&["--server", &s.addr, "health"]).status.code(), Some(0));
    assert_eq!(
        run(&["--server", &s.addr, "health", "--ready"])
            .status
            .code(),
        Some(0)
    );

    // /metrics.
    let (code, body) = http_get(&metrics, "/metrics");
    assert_eq!(code, 200, "{body}");
    let (types, samples) = parse_prometheus(&body);
    for name in METRIC_NAMES {
        assert!(types.contains_key(name), "TYPE of {name} missing:\n{body}");
    }
    for name in types.keys().filter(|n| n.starts_with("mg_")) {
        assert!(
            METRIC_NAMES.contains(&name.as_str()),
            "{name} is exported but not in METRIC_NAMES"
        );
    }
    assert_eq!(types["mg_rpc_total"], "counter");
    assert_eq!(types["mg_rpc_duration_seconds"], "histogram");
    assert_eq!(types["mg_apply_duration_seconds"], "histogram");
    assert!(one(&samples, "mg_raft_term") >= 1.0);
    assert_eq!(one(&samples, "mg_raft_leader_id"), 1.0);
    let roles: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.name == "mg_raft_role")
        .collect();
    assert_eq!(roles.iter().map(|s| s.value).sum::<f64>(), 1.0, "one role");
    assert!(roles
        .iter()
        .any(|s| s.labels["role"] == "leader" && s.value == 1.0));
    let applied = one(&samples, "mg_raft_applied_index");
    assert!(applied >= 1.0, "a write was applied");
    assert!(one(&samples, "mg_raft_committed_index") >= applied);
    assert!(one(&samples, "mg_raft_last_log_index") >= applied);
    assert!(one(&samples, "mg_store_bytes") > 0.0);
    assert!(one(&samples, "mg_log_bytes") > 0.0);
    assert_eq!(one(&samples, "mg_snapshot_handles_open"), 0.0);
    assert_eq!(one(&samples, "mg_writes_forwarded_total"), 0.0);
    assert!(one(&samples, "mg_apply_duration_seconds_count") >= 1.0);
    let build: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.name == "mg_build_info")
        .collect();
    assert_eq!(build.len(), 1);
    assert_eq!(build[0].value, 1.0);
    for l in ["version", "protocol", "store_format"] {
        assert!(!build[0].labels[l].is_empty(), "{l}");
    }
    let writes: f64 = samples
        .iter()
        .filter(|s| {
            s.name == "mg_rpc_total"
                && s.labels["rpc"].starts_with("Write/")
                && s.labels["outcome"] == "ok"
        })
        .map(|s| s.value)
        .sum();
    assert!(writes >= 1.0, "the write RPC is counted:\n{body}");
    assert!(
        samples
            .iter()
            .any(|s| s.name == "mg_rpc_total" && s.labels["rpc"] == "Health/Check"),
        "health checks are RPCs too"
    );
    check_histograms(&types, &samples);
    assert_eq!(http_get(&metrics, "/nope").0, 404);

    // cluster status --json carries the totals, and (idle: nothing is
    // written between the two reads) the same Raft numbers as /metrics.
    let st: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &s.addr, "cluster", "status", "--json"])).unwrap();
    let (_, now) = parse_prometheus(&http_get(&metrics, "/metrics").1);
    for (metric, key) in [
        ("mg_raft_term", "current_term"),
        ("mg_raft_leader_id", "leader_id"),
        ("mg_raft_last_log_index", "last_log_index"),
        ("mg_raft_committed_index", "committed_index"),
        ("mg_raft_applied_index", "applied_index"),
        ("mg_raft_snapshot_index", "snapshot_index"),
        ("mg_raft_purged_index", "purged_index"),
    ] {
        assert_eq!(
            one(&now, metric),
            st[key].as_u64().unwrap_or(0) as f64,
            "{metric} vs status {key}: {st}"
        );
    }
    let role = now
        .iter()
        .find(|s| s.name == "mg_raft_role" && s.value == 1.0)
        .map(|s| s.labels["role"].clone());
    assert_eq!(role.as_deref(), st["role"].as_str(), "{st}");
    assert!(
        !now.iter().any(|s| s.name == "mg_raft_replication_lag"),
        "a one-node cluster has no peers"
    );
    assert!(st["rpcs_total"].as_u64().unwrap() >= 1, "{st}");
    assert!(st["entries_applied_total"].as_u64().unwrap() >= 1, "{st}");
    assert_eq!(st["writes_forwarded_total"].as_u64(), Some(0));

    let stdout = s.stdout.clone();
    let addr = s.addr.clone();
    let stderr = s.stop();
    // Stopped: health answers 1 (unreachable), --ready too.
    assert_eq!(run(&["--server", &addr, "health"]).status.code(), Some(1));
    assert_eq!(
        run(&["--server", &addr, "health", "--ready"]).status.code(),
        Some(1)
    );

    // stdout: only the start lines, no logs, and JSON objects too (a
    // container runtime merges stdout into the log stream).
    assert_eq!(stdout.len(), 2, "{stdout:?}");
    for (l, event) in stdout.iter().zip(["metrics", "listening"]) {
        let v: serde_json::Value =
            serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout not JSON ({e}): {l}"));
        assert_eq!(v["event"], event, "{l}");
        for k in ["timestamp", "level", "target", "message", "addr"] {
            assert!(v.get(k).is_some(), "{k} missing: {l}");
        }
    }
    let v: serde_json::Value = serde_json::from_str(&stdout[1]).unwrap();
    assert_eq!(v["addr"], addr.as_str());
    let v: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
    assert_eq!(v["addr"], metrics.as_str());
    // stderr: JSON lines only, each with the four keys.
    assert!(!stderr.is_empty(), "logs were written");
    let mut rpc_span = false;
    let mut apply_span = false;
    for l in &stderr {
        let v: serde_json::Value =
            serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"));
        for k in ["timestamp", "level", "target", "message"] {
            assert!(v.get(k).is_some(), "{k} missing: {l}");
        }
        let span = &v["span"];
        if span["name"] == "rpc" && v["message"] == "rpc finished" {
            for k in ["method", "peer", "outcome", "duration_ms"] {
                assert!(span.get(k).is_some(), "rpc span {k} missing: {l}");
            }
            rpc_span = true;
        }
        if span["name"] == "apply" && span["kind"] == "index_chunk" {
            for k in ["index", "files", "duration_ms"] {
                assert!(span.get(k).is_some(), "apply span {k} missing: {l}");
            }
            apply_span = true;
        }
    }
    assert!(rpc_span, "an rpc span was logged");
    assert!(apply_span, "an apply span was logged");
}

#[test]
fn text_logs_and_a_bad_filter() {
    let d = tempfile::tempdir().unwrap();
    let o = run(&[
        "serve",
        "--db",
        d.path().join("g.redb").to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
        "--log-level",
        "info,[[[",
    ]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("--log-level"),
        "{}",
        text(&o.stderr)
    );
    // Text format: plain lines, not JSON.
    let s = Serve::start(
        &[
            "--db",
            d.path().join("g.redb").to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    assert!(s.metrics.is_none());
    assert_eq!(s.stdout.len(), 1);
    assert!(
        s.stdout[0].starts_with("memory-graph serve: listening on "),
        "text start line: {:?}",
        s.stdout
    );
    let stderr = s.stop();
    assert!(
        stderr
            .iter()
            .any(|l| l.contains("INFO") && l.contains("serving")),
        "{stderr:?}"
    );
    assert!(stderr.iter().all(|l| !l.starts_with('{')), "{stderr:?}");
}

#[test]
fn statefulset_identity_from_the_hostname() {
    let d = tempfile::tempdir().unwrap();
    let dir0 = d.path().join("pod0");
    let dir1 = d.path().join("pod1");
    // A host name without an ordinal is refused.
    let o = cmd()
        .env("HOSTNAME", "laptop")
        .args([
            "serve",
            "--data-dir",
            d.path().join("x").to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--node-id-from-hostname",
            "--bootstrap",
        ])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("laptop"), "{}", text(&o.stderr));

    // Ordinal 0 on an empty directory asks the other members (--peers)
    // whether a cluster exists; none answers within the probe timeout, so
    // it bootstraps as node 1.
    let p0 = Serve::start(
        &[
            "--data-dir",
            dir0.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--node-id-from-hostname",
            "--bootstrap-or-join",
            "127.0.0.1:1",
            "--peers",
            "127.0.0.1:1",
            "--bootstrap-probe-timeout",
            "1s",
            "--auto-promote",
        ],
        &[("HOSTNAME", "memory-graph-0")],
    );
    let st: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &p0.addr, "cluster", "status", "--json"])).unwrap();
    assert_eq!(st["node_id"], 1, "{st}");
    assert_eq!(st["role"], "leader", "{st}");
    // Ordinal 1: joins pod 0 as node 2 and is promoted.
    let p1 = Serve::start(
        &[
            "--data-dir",
            dir1.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--node-id-from-hostname",
            "--bootstrap-or-join",
            &p0.addr,
            "--auto-promote",
        ],
        &[("HOSTNAME", "memory-graph-1.memory-graph.default.svc")],
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let m: serde_json::Value =
            serde_json::from_str(&ok(&["--server", &p0.addr, "cluster", "members", "--json"]))
                .unwrap();
        let voters: Vec<u64> = m["members"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["role"] == "voter")
            .map(|x| x["node_id"].as_u64().unwrap())
            .collect();
        if voters == [1, 2] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "node 2 was not promoted within {WAIT:?}: {m}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let st: serde_json::Value =
        serde_json::from_str(&ok(&["--server", &p1.addr, "cluster", "status", "--json"])).unwrap();
    assert_eq!(st["node_id"], 2, "{st}");
    p1.stop();
    p0.stop();
}

/// ADR 0008 phase 0 (epic story 45): the read-path counters rise across a
/// fixed search served over the client, and `--read-timing` fills the
/// query seconds.
#[test]
fn read_counters_rise_across_a_search() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let s = Serve::start(
        &[
            "--db",
            db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--metrics-listen",
            "127.0.0.1:0",
            "--read-timing",
        ],
        &[],
    );
    let metrics = s.metrics.clone().expect("a metrics line on stdout");
    let f = src_file(d.path());
    ok(&[
        "--server",
        &s.addr,
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
        &f,
    ]);
    let scrape = || {
        let (code, body) = http_get(&metrics, "/metrics");
        assert_eq!(code, 200, "{body}");
        parse_prometheus(&body)
    };
    let (types, before) = scrape();
    for name in [
        "mg_read_decodes_total",
        "mg_read_decode_bytes_total",
        "mg_read_decode_seconds_total",
        "mg_read_queries_total",
        "mg_read_query_seconds_total",
        "mg_read_txns_total",
        "mg_read_dict_strings_total",
    ] {
        assert_eq!(types[name], "counter", "{name}");
    }
    assert!(ok(&["--server", &s.addr, "search", "observed"]).contains("observed"));
    let (_, after) = scrape();

    let rose = |name: &str| one(&after, name) - one(&before, name);
    assert!(rose("mg_read_queries_total") >= 1.0);
    assert!(rose("mg_read_txns_total") >= 1.0);
    assert!(rose("mg_read_query_seconds_total") > 0.0, "--read-timing");
    assert!(rose("mg_read_dict_strings_total") > 0.0);
    let dict = |samples: &[Sample], name: &str| by_kind(samples, name, "dict");
    assert!(dict(&after, "mg_read_decodes_total") > 0.0);
    assert!(
        dict(&after, "mg_read_decodes_total") > dict(&before, "mg_read_decodes_total"),
        "the search resolved term text"
    );
    assert!(
        dict(&after, "mg_read_decode_bytes_total") > dict(&before, "mg_read_decode_bytes_total")
    );
    for kind in ["dict", "symbol", "lazy", "full"] {
        for name in [
            "mg_read_decodes_total",
            "mg_read_decode_bytes_total",
            "mg_read_decode_seconds_total",
        ] {
            assert!(by_kind(&after, name, kind) >= by_kind(&before, name, kind));
        }
    }
    s.stop();
}

/// The sample of `name` whose `rpc` label is `rpc`.
fn by_rpc(samples: &[Sample], name: &str, rpc: &str) -> f64 {
    let v: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.name == name && s.labels.get("rpc").map(String::as_str) == Some(rpc))
        .collect();
    assert_eq!(v.len(), 1, "{name}{{rpc={rpc}}}: {v:?}");
    v[0].value
}

/// ADR 0008 phase 3 gate (epic story 49): the same search twice at an
/// unchanged applied index counts one exact repeat; a write in between
/// (the applied index advances) makes the next one a fresh query.
#[test]
fn an_identical_search_counts_one_repeat_until_a_write() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let s = Serve::start(
        &[
            "--db",
            db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--metrics-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let metrics = s.metrics.clone().expect("a metrics line on stdout");
    let index = |file: &str| {
        ok(&[
            "--server",
            &s.addr,
            "index-file",
            "--org",
            "o",
            "--repo",
            "r",
            file,
        ])
    };
    index(&src_file(d.path()));
    let scrape = || {
        let (code, body) = http_get(&metrics, "/metrics");
        assert_eq!(code, 200, "{body}");
        parse_prometheus(&body).1
    };
    let search = || assert!(ok(&["--server", &s.addr, "search", "observed"]).contains("observed"));
    let queries = |x: &[Sample]| by_rpc(x, "mg_queries_total", "Search");
    let repeats = |x: &[Sample]| by_rpc(x, "mg_query_exact_repeats_total", "Search");

    let before = scrape();
    search();
    let first = scrape();
    assert!(queries(&first) > queries(&before));
    assert_eq!(
        repeats(&first),
        repeats(&before),
        "a first query is no repeat"
    );
    search();
    let second = scrape();
    assert!(queries(&second) > queries(&first));
    assert_eq!(
        repeats(&second) - repeats(&first),
        1.0,
        "the same search again"
    );

    let g = d.path().join("other.rs");
    std::fs::write(&g, "pub fn observed_too() -> u32 { 2 }\n").unwrap();
    index(g.to_str().unwrap());
    search();
    let third = scrape();
    assert!(queries(&third) > queries(&second));
    assert_eq!(
        repeats(&third),
        repeats(&second),
        "a write advanced the applied index"
    );
    s.stop();
}
