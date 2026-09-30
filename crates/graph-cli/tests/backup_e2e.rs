//! Epic story 37 (ADR 0006) end to end with the real binary against the
//! in-process `FakeS3`: `serve --backup-url s3://... --backup-on none`,
//! `cluster snapshot --upload`, `cluster backups [--json]`, then a second
//! `serve --bootstrap --restore s3://.../latest` that answers like the
//! source, and a restore of a name that is no backup refused with nothing
//! left behind.
use graph_server::testing::fake_s3::FAKE_REGION;
use graph_server::testing::FakeS3;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");
const WAIT: Duration = Duration::from_secs(90);

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_CONFIG")
        // The keys come from the credentials file, never a developer's own.
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN");
    c
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn run(args: &[&str]) -> Output {
    cmd().args(args).output().unwrap()
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

fn json(args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&ok(args)).unwrap()
}

/// A running `serve`, killed on drop.
struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(args: &[String]) -> Server {
    let mut child = cmd()
        .arg("serve")
        .args(["--listen", "127.0.0.1:0", "--min-free-disk", "1"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
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
            panic!("serve {args:?} printed no listening line ({e}); exit: {st:?}");
        }
    };
    let addr = line
        .split("listening on ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .to_string();
    let s = Server { child, addr };
    // A --bootstrap node leads itself once its election ran.
    let deadline = Instant::now() + WAIT;
    while !run(&["--server", &s.addr, "cluster", "leader"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "no leader on {}", s.addr);
        std::thread::sleep(Duration::from_millis(100));
    }
    s
}

fn s3_flags(f: &FakeS3, creds: &Path) -> Vec<String> {
    vec![
        "--backup-endpoint".into(),
        f.endpoint(),
        "--backup-region".into(),
        FAKE_REGION.into(),
        "--backup-credentials-file".into(),
        creds.display().to_string(),
    ]
}

fn answers(addr: &str) -> Vec<String> {
    [
        vec!["describe", "--json"],
        vec!["search", "alpha", "--json"],
        vec!["symbols", "*", "--json"],
    ]
    .iter()
    .map(|q| {
        let mut a = vec!["--server", addr];
        a.extend(q);
        // `stale_possible` depends on the node's view, not on the data.
        ok(&a)
            .replace(",\"stale_possible\":false", "")
            .replace(",\"stale_possible\":true", "")
    })
    .collect()
}

#[test]
fn upload_list_and_restore_from_s3() {
    let f = FakeS3::start();
    let root = tempfile::tempdir().unwrap();
    let creds = root.path().join("aws-credentials");
    f.write_credentials(&creds);
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "fn alpha() -> u32 { 1 }\n").unwrap();
    std::fs::write(
        src.join("b.rs"),
        "struct Beta;\nimpl Beta { fn alpha(&self) {} }\n",
    )
    .unwrap();

    let mut args: Vec<String> = [
        "--data-dir",
        root.path().join("d1").to_str().unwrap(),
        "--node-id",
        "1",
        "--bootstrap",
        "--backup-url",
        "s3://backups/mg",
        "--backup-on",
        "none",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(s3_flags(&f, &creds));
    let source = serve(&args);
    ok(&[
        "--server",
        &source.addr,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--no-progress",
        src.to_str().unwrap(),
    ]);
    let cluster = json(&["--server", &source.addr, "cluster", "status", "--json"])["cluster_id"]
        .as_str()
        .unwrap()
        .to_string();
    // `--backup-on none`: nothing yet.
    let listed = json(&["--server", &source.addr, "cluster", "backups", "--json"]);
    assert_eq!(listed["backups"].as_array().unwrap().len(), 0, "{listed}");

    let up = json(&[
        "--server",
        &source.addr,
        "cluster",
        "snapshot",
        "--upload",
        "--json",
    ]);
    let url = up["url"].as_str().unwrap().to_string();
    let sha = up["sha256"].as_str().unwrap().to_string();
    assert!(
        url.starts_with(&format!("s3://backups/mg/{cluster}/snap-")) && url.ends_with(".redb"),
        "{up}"
    );
    assert_eq!(sha.len(), 64, "{up}");
    let meta_key = url
        .strip_prefix("s3://backups/")
        .unwrap()
        .replace(".redb", ".meta");
    let meta: serde_json::Value =
        serde_json::from_slice(&f.object("backups", &meta_key).expect("the .meta committed"))
            .unwrap();
    assert_eq!(meta["sha256"].as_str().unwrap(), sha);

    let listed = json(&["--server", &source.addr, "cluster", "backups", "--json"]);
    let backups = listed["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 1, "{listed}");
    assert_eq!(backups[0]["url"].as_str().unwrap(), url);
    assert_eq!(backups[0]["sha256"].as_str().unwrap(), sha);
    assert_eq!(listed["cluster_id"].as_str().unwrap(), cluster);
    let plain = ok(&["--server", &source.addr, "cluster", "backups"]);
    assert!(plain.contains(&url) && plain.contains(&sha), "{plain}");

    // Restore `latest` into a new cluster: it answers like the source.
    let latest = format!("s3://backups/mg/{cluster}/latest");
    let mut args: Vec<String> = [
        "--data-dir",
        root.path().join("d2").to_str().unwrap(),
        "--node-id",
        "1",
        "--bootstrap",
        "--restore",
        &latest,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(s3_flags(&f, &creds));
    let restored = serve(&args);
    assert_eq!(answers(&source.addr), answers(&restored.addr));
    let st = json(&["--server", &restored.addr, "cluster", "status", "--json"]);
    assert_ne!(st["cluster_id"].as_str().unwrap(), cluster);

    // A name that is no backup: refused, nothing left behind.
    let d3 = root.path().join("d3");
    let missing = format!("s3://backups/mg/{cluster}/snap-9-9.redb");
    let mut c = cmd();
    c.arg("serve")
        .args(["--listen", "127.0.0.1:0", "--min-free-disk", "1"])
        .args(["--data-dir", d3.to_str().unwrap(), "--node-id", "1"])
        .args(["--bootstrap", "--restore", &missing])
        .args(s3_flags(&f, &creds));
    let o = c.output().unwrap();
    assert!(!o.status.success(), "{}", text(&o.stdout));
    let err = text(&o.stderr);
    assert!(err.contains("not a committed backup"), "{err}");
    assert!(!d3.join("graph.redb").exists());
    assert!(!d3.join("graph.redb.restore.tmp").exists());

    // The S3 settings need an s3:// target (a backup URL or a restore).
    let o = run(&[
        "serve",
        "--db",
        root.path().join("x.redb").to_str().unwrap(),
        "--backup-endpoint",
        &f.endpoint(),
    ]);
    assert!(!o.status.success());
}
