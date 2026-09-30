//! The `s3://` backup sink (ADR 0006 E3-E6, epic story 36) against the
//! in-process `FakeS3`: the client itself (every verb, paging, addressing,
//! multipart above 64 MiB), the fault matrix (drop after N bytes, 500 on
//! part k, wrong ETag, 403, slow), credentials, and the story 35 scenarios
//! run through S3 instead of `file://`.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::backup::creds::Credentials;
use graph_server::backup::restore::{restore_from_sink, RestoreChecks};
use graph_server::backup::s3::{parse_endpoint, parse_s3_url, MULTIPART_THRESHOLD};
use graph_server::backup::{BackupConfig, BackupOn, BackupSink, S3Options, S3Sink};
use graph_server::testing::fake_s3::{FAKE_KEY_ID, FAKE_SECRET};
use graph_server::testing::{ClusterTestbed, FakeS3, Faults, TestServer, CLUSTER_WAIT, TEST_RAFT};
use graph_server::{InitMode, RaftSettings, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::{Store, StoreRead};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const BUCKET: &str = "backups";

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn get(s: &dyn BackupSink, key: &str) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    s.get(key, &mut v)?;
    Ok(v)
}

fn keys(s: &dyn BackupSink, prefix: &str) -> Vec<String> {
    s.list(prefix).unwrap().into_iter().map(|o| o.key).collect()
}

/// Small parts, so multipart runs without 64 MiB bodies.
fn small_parts(f: &FakeS3) -> S3Options {
    S3Options {
        multipart_threshold: 4096,
        part_size: 4096,
        ..f.options()
    }
}

