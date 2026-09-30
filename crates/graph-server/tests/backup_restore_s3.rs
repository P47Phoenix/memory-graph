//! Epic story 37 (ADR 0006 E1, E8, E10) against the in-process `FakeS3`:
//! `serve --bootstrap --restore s3://.../snap-T-I.redb` and `.../latest`
//! through the real S3 client (settings passed into the restore path, keys
//! from a credentials file), refusals that leave nothing behind, `latest`
//! past orphans and damaged pairs, `Admin.UploadSnapshot` (`cluster
//! snapshot --upload`) forwarded by a follower, and `Admin.ListBackups`
//! (`cluster backups`) listing exactly the committed pairs.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::backup::{BackupConfig, BackupOn, S3Options};
use graph_server::testing::{ClusterTestbed, FakeS3, TestServer, CLUSTER_WAIT};
use graph_server::{InitMode, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::{Store, StoreError, StoreRead};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const BUCKET: &str = "backups";
const PREFIX: &str = "mg";

fn s3_cfg(f: &FakeS3, on: BackupOn) -> BackupConfig {
    let url = format!("s3://{BUCKET}/{PREFIX}");
    let mut b = BackupConfig::new(url.clone());
    b.keep = 0;
    b.on = on;
    b.retry_backoff = Duration::from_millis(10);
    b.sink = Some(Arc::new(f.sink(&url)));
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

fn meta_at(f: &FakeS3, cluster: &str, index: u64) -> String {
    metas(f, cluster)
        .into_iter()
        .find(|k| k.ends_with(&format!("-{index}.meta")))
        .unwrap_or_else(|| panic!("no committed backup at {index}"))
}

/// The S3 settings of a restore, as `serve` passes them: the fake's
/// endpoint and region, the keys from a credentials file (never the
/// environment).
fn restore_opts(f: &FakeS3, dir: &Path) -> S3Options {
    std::fs::create_dir_all(dir).unwrap();
    let creds = dir.join("aws-credentials");
    f.write_credentials(&creds);
    S3Options {
        credentials_file: Some(creds),
        ..f.options()
    }
}

/// `serve --data-dir <dir> --bootstrap --restore <url>`.
fn restore(
    f: &FakeS3,
    root: &Path,
    dir: &Path,
    url: &str,
    allow_extractor_mismatch: bool,
) -> Result<TestServer, StoreError> {
    let mut c = ServeConfig::for_data_dir(
        dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap {
            restore: Some(PathBuf::from(url)),
        },
        Some(1),
    );
    c.restore_s3 = restore_opts(f, &root.join("creds"));
    c.restore_allow_extractor_mismatch = allow_extractor_mismatch;
    TestServer::try_start_config(c, exts())
}

fn url(cluster: &str, name: &str) -> String {
    format!("s3://{BUCKET}/{PREFIX}/{cluster}/{name}")
}

/// Nothing of a refused restore is left: no store, no `.restore.tmp`.
fn assert_nothing_left(dir: &Path) {
    let left: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !left.iter().any(|n| n.starts_with("graph.redb")),
        "left behind in {}: {left:?}",
        dir.display()
    );
}

/// `r` without the entry at `index`.
fn pb_without(
    mut r: graph_proto::pb::ListBackupsResponse,
    index: u64,
) -> graph_proto::pb::ListBackupsResponse {
    r.backups.retain(|b| b.index != index);
    r
}

fn files(s: &TestServer) -> usize {
    RemoteStore::connect(ClientConfig::new(s.endpoint()))
        .unwrap()
        .count_nodes(graph_core::NodeKind::File)
        .unwrap()
}

/// `--restore s3://.../snap-T-I.redb` and `.../latest` equal the source.
#[test]
fn restore_from_s3_equals_the_source() {
    let _w = watchdog("restore_from_s3_equals_the_source", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, BackupOn::Leader));
    let c = tb.client(1);
    for i in 0..12 {
        let fl = small_file(i);
        c.index_bytes("o", "r", &fl.0, &fl.1, None).unwrap();
    }
    let index = write_and_snapshot(&tb, 1, 100);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = c.admin_status().unwrap().cluster_id;
    let meta = meta_at(&f, &cluster, index);
    let name = meta.rsplit('/').next().unwrap().replace(".meta", ".redb");
    let root = tempfile::tempdir().unwrap();
    for (i, n) in [name.as_str(), "latest"].into_iter().enumerate() {
        let dir = root.path().join(format!("to{i}"));
        let s = restore(&f, root.path(), &dir, &url(&cluster, n), false)
            .unwrap_or_else(|e| panic!("{n}: {e}"));
        let r = RemoteStore::connect(ClientConfig::new(s.endpoint())).unwrap();
        run_differential(&replica(&tb, 1), &r);
        let st = r.admin_status().unwrap();
        assert_ne!(st.cluster_id, cluster, "a restore gets a new cluster id");
    }
}

