//! One readiness wait for every e2e test that spawns `memory-graph serve`
//! (issue #202).
//!
//! [`start_serve`] spawns the command with both pipes drained by threads,
//! reads stdout until the `listening on <addr>` start line, then confirms
//! the server answers a `grpc.health.v1` liveness check (`memory-graph
//! health`) before returning. Every wait shares one deadline: the
//! [`ready_timeout`], 180 s unless `MG_E2E_READY_TIMEOUT_SECS` says
//! otherwise, which is longer than `serve --join`'s default 2 m join
//! timeout (a joiner prints its listening line only after it joined). A
//! failure carries everything the process printed so far, so a flaky CI
//! run can be diagnosed from its log.
#![allow(dead_code)] // each test binary uses a different subset

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

/// Overrides [`DEFAULT_READY_TIMEOUT`], in whole seconds.
pub const READY_TIMEOUT_ENV: &str = "MG_E2E_READY_TIMEOUT_SECS";

/// Longer than `serve --join-timeout`'s 2 m default, with headroom for a
/// loaded CI runner.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(180);

const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// The readiness deadline: [`READY_TIMEOUT_ENV`] when set to a number of
/// seconds, else [`DEFAULT_READY_TIMEOUT`].
pub fn ready_timeout() -> Duration {
    std::env::var(READY_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_READY_TIMEOUT)
}

/// A `serve` that printed its listening line and answered a health check.
/// The caller owns the child (nothing here kills it on drop).
pub struct ServeProcess {
    pub child: Child,
    /// The gRPC address from the `listening on <addr>` line.
    pub addr: String,
    /// The `mcp on http://<addr>/mcp` endpoint, when printed.
    pub mcp: Option<SocketAddr>,
    /// The `metrics on http://<addr>/metrics` endpoint, when printed.
    pub metrics: Option<SocketAddr>,
    /// Every stdout line up to and including the listening line.
    pub start_lines: Vec<String>,
    /// Every stderr line, as it arrives (also echoed when
    /// [`StartOptions::echo_stderr`] is set). Lines printed before
    /// readiness are in here too.
    pub stderr: Receiver<String>,
}

/// Why a `serve` did not become ready, with everything it printed.
#[derive(Debug)]
pub struct StartFailure {
    pub reason: String,
    pub status: Option<ExitStatus>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl StartFailure {
    /// The captured stderr, one string.
    pub fn stderr_text(&self) -> String {
        self.stderr.join("\n")
    }
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; exit: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.reason,
            self.status,
            self.stdout.join("\n"),
            self.stderr.join("\n")
        )
    }
}

/// How [`start_serve`] waits.
#[derive(Clone, Copy, Debug)]
pub struct StartOptions {
    pub timeout: Duration,
    /// Copy every stderr line to the test's own stderr (what
    /// `Stdio::inherit()` gave before).
    pub echo_stderr: bool,
}

impl Default for StartOptions {
    fn default() -> Self {
        StartOptions {
            timeout: ready_timeout(),
            echo_stderr: true,
        }
    }
}

/// [`try_start_serve`], panicking with the captured output on failure.
pub fn start_serve(cmd: Command, options: StartOptions) -> ServeProcess {
    try_start_serve(cmd, options).unwrap_or_else(|e| panic!("serve did not become ready: {e}"))
}

/// Spawn `cmd` (a `memory-graph serve ...`; its stdio is replaced by
/// pipes) and wait for it to be ready. On failure the process is killed
/// and its output returned.
pub fn try_start_serve(
    mut cmd: Command,
    options: StartOptions,
) -> Result<ServeProcess, StartFailure> {
    let deadline = Instant::now() + options.timeout;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn memory-graph serve");
    let stdout = drain(child.stdout.take().expect("piped stdout"), false);
    let stderr_log = Arc::new(Mutex::new(Vec::new()));
    let stderr = drain_stderr(
        child.stderr.take().expect("piped stderr"),
        options.echo_stderr,
        Arc::clone(&stderr_log),
    );
    let mut start_lines = Vec::new();
    let fail = |child: &mut Child, start_lines: &[String], reason: String| {
        let _ = child.kill();
        let status = child.wait().ok();
        // The pipe is closed now; give the drain thread a moment to finish.
        std::thread::sleep(Duration::from_millis(50));
        StartFailure {
            reason,
            status,
            stdout: start_lines.to_vec(),
            stderr: stderr_log.lock().map(|l| l.clone()).unwrap_or_default(),
        }
    };

    let addr = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match stdout.recv_timeout(left) {
            Ok(line) => {
                let addr = listening_addr(&line);
                start_lines.push(line);
                if let Some(addr) = addr {
                    break addr;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let reason = format!("no listening line within {:?}", options.timeout);
                return Err(fail(&mut child, &start_lines, reason));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let reason = "stdout closed before the listening line".to_string();
                return Err(fail(&mut child, &start_lines, reason));
            }
        }
    };
    // Keep draining so the server never blocks on a print.
    std::thread::spawn(move || stdout.iter().for_each(drop));

    if let Err(reason) = await_health(&mut child, &addr, deadline) {
        return Err(fail(&mut child, &start_lines, reason));
    }
    Ok(ServeProcess {
        child,
        mcp: endpoint(&start_lines, "mcp on http://", "/mcp"),
        metrics: endpoint(&start_lines, "metrics on http://", "/metrics"),
        addr,
        start_lines,
        stderr,
    })
}

/// The address in a `listening on <addr> (...)` line.
pub fn listening_addr(line: &str) -> Option<String> {
    line.split("listening on ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
}

fn endpoint(lines: &[String], prefix: &str, suffix: &str) -> Option<SocketAddr> {
    lines.iter().find_map(|l| {
        let (_, rest) = l.split_once(prefix)?;
        // The endpoint may be followed by more text on the line.
        rest.split(suffix).next()?.parse().ok()
    })
}

/// Poll `memory-graph health` (liveness, no leader needed) until it
/// passes, the process exits, or the deadline.
fn await_health(child: &mut Child, addr: &str, deadline: Instant) -> Result<(), String> {
    loop {
        if health_ok(addr) {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("exited ({status}) before answering a health check"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "printed its listening line but {addr} never answered a health check"
            ));
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

fn health_ok(addr: &str) -> bool {
    Command::new(BIN)
        .env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_CONFIG")
        .args(["--server", addr, "health"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn drain(reader: impl Read + Send + 'static, echo: bool) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if echo {
                eprintln!("{line}");
            }
            // The receiver may be gone; keep draining regardless.
            let _ = tx.send(line);
        }
    });
    rx
}

fn drain_stderr(
    reader: impl Read + Send + 'static,
    echo: bool,
    log: Arc<Mutex<Vec<String>>>,
) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let lines = drain(reader, echo);
    std::thread::spawn(move || {
        for line in lines {
            if let Ok(mut l) = log.lock() {
                l.push(line.clone());
            }
            let _ = tx.send(line);
        }
    });
    rx
}
