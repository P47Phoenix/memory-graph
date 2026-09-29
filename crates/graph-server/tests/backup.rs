//! Snapshot backups (ADR 0006, epic story 35): the `file://` sink, the
//! leader-only uploader, retention, and verified restores, through the
//! in-process `ClusterTestbed`.
mod support;
use support::*;

use graph_client::{ClientConfig, RemoteStore};
use graph_server::backup::{BackupConfig, BackupOn, BackupSink, FileSink, ObjectInfo};
use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT, TEST_RAFT};
use graph_server::{InitMode, RaftSettings, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::Store;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display()).replace('\\', "/")
}

fn cfg(url: String, keep: usize) -> BackupConfig {
    let mut b = BackupConfig::new(url);
    b.keep = keep;
    b.retry_backoff = Duration::from_millis(10);
    b
}

/// Write one small file through the leader, then build a snapshot on
/// `node` (a new index, so a new pair).
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

/// The committed backups (`.meta` keys) under `<dir>/<cluster>/`.
fn metas(dir: &Path, cluster: &str) -> Vec<String> {
    FileSink::new(dir)
        .list(&format!("{cluster}/"))
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .filter(|k| k.ends_with(".meta"))
        .collect()
}

/// The committed data object of `index` under `<dir>/<cluster>/`.
fn data_of(dir: &Path, cluster: &str, index: u64) -> PathBuf {
    let key = metas(dir, cluster)
        .into_iter()
        .find(|k| k.ends_with(&format!("-{index}.meta")))
        .unwrap_or_else(|| panic!("no committed backup at {index}"));
    let mut p = dir.to_path_buf();
    p.extend(key.split('/'));
    p.with_extension("redb")
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

/// The leader writes data first, `.meta` last, under `<cluster_id>/`; a
/// new cluster restored from `latest` answers exactly like the source.
#[test]
fn upload_then_restore_from_latest_equals_the_source() {
    let _w = watchdog("upload_then_restore", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("backups");
    let mut tb = ClusterTestbed::with_backup(3, exts(), cfg(file_url(&dir), 7));
    tb.form();
    let leader = tb.leader();
    let c = tb.client(leader);
    for i in 0..12 {
        let f = small_file(i);
        c.index_bytes("o", "r", &f.0, &f.1, None).unwrap();
    }
    let index = write_and_snapshot(&tb, leader, 100);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = c.admin_status().unwrap().cluster_id;
    let data = data_of(&dir, &cluster, index);
    assert!(data.exists(), "{} missing", data.display());
    assert!(data.with_extension("meta").exists());
    let st = c.admin_status().unwrap().backup.expect("backup status");
    assert_eq!(st.last_index, index);
    assert!(st.last_success_unix > 0 && st.bytes_total > 0);
    assert!(st.last_backup_error.is_empty());
    let m = c.admin_metrics().unwrap();
    assert!(m.contains(&format!("mg_backup_last_index {index}")), "{m}");
    // Restore `latest` into a new single-node cluster.
    let to = root.path().join("restored");
    let url = format!("{}/{cluster}/latest", file_url(&dir));
    let restored = TestServer::try_start_config(restore_cfg(&to, url), exts()).unwrap();
    let r = RemoteStore::connect(ClientConfig::new(restored.endpoint())).unwrap();
    assert_ne!(r.admin_status().unwrap().cluster_id, cluster);
    assert!(!to.join("graph.redb.restore.tmp").exists());
    run_differential(&replica(&tb, leader), &r);
}

/// Only the newest `keep` committed backups remain.
#[test]
fn retention_keeps_the_newest_n() {
    let _w = watchdog("retention_keeps_the_newest_n", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("b");
    // A sibling cluster's prefix that retention must leave alone.
    let other = dir.join("some-other-cluster");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("snap-1-1.meta"), b"{}").unwrap();
    std::fs::write(other.join("snap-1-1.redb"), b"x").unwrap();
    let tb = ClusterTestbed::with_backup(1, exts(), cfg(file_url(&dir), 2));
    let mut indexes = Vec::new();
    for i in 0..4 {
        indexes.push(write_and_snapshot(&tb, 1, i));
        tb.wait_backups_idle(CLUSTER_WAIT);
    }
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let left = metas(&dir, &cluster);
    assert_eq!(left.len(), 2, "{left:?}");
    for i in &indexes[2..] {
        assert!(
            left.iter().any(|k| k.ends_with(&format!("-{i}.meta"))),
            "{i} kept: {left:?}"
        );
    }
    let all = FileSink::new(&dir).list(&format!("{cluster}/")).unwrap();
    assert_eq!(all.len(), 4, "two pairs, no orphans: {all:?}");
    assert!(other.join("snap-1-1.meta").exists() && other.join("snap-1-1.redb").exists());
}

/// A sink that stores data but dies before any `.meta` lands (a kill mid
/// upload, between the data object and its commit).
struct DiesBeforeMeta(FileSink);

impl BackupSink for DiesBeforeMeta {
    fn put(&self, key: &str, src: &mut dyn Read) -> std::io::Result<u64> {
        if key.ends_with(".meta") {
            return Err(std::io::Error::other("killed"));
        }
        self.0.put(key, src)
    }
    fn get(&self, key: &str, dst: &mut dyn Write) -> std::io::Result<u64> {
        self.0.get(key, dst)
    }
    fn list(&self, prefix: &str) -> std::io::Result<Vec<ObjectInfo>> {
        self.0.list(prefix)
    }
    fn delete(&self, key: &str) -> std::io::Result<()> {
        self.0.delete(key)
    }
    fn describe(&self) -> String {
        self.0.describe()
    }
}

/// An interrupted upload is never restorable: its data has no `.meta`, so
/// `latest` finds nothing and naming it is refused, with nothing left.
#[test]
fn an_interrupted_upload_is_not_restorable() {
    let _w = watchdog("an_interrupted_upload_is_not_restorable", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("b");
    let mut b = cfg(file_url(&dir), 7);
    b.sink = Some(Arc::new(DiesBeforeMeta(FileSink::new(&dir))));
    let tb = ClusterTestbed::with_backup(1, exts(), b);
    let index = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let st = tb.client(1).admin_status().unwrap();
    let bs = st.backup.unwrap();
    assert_eq!(bs.failures_total, 1);
    assert!(bs.last_backup_error.contains("killed"), "{bs:?}");
    let cluster = st.cluster_id;
    let objects = FileSink::new(&dir).list("").unwrap();
    let landed = objects
        .iter()
        .find(|o| o.key.ends_with(&format!("-{index}.redb")))
        .unwrap_or_else(|| panic!("the data object landed: {objects:?}"));
    let name = landed.key.rsplit('/').next().unwrap().to_string();
    assert!(metas(&dir, &cluster).is_empty());
    let base = file_url(&dir.join(&cluster));
    for (n, url) in [format!("{base}/latest"), format!("{base}/{name}")]
        .into_iter()
        .enumerate()
    {
        let to = root.path().join(format!("r{n}"));
        let e = TestServer::try_start_config(restore_cfg(&to, &url), exts())
            .err()
            .unwrap_or_else(|| panic!("{url} restored"));
        assert!(e.to_string().contains("--restore"), "{e}");
        assert!(!to.join("graph.redb").exists() && !to.join("graph.redb.restore.tmp").exists());
    }
}

/// One corrupt byte in the data object: refused before `restore_into`,
/// nothing left behind. The same for a plain path with a sibling `.meta`;
/// a plain path without one restores (with a warning).
#[test]
fn a_corrupt_byte_is_refused() {
    let _w = watchdog("a_corrupt_byte_is_refused", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("b");
    let tb = ClusterTestbed::with_backup(1, exts(), cfg(file_url(&dir), 7));
    let index = write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let st = tb.client(1).admin_status().unwrap();
    let data = data_of(&dir, &st.cluster_id, index);
    // A good copy (plain path, with its .meta) restores.
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let p = plain.join("snap.redb");
    std::fs::copy(&data, &p).unwrap();
    std::fs::copy(data.with_extension("meta"), p.with_extension("meta")).unwrap();
    let ok = TestServer::try_start_config(restore_cfg(&root.path().join("ok"), &p), exts());
    assert!(ok.is_ok(), "{:?}", ok.err());
    drop(ok);
    // Flip one byte in the middle of each.
    for f in [&data, &p] {
        let mut bytes = std::fs::read(f).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        std::fs::write(f, bytes).unwrap();
    }
    for (n, src) in [PathBuf::from(file_url(&data)), p.clone()]
        .into_iter()
        .enumerate()
    {
        let to = root.path().join(format!("r{n}"));
        let e = TestServer::try_start_config(restore_cfg(&to, &src), exts())
            .err()
            .expect("a corrupt backup is refused");
        assert!(e.to_string().contains("sha256"), "{e}");
        assert!(!to.join("graph.redb").exists() && !to.join("graph.redb.restore.tmp").exists());
    }
    // Without a .meta, the (corrupt-free) plain file restores anyway.
    let bare = plain.join("bare.redb");
    tb.client(1).admin_trigger_snapshot(Some(&bare)).unwrap();
    let ok = TestServer::try_start_config(restore_cfg(&root.path().join("bare"), &bare), exts());
    assert!(ok.is_ok(), "{:?}", ok.err());
}

/// Other extractors are refused unless explicitly allowed.
#[test]
fn extractor_mismatch_is_refused_unless_allowed() {
    let _w = watchdog("extractor_mismatch", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("b");
    let tb = ClusterTestbed::with_backup(1, exts(), cfg(file_url(&dir), 7));
    write_and_snapshot(&tb, 1, 0);
    tb.wait_backups_idle(CLUSTER_WAIT);
    let cluster = tb.client(1).admin_status().unwrap().cluster_id;
    let url = format!("{}/{cluster}/latest", file_url(&dir));
    let to = root.path().join("r");
    let e = TestServer::try_start_config(restore_cfg(&to, &url), vec![])
        .err()
        .expect("other extractors are refused");
    assert!(e.to_string().contains("extractor"), "{e}");
    assert!(!to.join("graph.redb.restore.tmp").exists());
    let mut c = restore_cfg(&root.path().join("r2"), &url);
    c.restore_allow_extractor_mismatch = true;
    assert!(TestServer::try_start_config(c, vec![]).is_ok());
}

/// A sink whose every call hangs: snapshots and purges go on regardless.
struct Hangs;

impl BackupSink for Hangs {
    fn put(&self, _: &str, _: &mut dyn Read) -> std::io::Result<u64> {
        loop {
            std::thread::park();
        }
    }
    fn get(&self, _: &str, _: &mut dyn Write) -> std::io::Result<u64> {
        Err(std::io::Error::other("hangs"))
    }
    fn list(&self, _: &str) -> std::io::Result<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }
    fn delete(&self, _: &str) -> std::io::Result<()> {
        Ok(())
    }
    fn describe(&self) -> String {
        "hangs://".into()
    }
}

/// A failing (and a hung) sink never blocks or delays snapshots and
/// purges; the failures are counted and reported.
#[test]
fn a_failing_sink_never_blocks_purge() {
    let _w = watchdog("a_failing_sink_never_blocks_purge", TEST_LIMIT);
    let snappy = RaftSettings {
        snapshot_log_entries: 5,
        log_keep_entries: 2,
        purge_batch_size: 1,
        ..TEST_RAFT
    };
    let root = tempfile::tempdir().unwrap();
    // A `file://` "directory" that is a regular file: every put fails.
    let not_a_dir = root.path().join("file");
    std::fs::write(&not_a_dir, b"x").unwrap();
    for sink in ["broken", "hung"] {
        let mut b = cfg(file_url(&not_a_dir), 7);
        if sink == "hung" {
            b.sink = Some(Arc::new(Hangs));
        }
        let tb = ClusterTestbed::with_config(1, exts(), |_, c| {
            c.raft = Some(snappy);
            c.backup = Some(b.clone());
        });
        let c = tb.client(1);
        for i in 0..40 {
            let f = small_file(i);
            c.index_bytes("o", "r", &f.0, &f.1, None).unwrap();
        }
        let raft = tb.node(1).raft().unwrap();
        let deadline = std::time::Instant::now() + CLUSTER_WAIT;
        loop {
            let m = raft.metrics();
            if raft.snapshots.built() >= 3 && m.purged.map_or(0, |p| p.index) > 20 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{sink}: snapshots {} purged {:?}",
                raft.snapshots.built(),
                m.purged
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        if sink == "broken" {
            tb.wait_backups_idle(CLUSTER_WAIT);
            let st = c.admin_status().unwrap().backup.unwrap();
            assert!(st.failures_total >= 1, "{st:?}");
            assert!(!st.last_backup_error.is_empty());
            assert_eq!(st.last_index, 0);
            let m = c.admin_metrics().unwrap();
            assert!(!m.contains("mg_backup_failures_total 0"), "{m}");
        }
    }
}

/// Only the leader uploads, and leadership moving moves the uploads.
#[test]
fn only_the_leader_uploads_across_a_transfer() {
    let _w = watchdog("only_the_leader_uploads_across_a_transfer", TEST_LIMIT);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("b");
    let mut b = cfg(file_url(&dir), 0);
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
        let want = u64::from(id == first);
        assert_eq!(uploads(&tb, id), want, "node {id} (leader {first})");
    }
    let target = tb.ids().into_iter().find(|id| *id != first).unwrap();
    tb.client(first).admin_transfer_leader(target).unwrap();
    let deadline = std::time::Instant::now() + CLUSTER_WAIT;
    while tb.leader() != target {
        assert!(std::time::Instant::now() < deadline, "transfer to {target}");
        std::thread::sleep(Duration::from_millis(20));
    }
    for id in tb.ids() {
        write_and_snapshot(&tb, id, i);
        i += 1;
    }
    tb.wait_backups_idle(CLUSTER_WAIT);
    assert_eq!(uploads(&tb, first), 1, "the old leader uploads no more");
    assert_eq!(uploads(&tb, target), 1, "the new leader uploads");
    let third = tb
        .ids()
        .into_iter()
        .find(|id| *id != first && *id != target)
        .unwrap();
    assert_eq!(uploads(&tb, third), 0);
    let cluster = tb.client(target).admin_status().unwrap().cluster_id;
    assert_eq!(metas(&dir, &cluster).len(), 2);
}