/// A size, sha256, store format or extractors mismatch is refused before
/// anything is placed, with nothing left behind; the extractors check
/// alone has an override.
#[test]
fn a_mismatch_is_refused_with_nothing_left_behind() {
    let _w = watchdog("a_mismatch_is_refused", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, BackupOn::Leader));
    let index = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let meta_key = meta_at(&f, &cluster, index);
    let data_key = meta_key.replace(".meta", ".redb");
    let name = data_key.rsplit('/').next().unwrap().to_string();
    let good_meta = f.object(BUCKET, &meta_key).unwrap();
    let side: serde_json::Value = serde_json::from_slice(&good_meta).unwrap();
    let root = tempfile::tempdir().unwrap();
    type Tamper = fn(&mut serde_json::Value);
    let cases: [(&str, Tamper, &str); 4] = [
        (
            "size",
            |v| v["size"] = (v["size"].as_u64().unwrap() + 1).into(),
            "sha256",
        ),
        ("sha256", |v| v["sha256"] = "00".repeat(32).into(), "sha256"),
        (
            "format",
            |v| {
                v["store_format_version"] = (v["store_format_version"].as_u64().unwrap() + 1).into()
            },
            "store format",
        ),
        (
            "extractors",
            |v| v["extractors_hash"] = "some-other-extractors".into(),
            "--restore-allow-extractor-mismatch",
        ),
    ];
    for (what, tamper, want) in cases {
        let mut v = side.clone();
        tamper(&mut v);
        f.replace(BUCKET, &meta_key, serde_json::to_vec(&v).unwrap());
        let dir = root.path().join(what);
        let e = restore(&f, root.path(), &dir, &url(&cluster, &name), false)
            .err()
            .unwrap_or_else(|| panic!("{what}: a mismatch was restored"));
        assert!(e.to_string().contains(want), "{what}: {e}");
        assert_nothing_left(&dir);
        if what == "extractors" {
            // The override: the store is valid, its files re-extract later.
            let s = restore(&f, root.path(), &dir, &url(&cluster, &name), true).unwrap();
            assert_eq!(files(&s), 1);
        }
    }
    // A corrupt byte in the data (the .meta intact again).
    f.replace(BUCKET, &meta_key, good_meta);
    assert!(f.corrupt(BUCKET, &data_key));
    for n in [name.as_str(), "latest"] {
        let dir = root.path().join(format!("corrupt-{n}"));
        let e = restore(&f, root.path(), &dir, &url(&cluster, n), false)
            .err()
            .unwrap();
        assert!(e.to_string().contains("sha256"), "{n}: {e}");
        assert_nothing_left(&dir);
    }
    // A name that is no backup, and a URL naming none.
    let dir = root.path().join("missing");
    let e = restore(
        &f,
        root.path(),
        &dir,
        &url(&cluster, "snap-9-9.redb"),
        false,
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("not a committed backup"), "{e}");
    assert_nothing_left(&dir);
    let e = restore(
        &f,
        root.path(),
        &root.path().join("bare"),
        &format!("s3://{BUCKET}/"),
        false,
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("names no snapshot"), "{e}");
}