#[test]
fn put_get_head_list_delete_round_trip() {
    let f = FakeS3::start();
    let s = f.sink(&format!("s3://{BUCKET}/root/dir"));
    assert!(s.list("").unwrap().is_empty());
    let odd = "c/a key with spaces+ünïcode~.redb";
    assert_eq!(s.put(odd, &mut &b"hello"[..]).unwrap(), 5);
    assert_eq!(get(&s, odd).unwrap(), b"hello");
    assert_eq!(s.put("c/empty", &mut &b""[..]).unwrap(), 0);
    assert_eq!(get(&s, "c/empty").unwrap(), b"");
    s.put("d/x", &mut &b"x"[..]).unwrap();
    // Outside the sink's prefix: never listed, never touched.
    f.put_object(BUCKET, "root/dirx/y", b"no");
    f.put_object(BUCKET, "elsewhere/y", b"no");
    assert_eq!(
        keys(&s, "c/"),
        ["c/a key with spaces+ünïcode~.redb", "c/empty"]
    );
    assert_eq!(keys(&s, "").len(), 3);
    let info = &s.list("c/a").unwrap()[0];
    assert_eq!(info.size, 5);
    let age = SystemTime::now().duration_since(info.modified).unwrap();
    assert!(age < Duration::from_secs(120), "{age:?}");
    // Paging: one key per ListObjectsV2 page.
    f.set_page_size(1);
    assert_eq!(keys(&s, "").len(), 3);
    assert!(
        f.requests()
            .iter()
            .any(|r| r.contains("continuation-token=")),
        "paged"
    );
    s.delete(odd).unwrap();
    s.delete(odd).unwrap();
    assert_eq!(get(&s, odd).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(f.object(BUCKET, "root/dir/d/x").unwrap(), b"x");
    for bad in ["", "/abs", "a/../b", "a//b", "a\\b"] {
        assert!(s.put(bad, &mut &b""[..]).is_err(), "{bad}");
    }
    // Path-style: the bucket is the first path segment.
    assert!(f
        .requests()
        .iter()
        .any(|r| r.starts_with("PUT /backups/root/dir/")));
    assert!(s
        .describe()
        .starts_with("s3://backups/root/dir via http://127.0.0.1:"));
}

#[test]
fn virtual_hosted_addressing() {
    let f = FakeS3::start();
    let opts = S3Options {
        virtual_host: true,
        endpoint: Some(format!("http://s3.test:{}", f.addr().port())),
        connect_to: Some(f.addr()),
        ..f.options()
    };
    let s = f.sink_with("s3://vbucket/p", opts);
    s.put("k/v", &mut &b"v"[..]).unwrap();
    assert_eq!(f.object("vbucket", "p/k/v").unwrap(), b"v");
    assert_eq!(keys(&s, "k/"), ["k/v"]);
    assert!(
        f.requests().iter().any(|r| r == "PUT /p/k/v"),
        "{:?}",
        f.requests()
    );
}

/// Uploads over 64 MiB use multipart (and exactly 64 MiB does not).
#[test]
fn over_64_mib_uses_multipart() {
    let f = FakeS3::start();
    let s = f.sink(&format!("s3://{BUCKET}"));
    let exact = bytes(MULTIPART_THRESHOLD, 1);
    s.put("exact", &mut exact.as_slice()).unwrap();
    assert!(!f.requests().iter().any(|r| r.contains("uploads")));
    let big = bytes(MULTIPART_THRESHOLD + 1, 2);
    assert_eq!(s.put("big", &mut big.as_slice()).unwrap(), big.len() as u64);
    let reqs = f.requests();
    assert!(
        reqs.iter().any(|r| r == "POST /backups/big?uploads="),
        "{reqs:?}"
    );
    let parts = reqs.iter().filter(|r| r.contains("partNumber=")).count();
    assert_eq!(parts, 5, "64 MiB + 1 in 16 MiB parts");
    assert_eq!(f.object(BUCKET, "big").unwrap(), big);
    assert_eq!(get(&s, "big").unwrap(), big);
    assert_eq!(f.pending_uploads(), 0);
}

/// The fault matrix: each fault fails the put with nothing stored under
/// the key and no multipart upload left open.
#[test]
fn the_fault_matrix() {
    let f = FakeS3::start();
    let small = f.sink(&format!("s3://{BUCKET}"));
    let multi = f.sink_with(&format!("s3://{BUCKET}"), small_parts(&f));
    let body = bytes(20_000, 3);
    let cases: Vec<(&str, Faults, &S3Sink, &str)> = vec![
        (
            "drop after N bytes (single)",
            Faults {
                drop_after_bytes: Some(1000),
                ..Default::default()
            },
            &small,
            "",
        ),
        (
            "drop after N bytes (a part)",
            Faults {
                drop_after_bytes: Some(500),
                ..Default::default()
            },
            &multi,
            "",
        ),
        (
            "500 on part 3",
            Faults {
                fail_part: Some(3),
                ..Default::default()
            },
            &multi,
            "HTTP 500 InternalError",
        ),
        (
            "wrong ETag",
            Faults {
                wrong_etag: true,
                ..Default::default()
            },
            &multi,
            "InvalidPart",
        ),
        (
            "403",
            Faults {
                forbidden: true,
                ..Default::default()
            },
            &small,
            "HTTP 403 AccessDenied",
        ),
    ];
    for (name, faults, sink, want) in cases {
        f.set_faults(faults);
        let e = sink
            .put(name, &mut body.as_slice())
            .expect_err(name)
            .to_string();
        assert!(e.contains(want), "{name}: {e}");
        f.clear_faults();
        assert!(f.object(BUCKET, name).is_none(), "{name}: stored");
        assert_eq!(f.pending_uploads(), 0, "{name}: an upload left open");
    }
    // Aborts were sent for the multipart cases.
    let aborts = f
        .requests()
        .iter()
        .filter(|r| r.starts_with("DELETE ") && r.contains("uploadId="))
        .count();
    assert_eq!(aborts, 3);
    // 403 on reads too, as PermissionDenied.
    f.set_faults(Faults {
        forbidden: true,
        ..Default::default()
    });
    assert_eq!(
        small.list("").unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        get(&small, "x").unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
    f.clear_faults();
    // With the faults gone the same sinks work.
    multi.put("ok", &mut body.as_slice()).unwrap();
    assert_eq!(f.object(BUCKET, "ok").unwrap(), body);
}

/// Slow: within the timeout it succeeds; past it, a `TimedOut` error.
#[test]
fn slow_responses_time_out() {
    let f = FakeS3::start();
    f.set_faults(Faults {
        delay: Some(Duration::from_millis(300)),
        ..Default::default()
    });
    let patient = f.sink(&format!("s3://{BUCKET}"));
    patient.put("a", &mut &b"a"[..]).unwrap();
    let hasty = f.sink_with(
        &format!("s3://{BUCKET}"),
        S3Options {
            timeout: Duration::from_millis(50),
            ..f.options()
        },
    );
    let t = Instant::now();
    let e = hasty.put("b", &mut &b"b"[..]).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::TimedOut, "{e}");
    assert!(t.elapsed() < Duration::from_secs(5));
}

/// A wrong secret is refused by the signature check, verbatim.
#[test]
fn a_bad_signature_is_refused() {
    let f = FakeS3::start();
    let s = S3Sink::with_credentials(
        parse_s3_url("s3://backups").unwrap(),
        parse_endpoint(&f.endpoint()).unwrap(),
        f.options(),
        Credentials::new(FAKE_KEY_ID, "not-the-secret"),
    )
    .unwrap();
    let e = s.put("k", &mut &b"k"[..]).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("SignatureDoesNotMatch"), "{e}");
    assert!(!e.to_string().contains("not-the-secret"));
}

