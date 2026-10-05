//! #224: `serve` handles SIGTERM from the moment it announces itself. A
//! supervisor (or `sigterm_shuts_down_gracefully` in `serve_e2e`) may send
//! it as soon as it reads the start line; a signal that arrives before its
//! handler is registered takes the default action, so the process died
//! with no graceful stop and left its LOCK sidecar behind.
//!
//! Deterministic: the test binary re-runs itself as a child serving a
//! store, whose `on_ready` (the hook the CLI prints its start line from)
//! sends SIGTERM to the child itself and waits until it was sent. The
//! child must then stop gracefully: `run_blocking_with` returns `Ok`, the
//! LOCK sidecar is gone, exit status 0.
#![cfg(unix)]
use graph_core::Extractor;
use graph_server::ServeConfig;
use std::process::Command;

const CHILD_ENV: &str = "MEMORY_GRAPH_TEST_SIGNAL_CHILD_DB";
const STOPPED: &str = "CHILD-STOPPED-GRACEFULLY";

fn exts() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// The child: SIGTERM to itself from `on_ready`. A no-op in a normal run.
#[test]
fn child_signals_itself_at_ready() {
    let Ok(db) = std::env::var(CHILD_ENV) else {
        return;
    };
    let cfg = ServeConfig::new(db, "127.0.0.1:0".parse().unwrap());
    graph_server::run_blocking_with(cfg, exts(), |_| {
        let st = Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("run kill");
        assert!(st.success(), "kill -TERM: {st:?}");
    })
    .expect("a graceful stop");
    println!("{STOPPED}");
}

#[test]
fn sigterm_right_after_ready_stops_gracefully() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_signals_itself_at_ready",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &db)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(STOPPED),
        "the child must stop gracefully on a SIGTERM sent at ready ({:?}):\n{stdout}\n{stderr}",
        out.status
    );
    let mut lock = db.into_os_string();
    lock.push(".LOCK");
    assert!(
        !std::path::Path::new(&lock).exists(),
        "a graceful stop removes the LOCK sidecar"
    );
}