/// `latest` picks the highest committed index that verifies: it ignores a
/// higher orphan (data without a `.meta`), a higher `.meta` whose data is
/// missing, and a higher pair whose data is damaged.
#[test]
fn latest_ignores_orphans_and_damaged_pairs() {
    let _w = watchdog("latest_ignores_orphans_and_damaged_pairs", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, BackupOn::Leader));
    let _a = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let _b = write_and_snapshot(&tb, 1, 1);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let c = write_and_snapshot(&tb, 1, 2);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let c_meta = meta_at(&f, &cluster, c);
    assert!(f.corrupt(BUCKET, &c_meta.replace(".meta", ".redb")));
    let dir_key = format!("{PREFIX}/{cluster}");
    f.put_object(BUCKET, &format!("{dir_key}/snap-9-99999.redb"), b"orphan");
    f.put_object(
        BUCKET,
        &format!("{dir_key}/snap-9-99998.meta"),
        &f.object(BUCKET, &c_meta).unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let s = restore(
        &f,
        root.path(),
        &root.path().join("to"),
        &url(&cluster, "latest"),
        false,
    )
    .unwrap();
    // The second snapshot: two files (the third is the damaged one).
    assert_eq!(files(&s), 2);
}

/// `cluster snapshot --upload` sent to a follower is forwarded: the leader
/// builds and uploads (under `--backup-on none` too) and answers the
/// committed backup's URL and sha256, which restores.
#[test]
fn upload_on_a_follower_is_forwarded() {
    let _w = watchdog("upload_on_a_follower_is_forwarded", TEST_LIMIT);
    let f = FakeS3::start();
    let mut tb = ClusterTestbed::with_backup(3, exts(), s3_cfg(&f, BackupOn::None));
    tb.form();
    let leader = tb.leader();
    let follower = tb.ids().into_iter().find(|id| *id != leader).unwrap();
    for i in 0..3 {
        let fl = small_file(i);
        tb.client(leader)
            .index_bytes("o", "r", &fl.0, &fl.1, None)
            .unwrap();
    }
    let cluster = tb.client(leader).admin_status().unwrap().cluster_id;
    assert!(
        metas(&f, &cluster).is_empty(),
        "--backup-on none uploads nothing by itself"
    );
    let fc = tb.client(follower);
    let before = fc.admin_status().unwrap().writes_forwarded_total;
    let r = fc.admin_upload_snapshot().unwrap();
    assert_eq!(r.node_id, leader, "the leader uploaded");
    assert!(fc.admin_status().unwrap().writes_forwarded_total > before);
    let b = r.backup.unwrap();
    let meta_key = meta_at(&f, &cluster, b.index);
    assert_eq!(metas(&f, &cluster), std::slice::from_ref(&meta_key));
    assert_eq!(
        b.url,
        format!("s3://{BUCKET}/{}", meta_key.replace(".meta", ".redb"))
    );
    let side: serde_json::Value =
        serde_json::from_slice(&f.object(BUCKET, &meta_key).unwrap()).unwrap();
    assert_eq!(side["sha256"].as_str().unwrap(), b.sha256);
    assert_eq!(side["size"].as_u64().unwrap(), b.size);
    let data = f
        .object(BUCKET, &meta_key.replace(".meta", ".redb"))
        .unwrap();
    assert_eq!(data.len() as u64, b.size);
    // A second upload with nothing new: the committed pair counts as done.
    let again = tb.client(leader).admin_upload_snapshot().unwrap();
    assert!(again.backup.unwrap().index >= b.index);
    // What it answered restores.
    let root = tempfile::tempdir().unwrap();
    let s = restore(&f, root.path(), &root.path().join("to"), &b.url, false).unwrap();
    assert_eq!(files(&s), 3);
    // `cluster backups` from the follower lists it too.
    let listed = fc.admin_list_backups().unwrap();
    assert!(listed.backups.iter().any(|e| e.url == b.url), "{listed:?}");
}