/// Credentials from a file through `BackupConfig::open_sink` (the
/// production path), with the environment off; `Debug` never shows them.
#[test]
fn credentials_file_through_the_config() {
    let f = FakeS3::start();
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("credentials");
    f.write_credentials(&file);
    let mut b = BackupConfig::new(format!("s3://{BUCKET}/p"));
    b.s3 = S3Options {
        credentials_file: Some(file),
        ..f.options()
    };
    let sink = b.open_sink().unwrap();
    sink.put("k", &mut &b"v"[..]).unwrap();
    assert_eq!(f.object(BUCKET, "p/k").unwrap(), b"v");
    assert!(!format!("{b:?}").contains(FAKE_SECRET));
    // A missing profile, and no credentials at all, are refused.
    b.s3.profile = Some("nope".into());
    assert!(b.open_sink().err().unwrap().contains("[nope]"));
    b.s3.credentials_file = None;
    assert!(b.open_sink().err().unwrap().contains("no S3 credentials"));
    // S3 settings with a file:// URL are a configuration mistake.
    let mut fb = BackupConfig::new("file:///tmp/x");
    fb.s3.endpoint = Some(f.endpoint());
    assert!(fb.open_sink().err().unwrap().contains("s3:// only"));
}

/// Called from inside an async runtime it errors instead of panicking.
#[test]
fn inside_a_runtime_is_an_error_not_a_panic() {
    let f = FakeS3::start();
    let s = f.sink(&format!("s3://{BUCKET}"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let e = rt
        .block_on(async { s.put("k", &mut &b"v"[..]) })
        .unwrap_err();
    assert!(e.to_string().contains("off the async runtime"), "{e}");
    s.put("k", &mut &b"v"[..]).unwrap();
}

// ---- The story 35 scenarios, through S3 ----

const PREFIX: &str = "mg";

fn s3_cfg(f: &FakeS3, keep: usize, opts: S3Options) -> BackupConfig {
    let mut b = BackupConfig::new(format!("s3://{BUCKET}/{PREFIX}"));
    b.keep = keep;
    b.retry_backoff = Duration::from_millis(10);
    b.sink = Some(Arc::new(
        f.sink_with(&format!("s3://{BUCKET}/{PREFIX}"), opts),
    ));
    b
}

fn write_and_snapshot(tb: &ClusterTestbed, node: u64, i: usize) -> u64 {
    let f = small_file(i);
    tb.client(tb.leader())
        .index_bytes("o", "r", &f.0, &f.1, None)
        .unwrap();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    tb.client(node)
        .admin_trigger_snapshot(None)
        .unwrap()
        .last_applied_index
}

/// The committed `.meta` keys (relative to the bucket) of `cluster`.
fn metas(f: &FakeS3, cluster: &str) -> Vec<String> {
    let p = format!("{PREFIX}/{cluster}/");
    f.keys(BUCKET)
        .into_iter()
        .filter(|k| k.starts_with(&p) && k.ends_with(".meta"))
        .collect()
}

fn data_key(f: &FakeS3, cluster: &str, index: u64) -> String {
    metas(f, cluster)
        .into_iter()
        .find(|k| k.ends_with(&format!("-{index}.meta")))
        .unwrap_or_else(|| panic!("no committed backup at {index}"))
        .replace(".meta", ".redb")
}

/// The checks of this binary (the hash the uploaded `.meta` records).
fn checks(f: &FakeS3, cluster: &str) -> RestoreChecks {
    let meta = metas(f, cluster).pop().expect("a committed backup");
    let side: serde_json::Value =
        serde_json::from_slice(&f.object(BUCKET, &meta).unwrap()).unwrap();
    RestoreChecks {
        extractors_hash: side["extractors_hash"].as_str().unwrap().to_string(),
        allow_extractor_mismatch: false,
        min_free_disk: 0,
        probe: None,
    }
}

/// Download and verify `name` (or `latest`) of `cluster` into a local store
/// file under `dir`.
fn restore_s3(
    f: &FakeS3,
    cluster: &str,
    name: &str,
    dir: &Path,
    c: &RestoreChecks,
) -> Result<PathBuf, graph_store::StoreError> {
    std::fs::create_dir_all(dir).unwrap();
    let store = dir.join("graph.redb");
    let sink = f.sink(&format!("s3://{BUCKET}/{PREFIX}"));
    restore_from_sink(&sink, &format!("{cluster}/"), name, &store, c).map(|_| store)
}

fn restore_cfg(dir: &Path, restore: impl Into<PathBuf>) -> ServeConfig {
    ServeConfig::for_data_dir(
        dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap {
            restore: Some(restore.into()),
        },
        Some(1),
    )
}

/// Upload then restore `latest` equals the source, in one PUT and in
/// multipart.
#[test]
fn upload_then_restore_equals_the_source() {
    let _w = watchdog("s3 upload_then_restore", TEST_LIMIT);
    for multipart in [false, true] {
        let f = FakeS3::start();
        let opts = if multipart {
            small_parts(&f)
        } else {
            f.options()
        };
        let mut tb = ClusterTestbed::with_backup(3, exts(), s3_cfg(&f, 7, opts));
        tb.form();
        let leader = tb.leader();
        let c = tb.client(leader);
        for i in 0..12 {
            let fl = small_file(i);
            c.index_bytes("o", "r", &fl.0, &fl.1, None).unwrap();
        }
        let index = write_and_snapshot(&tb, leader, 100);
        tb.wait_backups_idle(CLUSTER_WAIT);
        let cluster = c.admin_status().unwrap().cluster_id;
        let data = data_key(&f, &cluster, index);
        // Data first, then the .meta.
        let reqs = f.requests();
        let pos = |suffix: &str| {
            reqs.iter()
                .rposition(|r| {
                    (r.starts_with("PUT ") || r.starts_with("POST "))
                        && r.split('?').next().unwrap().ends_with(suffix)
                })
                .unwrap_or_else(|| panic!("{suffix} never written: {reqs:?}"))
        };
        assert!(pos(&data) < pos(&data.replace(".redb", ".meta")));
        assert_eq!(
            reqs.iter()
                .any(|r| r.starts_with("POST ") && r.contains("uploads=")),
            multipart,
            "multipart {multipart}"
        );
        let st = c.admin_status().unwrap().backup.expect("backup status");
        assert_eq!(st.last_index, index);
        assert!(st.last_backup_error.is_empty());
        let root = tempfile::tempdir().unwrap();
        let chk = checks(&f, &cluster);
        let local = restore_s3(&f, &cluster, "latest", &root.path().join("dl"), &chk).unwrap();
        let s = TestServer::try_start_config(restore_cfg(&root.path().join("to"), &local), exts())
            .unwrap();
        let r = RemoteStore::connect(ClientConfig::new(s.endpoint())).unwrap();
        run_differential(&replica(&tb, leader), &r);
    }
}

/// A partial upload is invisible: data without a `.meta` (the `.meta` put
/// fails), or nothing at all (the connection drops mid-data).
#[test]
fn a_partial_upload_is_invisible() {
    let _w = watchdog("s3 a_partial_upload_is_invisible", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 7, small_parts(&f)));
    // A good backup first, so `latest` has something to (not) fall to.
    write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let chk = checks(&f, &cluster);
    let good = metas(&f, &cluster);
    f.set_faults(Faults {
        fail_puts_ending: Some(".meta".into()),
        ..Default::default()
    });
    let index = write_and_snapshot(&tb, 1, 1);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let bs = tb.client(1).admin_status().unwrap().backup.unwrap();
    assert_eq!(bs.failures_total, 1, "{bs:?}");
    assert!(bs.last_backup_error.contains("500"), "{bs:?}");
    assert_eq!(metas(&f, &cluster), good, "no new commit");
    let orphan = f
        .keys(BUCKET)
        .into_iter()
        .find(|k| k.ends_with(&format!("-{index}.redb")))
        .expect("the data landed");
    let name = orphan.rsplit('/').next().unwrap().to_string();
    let root = tempfile::tempdir().unwrap();
    let e = restore_s3(&f, &cluster, &name, &root.path().join("a"), &chk).unwrap_err();
    assert!(e.to_string().contains("not a committed backup"), "{e}");
    assert!(!root.path().join("a/graph.redb.restore.tmp").exists());
    // `latest` ignores it and restores the committed one.
    let side = restore_s3(&f, &cluster, "latest", &root.path().join("b"), &chk);
    assert!(side.is_ok(), "{side:?}");
    // The connection dropped mid-data: nothing of that snapshot lands.
    f.set_faults(Faults {
        drop_after_bytes: Some(700),
        ..Default::default()
    });
    let index = write_and_snapshot(&tb, 1, 2);
    tb.wait_backups_idle(CLUSTER_WAIT);
    f.clear_faults();
    assert!(!f
        .keys(BUCKET)
        .iter()
        .any(|k| k.contains(&format!("-{index}."))));
    assert_eq!(f.pending_uploads(), 0);
}

/// One corrupt byte in the data object is refused, nothing left behind.
#[test]
fn a_corrupt_byte_is_refused() {
    let _w = watchdog("s3 a_corrupt_byte_is_refused", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 7, f.options()));
    let index = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let chk = checks(&f, &cluster);
    let key = data_key(&f, &cluster, index);
    assert!(f.corrupt(BUCKET, &key));
    let root = tempfile::tempdir().unwrap();
    for name in [key.rsplit('/').next().unwrap(), "latest"] {
        let to = root.path().join(name.replace('.', "_"));
        let e = restore_s3(&f, &cluster, name, &to, &chk).unwrap_err();
        assert!(e.to_string().contains("sha256"), "{e}");
        assert!(!to.join("graph.redb").exists() && !to.join("graph.redb.restore.tmp").exists());
    }
}

