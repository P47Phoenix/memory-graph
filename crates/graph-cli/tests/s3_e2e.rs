//! Epic story 38 (ADR 0006 testing): backups against a **real** S3 server
//! with the real binary. Skipped unless `MG_S3_ENDPOINT` is set; CI's
//! `s3-e2e` job runs it against SeaweedFS over plain HTTP (the ADR named
//! MinIO, whose images can no longer be pulled from Docker Hub).
//!
//! * `MG_S3_ENDPOINT`: `http://host:port` of the server.
//! * `MG_S3_BUCKET`: an existing bucket (default `mg-e2e`).
//! * `MG_S3_REGION`: the signing region (default `us-east-1`).
//! * `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`: its credentials. The
//!   test hands them to the binary through a credentials file.
//!
//! It bootstraps a node with `--backup-keep 2`, indexes the vendored corpus
//! in three parts with a `cluster snapshot --upload` after each, checks
//! `cluster backups` and that retention kept the newest two, restores
//! `latest` into a fresh data directory and diffs the answers, checks that
//! a wrong secret is refused (auth is enforced) and a real multipart upload
//! round-trips, and that no multipart upload is left dangling.
use graph_server::backup::s3::{S3Options, S3Sink};
use graph_server::backup::BackupSink;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");
const WAIT: Duration = Duration::from_secs(120);

struct Env {
    endpoint: String,
    bucket: String,
    region: String,
    key: String,
    secret: String,
}

fn s3_env() -> Option<Env> {
    let endpoint = std::env::var("MG_S3_ENDPOINT").ok()?;
    let get = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| panic!("MG_S3_ENDPOINT is set but {k} is not"))
    };
    Some(Env {
        endpoint,
        bucket: std::env::var("MG_S3_BUCKET").unwrap_or_else(|_| "mg-e2e".into()),
        region: std::env::var("MG_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        key: get("AWS_ACCESS_KEY_ID"),
        secret: get("AWS_SECRET_ACCESS_KEY"),
    })
}

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
        .env_remove("MEMORY_GRAPH_CONFIG")
        // The binary takes the keys from the credentials file only, so the
        // wrong-key case below really uses the wrong key.
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

fn write_creds(path: &Path, key: &str, secret: &str) {
    std::fs::write(
        path,
        format!("[default]\naws_access_key_id = {key}\naws_secret_access_key = {secret}\n"),
    )
    .unwrap();
}

fn s3_flags(e: &Env, creds: &Path) -> Vec<String> {
    vec![
        "--backup-endpoint".into(),
        e.endpoint.clone(),
        "--backup-region".into(),
        e.region.clone(),
        "--backup-credentials-file".into(),
        creds.display().to_string(),
    ]
}

/// A sink for checks from the test itself (never the environment's keys:
/// the file names them explicitly).
fn sink(e: &Env, url: &str, creds: &Path) -> S3Sink {
    S3Sink::new(
        url,
        S3Options {
            endpoint: Some(e.endpoint.clone()),
            region: e.region.clone(),
            credentials_file: Some(creds.to_path_buf()),
            credentials_from_env: false,
            ..S3Options::default()
        },
    )
    .unwrap()
}

