//! Dev review 9: a real crash. The test binary re-runs itself as a child
//! process serving a `--data-dir` node, writes through it, kills it
//! (`TerminateProcess` / `SIGKILL`: no destructor, no redb close), starts
//! it again on the same directory and checks every acknowledged write is
//! there. Only the child this test spawned is ever killed.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::{Extractor, NodeKind};
use graph_server::{InitMode, RaftSettings, ServeConfig};
use graph_store::StoreRead;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

const CHILD_ENV: &str = "MEMORY_GRAPH_TEST_CHILD_DATA_DIR";
const READY: &str = "CHILD-SERVING ";

fn exts() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

/// The child: serves until killed. A no-op in a normal test run.
#[test]
fn child_server_process() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut cfg = ServeConfig::for_data_dir(
        dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap { restore: None },
        Some(1),
    );
    cfg.raft = Some(RaftSettings::standalone());
    graph_server::run_blocking_with(cfg, exts(), |addr| {
        use std::io::Write;
        println!("{READY}{addr}");
        let _ = std::io::stdout().flush();
    })
    .unwrap();
}

/// Kills the child on every exit path of the test.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(dir: &std::path::Path) -> (ChildGuard, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_server_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let guard = ChildGuard(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            // The harness prints "test child_server_process ... " first.
            if let Some(at) = line.find(READY) {
                let addr = line[at + READY.len()..].trim();
                let _ = tx.send(addr.to_string());
            }
        }
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the child server did not report its address within 30 s");
    (guard, addr)
}

fn client(addr: &str) -> RemoteStore {
    let mut cfg = ClientConfig::new(addr.to_string());
    cfg.write_deadline = Duration::from_secs(10);
    RemoteStore::connect(cfg).unwrap()
}

#[test]
fn a_killed_server_process_keeps_every_acked_write() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("n");
    let (mut child, addr) = spawn(&dir);
    let c = client(&addr);
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("f{i}.rs"), format!("fn f{i}() {{}}").into_bytes()))
        .collect();
    for f in &files {
        // One `Write.Index` per file: each answers with its log index.
        let one = [graph_store::BatchFile {
            path: &f.0,
            bytes: &f.1,
            language: None,
            origin: None,
        }];
        graph_store::Store::index_batch(&c, "o", "r", &one, Default::default()).unwrap();
    }
    let acked = c.applied_index().load(Ordering::SeqCst);
    assert!(acked > 0);
    // Killed the moment the last write returned.
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    drop(c);
    let (_child, addr) = spawn(&dir);
    let c = client(&addr);
    let st = c.admin_status().unwrap();
    assert!(st.applied_index >= acked, "{} < {acked}", st.applied_index);
    assert_eq!(c.count_nodes(NodeKind::File).unwrap(), 5);
}
