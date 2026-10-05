//! #224: `serve` handles SIGTERM from before it starts. A supervisor (or
//! `sigterm_shuts_down_gracefully` in `serve_e2e`) may send it as soon as
//! it reads the start line, or while a long store open or log replay runs;
//! a signal that arrives before its handler is registered takes the
//! default action, so the process died with no graceful stop and left its
//! LOCK sidecar behind.
//!
//! The test binary re-runs itself as a child serving a store, which sends
//! SIGTERM to itself and waits until it was sent:
//! - `ready`: from `on_ready`, the hook the CLI prints its start line from
//!   (deterministic: before the fix the handler was registered only after
//!   `on_ready`, and the child always died of the signal);
//! - `start`: from a thread, as soon as the LOCK sidecar exists, which
//!   `start` writes while it is still opening the node.
//!
//! Either way the child must stop gracefully: `run_blocking_with` returns
//! `Ok`, the LOCK sidecar is gone, exit status 0.
#![cfg(unix)]
use graph_core::Extractor;
use graph_server::ServeConfig;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "MEMORY_GRAPH_TEST_SIGNAL_CHILD_DB";
const WHEN_ENV: &str = "MEMORY_GRAPH_TEST_SIGNAL_WHEN";
const STOPPED: &str = "CHILD-STOPPED-GRACEFULLY";

fn exts() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn lock_of(db: &Path) -> PathBuf {
    let mut lock = db.as_os_str().to_owned();
    lock.push(".LOCK");
    PathBuf::from(lock)
}

fn sigterm_self() {
    let st = Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .expect("run kill");
    assert!(st.success(), "kill -TERM: {st:?}");
}

/// The child: SIGTERM to itself at `ready` or during `start`. A no-op in a
/// normal run.
#[test]
fn child_signals_itself() {
    let Ok(db) = std::env::var(CHILD_ENV) else {
        return;
    };
    let at_start = std::env::var(WHEN_ENV).as_deref() == Ok("start");
    if at_start {
        let lock = lock_of(Path::new(&db));
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !lock.exists() {
                assert!(Instant::now() < deadline, "no LOCK sidecar within 60 s");
                std::thread::sleep(Duration::from_millis(1));
            }
            sigterm_self();
        });
    }
    let cfg = ServeConfig::new(&db, "127.0.0.1:0".parse().unwrap());
    graph_server::run_blocking_with(cfg, exts(), move |_| {
        if !at_start {
            sigterm_self();
        }
    })
    .expect("a graceful stop");
    println!("{STOPPED}");
}

fn run_child(when: &str) {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_signals_itself",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &db)
        .env(WHEN_ENV, when)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(STOPPED),
        "the child must stop gracefully on a SIGTERM sent at {when} ({:?}):\n{stdout}\n{stderr}",
        out.status
    );
    assert!(
        !lock_of(&db).exists(),
        "a graceful stop removes the LOCK sidecar"
    );
}

#[test]
fn sigterm_right_after_ready_stops_gracefully() {
    run_child("ready");
}

#[test]
fn sigterm_during_start_stops_gracefully_once_started() {
    run_child("start");
}