/// Retention keeps the newest N; another cluster's prefix and objects
/// outside the sink's prefix are untouched.
#[test]
fn retention_keeps_n_and_leaves_other_prefixes() {
    let _w = watchdog("s3 retention", TEST_LIMIT);
    let f = FakeS3::start();
    f.put_object(
        BUCKET,
        &format!("{PREFIX}/other-cluster/snap-1-1.meta"),
        b"{}",
    );
    f.put_object(
        BUCKET,
        &format!("{PREFIX}/other-cluster/snap-1-1.redb"),
        b"x",
    );
    f.put_object(BUCKET, "unrelated/snap-1-1.redb", b"x");
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 2, f.options()));
    let mut indexes = Vec::new();
    for i in 0..4 {
        indexes.push(write_and_snapshot(&tb, 1, i));
        tb.wait_backups_idle(CLUSTER_WAIT);
    }
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let left = metas(&f, &cluster);
    assert_eq!(left.len(), 2, "{left:?}");
    for i in &indexes[2..] {
        assert!(
            left.iter().any(|k| k.ends_with(&format!("-{i}.meta"))),
            "{left:?}"
        );
    }
    let mine = f
        .keys(BUCKET)
        .into_iter()
        .filter(|k| k.starts_with(&format!("{PREFIX}/{cluster}/")))
        .count();
    assert_eq!(mine, 4, "two pairs, no orphans");
    for k in [
        format!("{PREFIX}/other-cluster/snap-1-1.meta"),
        format!("{PREFIX}/other-cluster/snap-1-1.redb"),
        "unrelated/snap-1-1.redb".to_string(),
    ] {
        assert!(f.object(BUCKET, &k).is_some(), "{k} removed");
    }
}