/// Without `--backup-url`, both RPCs are refused with a clear message.
#[test]
fn no_backup_location_is_refused() {
    let _w = watchdog("no_backup_location_is_refused", TEST_LIMIT);
    let tb = ClusterTestbed::new(1, exts());
    let c = tb.client(1);
    for e in [
        c.admin_upload_snapshot().err().unwrap().to_string(),
        c.admin_list_backups().err().unwrap().to_string(),
    ] {
        assert!(e.contains("no backup location"), "{e}");
    }
}

/// `cluster backups` lists exactly the committed pairs of this cluster,
/// newest first: not an orphan, not another cluster's, not a nested key;
/// an unreadable `.meta` is listed with its error.
#[test]
fn backups_lists_exactly_the_committed_pairs() {
    let _w = watchdog("backups_lists_exactly_the_committed_pairs", TEST_LIMIT);
    let f = FakeS3::start();
    let tb = ClusterTestbed::with_backup(1, exts(), s3_cfg(&f, BackupOn::Leader));
    let mut indexes = Vec::new();
    for i in 0..3 {
        indexes.push(write_and_snapshot(&tb, 1, i));
        tb.wait_backups_idle(CLUSTER_WAIT);
    }
    let c = tb.client(1);
    let cluster = c.admin_status().unwrap().cluster_id;
    let dir_key = format!("{PREFIX}/{cluster}");
    f.put_object(BUCKET, &format!("{dir_key}/snap-9-99999.redb"), b"orphan");
    f.put_object(BUCKET, &format!("{dir_key}/nested/snap-9-5.meta"), b"{}");
    f.put_object(BUCKET, &format!("{PREFIX}/other/snap-1-1.meta"), b"{}");
    f.put_object(BUCKET, &format!("{dir_key}/snap-7-1.meta"), b"not json");
    // A `.meta` whose data object is gone: listed, marked.
    let first_meta = meta_at(&f, &cluster, indexes[0]);
    f.put_object(
        BUCKET,
        &format!("{dir_key}/snap-8-99.meta"),
        &f.object(BUCKET, &first_meta).unwrap(),
    );
    let r = c.admin_list_backups().unwrap();
    let orphan_meta = r
        .backups
        .iter()
        .find(|b| b.index == 99)
        .expect("the data-less .meta is listed");
    assert!(orphan_meta.error.contains("is missing"), "{orphan_meta:?}");
    let r = pb_without(r, 99);
    assert_eq!(r.cluster_id, cluster);
    assert!(
        r.location.starts_with(&format!("s3://{BUCKET}/{PREFIX}")),
        "{}",
        r.location
    );
    let mut want: Vec<u64> = indexes.clone();
    want.push(1);
    want.sort_unstable_by(|a, b| b.cmp(a));
    let got: Vec<u64> = r.backups.iter().map(|b| b.index).collect();
    assert_eq!(got, want, "{r:?}");
    for b in &r.backups {
        if b.index == 1 {
            assert!(b.error.contains("not a snapshot meta"), "{b:?}");
            continue;
        }
        assert!(b.error.is_empty(), "{b:?}");
        let meta_key = meta_at(&f, &cluster, b.index);
        let side: serde_json::Value =
            serde_json::from_slice(&f.object(BUCKET, &meta_key).unwrap()).unwrap();
        assert_eq!(side["sha256"].as_str().unwrap(), b.sha256);
        assert_eq!(
            b.url,
            format!("s3://{BUCKET}/{}", meta_key.replace(".meta", ".redb"))
        );
    }
}