fn answers(addr: &str) -> Vec<String> {
    [
        vec!["describe", "--json"],
        vec!["search", "main", "--json"],
        vec!["search", "Request", "--grain", "symbol", "--json"],
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

fn upload(addr: &str) -> String {
    let up = json(&[
        "--server", addr, "cluster", "snapshot", "--upload", "--json",
    ]);
    up["url"].as_str().unwrap().to_string()
}

#[test]
fn backup_retention_and_restore_against_real_s3() {
    let Some(e) = s3_env() else {
        eprintln!("MG_S3_ENDPOINT is not set: skipping the real-S3 backup e2e");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let creds = root.path().join("aws-credentials");
    write_creds(&creds, &e.key, &e.secret);
    // A prefix of its own per run, so runs never see each other.
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let prefix = format!("e2e/{run_id}");
    let base = format!("s3://{}/{prefix}", e.bucket);
    let check = sink(&e, &base, &creds);

    // Auth is enforced: the right key lists, a wrong secret is refused.
    check.list("").expect("listing with the right key");
    let bad = root.path().join("bad-credentials");
    write_creds(&bad, &e.key, "not-the-secret");
    let err = sink(&e, &base, &bad)
        .list("")
        .expect_err("a wrong secret must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");

    // A real multipart upload (5 MiB parts, S3's minimum) round-trips and
    // completes, leaving no upload in progress.
    let mp = S3Sink::new(
        &base,
        S3Options {
            endpoint: Some(e.endpoint.clone()),
            region: e.region.clone(),
            credentials_file: Some(creds.clone()),
            credentials_from_env: false,
            multipart_threshold: 5 << 20,
            part_size: 5 << 20,
            ..S3Options::default()
        },
    )
    .unwrap();
    let blob: Vec<u8> = (0..(12u32 << 20)).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        mp.put("mp/blob", &mut blob.as_slice()).unwrap(),
        blob.len() as u64
    );
    let mut back = Vec::new();
    mp.get("mp/blob", &mut back).unwrap();
    assert!(back == blob, "the multipart object differs");
    assert!(mp.incomplete_uploads("mp/").unwrap().is_empty());
    mp.delete("mp/blob").unwrap();

    // The source: keep 2, uploads on demand only.
    let mut args: Vec<String> = [
        "--data-dir",
        root.path().join("d1").to_str().unwrap(),
        "--node-id",
        "1",
        "--bootstrap",
        "--backup-url",
        &base,
        "--backup-on",
        "none",
        "--backup-keep",
        "2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(s3_flags(&e, &creds));
    let source = serve(&args);
    let cluster = json(&["--server", &source.addr, "cluster", "status", "--json"])["cluster_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The corpus in three parts, one upload after each.
    let repos = corpus_repos();
    assert!(repos.len() >= 3, "the corpus is missing");
    let third = repos.len().div_ceil(3);
    let mut urls = Vec::new();
    for part in repos.chunks(third) {
        for repo in part {
            let dir = corpus().join(repo);
            ok(&[
                "--server",
                &source.addr,
                "index",
                "--org",
                "corpus",
                "--repo",
                repo,
                "--no-progress",
                dir.to_str().unwrap(),
            ]);
        }
        let url = upload(&source.addr);
        assert!(
            url.starts_with(&format!("{base}/{cluster}/snap-")) && url.ends_with(".redb"),
            "{url}"
        );
        urls.push(url);
    }
    assert_eq!(urls.len(), 3);

    // Retention keeps the newest two; the first pair is gone from the bucket.
    let deadline = Instant::now() + WAIT;
    let listed = loop {
        let l = json(&["--server", &source.addr, "cluster", "backups", "--json"]);
        if l["backups"].as_array().unwrap().len() == 2 {
            break l;
        }
        assert!(Instant::now() < deadline, "retention did not keep 2: {l}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(listed["cluster_id"].as_str().unwrap(), cluster);
    let got: Vec<&str> = listed["backups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["url"].as_str().unwrap())
        .collect();
    assert_eq!(got, vec![urls[2].as_str(), urls[1].as_str()], "{listed}");
    let keys: Vec<String> = check
        .list(&format!("{cluster}/"))
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    let first = urls[0].rsplit('/').next().unwrap();
    assert!(
        !keys
            .iter()
            .any(|k| k.contains(first.trim_end_matches(".redb"))),
        "the oldest backup survived retention: {keys:?}"
    );
    assert_eq!(keys.len(), 4, "two .redb + .meta pairs: {keys:?}");

    // A restore with the wrong secret is refused, nothing left behind.
    let latest = format!("{base}/{cluster}/latest");
    let d_bad = root.path().join("d-bad");
    let o = cmd()
        .arg("serve")
        .args(["--listen", "127.0.0.1:0", "--min-free-disk", "1"])
        .args(["--data-dir", d_bad.to_str().unwrap(), "--node-id", "1"])
        .args(["--bootstrap", "--restore", &latest])
        .args(s3_flags(&e, &bad))
        .output()
        .unwrap();
    assert!(!o.status.success(), "{}", text(&o.stdout));
    assert!(!d_bad.join("graph.redb").exists());
    assert!(!d_bad.join("graph.redb.restore.tmp").exists());

    // Restore `latest` into a fresh directory: it answers like the source.
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
    args.extend(s3_flags(&e, &creds));
    let restored = serve(&args);
    let a = answers(&source.addr);
    let b = answers(&restored.addr);
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert!(x == y, "answer {i} differs after the restore");
    }
    let st = json(&["--server", &restored.addr, "cluster", "status", "--json"]);
    assert_ne!(st["cluster_id"].as_str().unwrap(), cluster);

    // Nothing dangling: no multipart upload left in progress anywhere
    // under this run's prefix.
    let left = check.incomplete_uploads("").unwrap();
    assert!(left.is_empty(), "dangling multipart uploads: {left:?}");

    // Tidy the run's objects (best effort; the bucket is throwaway in CI).
    drop(restored);
    drop(source);
    for o in check.list("").unwrap_or_default() {
        let _ = check.delete(&o.key);
    }
}