/// An old orphan (data without a `.meta`) is swept; a young one is kept.
#[test]
fn old_orphans_are_swept() {
    let _w = watchdog("s3 orphans", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 7, f.options()));
    write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let (old, young) = (
        format!("{PREFIX}/{cluster}/snap-1-99998.redb"),
        format!("{PREFIX}/{cluster}/snap-1-99999.redb"),
    );
    f.put_object(BUCKET, &old, b"x");
    f.put_object(BUCKET, &young, b"x");
    f.set_modified(
        BUCKET,
        &old,
        SystemTime::now() - Duration::from_secs(48 * 3600),
    );
    write_and_snapshot(&tb, 1, 1);
    tb.wait_backups_idle(CLUSTER_WAIT);
    assert!(f.object(BUCKET, &old).is_none(), "the old orphan is swept");
    assert!(f.object(BUCKET, &young).is_some(), "the young one stays");
}

/// A failing (403) or hung (slow past the timeout) S3 never blocks
/// snapshots and purges; failures are counted and reported.
#[test]
fn a_failing_s3_never_blocks_purge() {
    let _w = watchdog("s3 a_failing_s3_never_blocks_purge", TEST_LIMIT);
    let snappy = RaftSettings {
        snapshot_log_entries: 5,
        log_keep_entries: 2,
        purge_batch_size: 1,
        ..TEST_RAFT
    };
    for fault in ["403", "slow"] {
        let f = FakeS3::start();
        f.set_faults(if fault == "403" {
            Faults {
                forbidden: true,
                ..Default::default()
            }
        } else {
            Faults {
                delay: Some(Duration::from_secs(3600)),
                ..Default::default()
            }
        });
        let b = s3_cfg(
            &f,
            7,
            S3Options {
                timeout: Duration::from_millis(200),
                ..f.options()
            },
        );
        let tb = ClusterTestbed::with_config(1, exts(), |_, c| {
            c.raft = Some(snappy);
            c.backup = Some(b.clone());
        });
        let c = tb.client(1);
        for i in 0..40 {
            let fl = small_file(i);
            c.index_bytes("o", "r", &fl.0, &fl.1, None).unwrap();
        }
        let raft = tb.node(1).raft().unwrap();
        let deadline = Instant::now() + CLUSTER_WAIT;
        loop {
            let m = raft.metrics();
            if raft.snapshots.built() >= 3 && m.purged.map_or(0, |p| p.index) > 20 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{fault}: snapshots {} purged {:?}",
                raft.snapshots.built(),
                m.purged
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        tb.wait_backups_idle(CLUSTER_WAIT);
        let st = c.admin_status().unwrap().backup.unwrap();
        assert!(st.failures_total >= 1, "{fault}: {st:?}");
        let want = if fault == "403" {
            "AccessDenied"
        } else {
            "no progress"
        };
        assert!(st.last_backup_error.contains(want), "{fault}: {st:?}");
        assert_eq!(st.last_index, 0);
    }
}

/// `latest` picks the highest committed index and ignores a higher orphan.
#[test]
fn latest_picks_the_highest_committed() {
    let _w = watchdog("s3 latest", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 0, f.options()));
    let _first = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let second = write_and_snapshot(&tb, 1, 1);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    f.put_object(
        BUCKET,
        &format!("{PREFIX}/{cluster}/snap-9-99999.redb"),
        b"orphan",
    );
    let chk = checks(&f, &cluster);
    let root = tempfile::tempdir().unwrap();
    let sink = f.sink(&format!("s3://{BUCKET}/{PREFIX}"));
    let store = root.path().join("graph.redb");
    let side = restore_from_sink(&sink, &format!("{cluster}/"), "latest", &store, &chk).unwrap();
    assert_eq!(side.index, second);
    let s =
        TestServer::try_start_config(restore_cfg(&root.path().join("to"), &store), exts()).unwrap();
    let r = RemoteStore::connect(ClientConfig::new(s.endpoint())).unwrap();
    assert_eq!(r.count_nodes(graph_core::NodeKind::File).unwrap(), 2);
}

