//! A shared readiness wait for e2e tests that spawn `memory-graph serve`
//! (issue #202). `cluster_e2e`, `backup_e2e`, `s3_e2e` and
//! `observability_e2e` use it; other test binaries still parse the
//! listening line themselves.
//!
//! [`start_serve`] spawns the command with both pipes drained by threads,
//! reads stdout until the `listening on <addr>` start line, then confirms
//! the server answers a `grpc.health.v1` liveness check (`memory-graph
//! health`) before returning. Every wait shares one deadline: the
//! [`ready_timeout`], 180 s unless `MG_E2E_READY_TIMEOUT_SECS` says
//! otherwise, which is longer than `serve --join`'s default 2 m join
//! timeout (a joiner prints its listening line only after it joined). A
//! failure carries everything the process printed, so a flaky CI run can
//! be diagnosed from its log.
#![allow(dead_code)] // each test binary uses a different subset

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

/// Overrides [`DEFAULT_READY_TIMEOUT`], in whole seconds.
pub const READY_TIMEOUT_ENV: &str = "MG_E2E_READY_TIMEOUT_SECS";

/// Longer than `serve --join-timeout`'s 2 m default, with headroom for a
/// loaded CI runner.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(180);

const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// The most one `memory-graph health` probe may take before it is killed.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The readiness deadline: [`READY_TIMEOUT_ENV`] when set, else
/// [`DEFAULT_READY_TIMEOUT`]. Panics on a value that is not a positive
/// whole number of seconds.
pub fn ready_timeout() -> Duration {
    let Ok(raw) = std::env::var(READY_TIMEOUT_ENV) else {
        return DEFAULT_READY_TIMEOUT;
    };
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Duration::from_secs(secs),
        _ => panic!("{READY_TIMEOUT_ENV}={raw:?} is not a positive number of seconds"),
    }
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
    /// [`StartOptions::echo_stderr`] is set), including those printed
    /// before readiness.
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
/// pipes) and wait for it to be ready. On failure the process is killed,
/// both pipes are read to the end, and its whole output returned.
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
    let (stdout, stdout_reader) = drain_stdout(child.stdout.take().expect("piped stdout"));
    let capture = StderrCapture::default();
    let (stderr, stderr_reader) = drain_stderr(
        child.stderr.take().expect("piped stderr"),
        options.echo_stderr,
        capture.clone(),
    );
    let fail = |mut child: Child, start_lines: Vec<String>, reason: String| {
        let _ = child.kill();
        let status = child.wait().ok();
        // The pipes are closed now: joining the readers guarantees the last
        // lines (a port in use, a join error) are captured.
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        StartFailure {
            reason,
            status,
            stdout: start_lines,
            stderr: capture.take(),
        }
    };

    let mut start_lines = Vec::new();
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
                return Err(fail(child, start_lines, reason));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let reason = "stdout closed before the listening line".to_string();
                return Err(fail(child, start_lines, reason));
            }
        }
    };
    // Keep draining so the server never blocks on a print.
    std::thread::spawn(move || stdout.iter().for_each(drop));

    if let Err(reason) = await_health(&mut child, &addr, deadline) {
        return Err(fail(child, start_lines, reason));
    }
    capture.stop();
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
        if health_ok(addr, deadline) {
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

/// One health probe, killed after [`PROBE_LIMIT`] or at `deadline`,
/// whichever comes first.
fn health_ok(addr: &str, deadline: Instant) -> bool {
    let probe_deadline = deadline.min(Instant::now() + PROBE_LIMIT);
    let spawned = Command::new(BIN)
        .env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_CONFIG")
        .args(["--server", addr, "health"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut probe) = spawned else {
        return false;
    };
    loop {
        match probe.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < probe_deadline => std::thread::sleep(PROBE_INTERVAL),
            _ => {
                let _ = probe.kill();
                let _ = probe.wait();
                return false;
            }
        }
    }
}

/// Stderr lines kept for a [`StartFailure`] until readiness, then no more.
#[derive(Clone, Default)]
struct StderrCapture {
    lines: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
}

impl StderrCapture {
    fn push(&self, line: &str) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line.to_string());
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.take();
    }

    fn take(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|mut lines| std::mem::take(&mut *lines))
            .unwrap_or_default()
    }
}

fn drain_stdout(reader: impl Read + Send + 'static) -> (Receiver<String>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            // The receiver may be gone; keep draining regardless.
            let _ = tx.send(line);
        }
    });
    (rx, handle)
}

fn drain_stderr(
    reader: impl Read + Send + 'static,
    echo: bool,
    capture: StderrCapture,
) -> (Receiver<String>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if echo {
                eprintln!("{line}");
            }
            capture.push(&line);
            let _ = tx.send(line);
        }
    });
    (rx, handle)
}
