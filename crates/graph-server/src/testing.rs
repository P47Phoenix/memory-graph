//! [`TestServer`]: an in-process server for other crates' tests, on
//! `127.0.0.1:0`, with its own runtime so a synchronous test can drive it.
use crate::extractors::share;
use crate::server::{start, Running, ServeConfig};
use graph_core::Extractor;
use graph_store::StoreError;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub mod cluster;
pub mod fake_s3;
pub mod mcp_http;
pub use cluster::{ClusterTestbed, TestNode, CLUSTER_WAIT, TEST_RAFT};
pub use fake_s3::{FakeS3, Faults};

/// Make the snapshot pair at `data` (a `snap-*.redb` with its `.meta`)
/// look like one an older build made (#151): stamp the store file's schema
/// version `format` (when given, an upgradable older one) and record it,
/// the new size and digest, and `extractors_hash` (when given) in the
/// sidecar. The file must not be open.
pub fn age_snapshot(
    data: &Path,
    format: Option<u64>,
    extractors_hash: Option<&str>,
) -> Result<(), StoreError> {
    use crate::raft::snapshot_dir::{meta_path_of, read_sidecar, sha256_file};
    let st = |e: &dyn std::fmt::Display| StoreError::Storage(e.to_string());
    let mut side = read_sidecar(data)?;
    if let Some(v) = format {
        const META: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("meta");
        let db = redb::Database::open(data).map_err(|e| st(&e))?;
        let wt = db.begin_write().map_err(|e| st(&e))?;
        {
            let mut t = wt.open_table(META).map_err(|e| st(&e))?;
            t.insert("schema_version", v).map_err(|e| st(&e))?;
        }
        wt.commit().map_err(|e| st(&e))?;
        side.store_format_version = v;
    }
    if let Some(h) = extractors_hash {
        side.extractors_hash = h.to_string();
    }
    let (sha256, size) = sha256_file(data)?;
    side.sha256 = sha256;
    side.size = size;
    let json = serde_json::to_string_pretty(&side).map_err(|e| st(&e))?;
    std::fs::write(meta_path_of(data), json).map_err(|e| st(&e))
}

pub struct TestServer {
    db: PathBuf,
    extractors: Vec<Arc<dyn Extractor>>,
    cfg: ServeConfig,
    rt: tokio::runtime::Runtime,
    running: Option<Running>,
    addr: SocketAddr,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime")
}

/// A single-voter server leads before `start` returns; wait, bounded, until
/// its metrics name the leader too, so a test's first `Hello` sees it.
/// Other nodes (members of a cluster, a join) are left alone.
async fn leader_known(running: &Running) {
    let raft = &running.raft;
    let me = raft.node_id;
    let m = raft.metrics();
    if m.membership_config
        .membership()
        .voter_ids()
        .collect::<Vec<_>>()
        != [me]
    {
        return;
    }
    let _ = raft
        .raft
        .wait(Some(Duration::from_secs(10)))
        .metrics(|m| m.current_leader == Some(me), "a known leader")
        .await;
}

impl TestServer {
    /// Start on a free port with these extractors; panics on failure.
    pub fn start(db: &Path, extractors: Vec<Box<dyn Extractor>>) -> TestServer {
        Self::try_start_with(db, extractors, |_| {}).expect("test server starts")
    }

    /// `start` with a hook to adjust the config (snapshot max age, node id).
    pub fn start_with(
        db: &Path,
        extractors: Vec<Box<dyn Extractor>>,
        tweak: impl FnOnce(&mut ServeConfig),
    ) -> TestServer {
        Self::try_start_with(db, extractors, tweak).expect("test server starts")
    }

    /// Start with a complete configuration (a `--data-dir` node, say);
    /// `db()` then answers the store path the configuration resolves to.
    pub fn try_start_config(
        cfg: ServeConfig,
        extractors: Vec<Box<dyn Extractor>>,
    ) -> Result<TestServer, StoreError> {
        let extractors = share(extractors);
        let rt = runtime();
        let running = rt.block_on(start(cfg.clone(), extractors.clone()))?;
        rt.block_on(leader_known(&running));
        let addr = running.addr;
        Ok(TestServer {
            db: running.paths.store.clone(),
            extractors,
            cfg,
            rt,
            running: Some(running),
            addr,
        })
    }

    /// Fallible `start_with` (a second server on the same file is `Locked`).
    pub fn try_start_with(
        db: &Path,
        extractors: Vec<Box<dyn Extractor>>,
        tweak: impl FnOnce(&mut ServeConfig),
    ) -> Result<TestServer, StoreError> {
        let mut cfg = ServeConfig::new(db, "127.0.0.1:0".parse().unwrap());
        cfg.shutdown_grace = Duration::from_secs(5);
        tweak(&mut cfg);
        let extractors = share(extractors);
        let rt = runtime();
        let running = rt.block_on(start(cfg.clone(), extractors.clone()))?;
        rt.block_on(leader_known(&running));
        let addr = running.addr;
        Ok(TestServer {
            db: db.to_path_buf(),
            extractors,
            cfg,
            rt,
            running: Some(running),
            addr,
        })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `host:port` for a client endpoint list.
    pub fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    pub fn db(&self) -> &Path {
        &self.db
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn running(&self) -> Option<&Running> {
        self.running.as_ref()
    }

    /// Graceful stop; waits until the store is closed and the LOCK removed.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.shutdown();
            if let Err(e) = self.rt.block_on(r.wait()) {
                eprintln!("test server stop: {e}");
            }
        }
    }

    /// Stop, then start again on the same address with the same
    /// extractors (a client's endpoint stays valid). Falls back to a free
    /// port if the old one cannot be rebound in time.
    pub fn restart(&mut self) {
        self.stop();
        let mut cfg = self.cfg.clone();
        cfg.listen = self.addr;
        let mut last = None;
        for _ in 0..20 {
            match self
                .rt
                .block_on(start(cfg.clone(), self.extractors.clone()))
            {
                Ok(r) => {
                    self.addr = r.addr;
                    self.running = Some(r);
                    return;
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let e = last.expect("an error");
        if e.to_string().contains("cannot listen") {
            cfg.listen = "127.0.0.1:0".parse().unwrap();
            let r = self
                .rt
                .block_on(start(cfg, self.extractors.clone()))
                .expect("test server restarts");
            self.addr = r.addr;
            self.running = Some(r);
        } else {
            panic!("test server restart: {e}");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}