/// Only the leader uploads, and leadership moving moves the uploads.
#[test]
fn only_the_leader_uploads_across_a_transfer() {
    let _w = watchdog("s3 only_the_leader_uploads", TEST_LIMIT);
    let f = FakeS3::start();
    let mut b = s3_cfg(&f, 0, f.options());
    b.on = BackupOn::Leader;
    let mut tb = ClusterTestbed::with_backup(3, exts(), b);
    tb.form();
    let uploads =
        |tb: &ClusterTestbed, id: u64| tb.node(id).backup().unwrap().stats().uploads_total;
    let first = tb.leader();
    let mut i = 0;
    for id in tb.ids() {
        write_and_snapshot(&tb, id, i);
        i += 1;
    }
    tb.wait_backups_idle(CLUSTER_WAIT);
    for id in tb.ids() {
        assert_eq!(uploads(&tb, id), u64::from(id == first), "node {id}");
    }
    let target = tb.ids().into_iter().find(|id| *id != first).unwrap();
    tb.client(first).admin_transfer_leader(target).unwrap();
    let deadline = Instant::now() + CLUSTER_WAIT;
    while tb.leader() != target {
        assert!(Instant::now() < deadline, "transfer to {target}");
        std::thread::sleep(Duration::from_millis(20));
    }
    for id in tb.ids() {
        write_and_snapshot(&tb, id, i);
        i += 1;
    }
    tb.wait_backups_idle(CLUSTER_WAIT);
    assert_eq!(uploads(&tb, first), 1);
    assert_eq!(uploads(&tb, target), 1);
    let cluster = tb.client(target).admin_status().unwrap().cluster_id;
    assert_eq!(metas(&f, &cluster).len(), 2);
}

/// One kept-alive connection serves a whole multipart upload (no
/// handshake per part, no ephemeral-port churn).
#[test]
fn a_connection_is_reused() {
    let f = FakeS3::start();
    let s = f.sink_with(&format!("s3://{BUCKET}"), small_parts(&f));
    let body = bytes(20_000, 9);
    s.put("k", &mut body.as_slice()).unwrap();
    assert_eq!(get(&s, "k").unwrap(), body);
    assert!(f.requests().len() >= 8, "{:?}", f.requests());
    assert_eq!(f.connections(), 1);
    // A dropped connection is replaced transparently.
    f.set_faults(Faults {
        drop_after_bytes: Some(10),
        ..Default::default()
    });
    assert!(s.put("x", &mut &b"0123456789abcdef"[..]).is_err());
    f.clear_faults();
    s.put("x", &mut &b"ok"[..]).unwrap();
    assert_eq!(f.object(BUCKET, "x").unwrap(), b"ok");
}

/// Retention aborts multipart uploads under this cluster's prefix begun
/// before the orphan age; a young one and another cluster's stay.
#[test]
fn stale_multipart_uploads_are_aborted() {
    let _w = watchdog("s3 stale multipart", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 7, f.options()));
    write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let old = SystemTime::now() - Duration::from_secs(48 * 3600);
    let stale = format!("{PREFIX}/{cluster}/snap-1-7.redb");
    let young = format!("{PREFIX}/{cluster}/snap-1-8.redb");
    let other = format!("{PREFIX}/other-cluster/snap-1-7.redb");
    f.begin_upload(BUCKET, &stale, old);
    f.begin_upload(BUCKET, &young, SystemTime::now());
    f.begin_upload(BUCKET, &other, old);
    write_and_snapshot(&tb, 1, 1);
    tb.wait_backups_idle(CLUSTER_WAIT);
    assert_eq!(f.upload_keys(), {
        let mut want = vec![format!("{BUCKET}/{other}"), format!("{BUCKET}/{young}")];
        want.sort();
        want
    });
}

/// A 403 is not retried: one request per snapshot, counted at once.
#[test]
fn a_403_is_not_retried() {
    let _w = watchdog("s3 403 not retried", TEST_LIMIT);
    let f = FakeS3::start();
    f.set_faults(Faults {
        forbidden: true,
        ..Default::default()
    });
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, 7, f.options()));
    write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let st = tb.client(1).admin_status().unwrap().backup.unwrap();
    assert_eq!(st.failures_total, 1, "{st:?}");
    assert_eq!(f.requests().len(), 1, "{:?}", f.requests());
}
