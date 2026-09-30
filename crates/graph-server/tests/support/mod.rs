//! Helpers shared by the cluster test binaries (`cluster.rs`, stage B;
//! `membership.rs`, stage C): extractors, corpus files, a replica as a
//! `Store` for the differential harness, summaries.
#![allow(dead_code)]
use graph_client::RemoteStore;
use graph_core::{Extraction, Extractor, Node, NodeId as GNodeId, NodeKind};
use graph_server::testing::{ClusterTestbed, CLUSTER_WAIT};
use graph_store::{
    open_store, BatchFile, Hit, IndexOptions, IngestStats, Page, PreparedFile, Query, RepoInfo,
    SnapshotStats, Store, StoreError, StoreRead, SymbolHit, SymbolQuery, VacuumStats,
    ORIGIN_DIRECTORY,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub type R<T> = Result<T, StoreError>;

pub fn exts() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(graph_lang_rust::RustExtractor),
        Box::new(graph_lang_csharp::CSharpExtractor),
        Box::new(graph_lang_javascript::JavaScriptExtractor),
    ]
}

pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

/// Every file under `dir`, sorted, as `(relative path, bytes)`.
pub fn files_under(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

pub fn index_files(s: &dyn Store, org: &str, repo: &str, files: &[(String, Vec<u8>)]) {
    for chunk in files.chunks(64) {
        let batch: Vec<BatchFile<'_>> = chunk
            .iter()
            .map(|(p, b)| BatchFile {
                path: p,
                bytes: b,
                language: None,
                origin: Some(ORIGIN_DIRECTORY),
                ..Default::default()
            })
            .collect();
        s.index_batch(org, repo, &batch, IndexOptions::default())
            .unwrap();
    }
}

/// Wait until node `c` (a client of it) has applied at least `index`.
pub fn wait_node_applied(c: &RemoteStore, index: u64) {
    let deadline = Instant::now() + CLUSTER_WAIT;
    loop {
        let st = c.admin_status().unwrap();
        if st.applied_index >= index {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node {} stuck at applied {} < {index}",
            st.node_id,
            st.applied_index
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One replica as a `Store` for the differential harness: reads go to
/// that node only (LOCAL), writes to the leader, and a write returns once
/// the replica has applied everything the leader had.
pub struct Replica {
    pub reads: RemoteStore,
    pub writes: RemoteStore,
}

impl Replica {
    fn synced<T>(&self, r: R<T>) -> R<T> {
        let leader_applied = self.writes.admin_status()?.applied_index;
        wait_node_applied(&self.reads, leader_applied);
        r
    }
}

impl StoreRead for Replica {
    fn get(&self, id: GNodeId) -> R<Option<Node>> {
        self.reads.get(id)
    }
    fn parent(&self, id: GNodeId) -> R<Option<Node>> {
        self.reads.parent(id)
    }
    fn count_nodes(&self, kind: NodeKind) -> R<usize> {
        self.reads.count_nodes(kind)
    }
    fn roots(&self) -> R<Vec<Node>> {
        self.reads.roots()
    }
    fn children(&self, id: GNodeId) -> R<Vec<Node>> {
        self.reads.children(id)
    }
    fn descendants(&self, id: GNodeId) -> R<Vec<Node>> {
        self.reads.descendants(id)
    }
    fn ancestors(&self, id: GNodeId) -> R<Vec<Node>> {
        self.reads.ancestors(id)
    }
    fn children_page(&self, id: GNodeId, offset: usize, limit: usize) -> R<Page<Node>> {
        self.reads.children_page(id, offset, limit)
    }
    fn descendants_page(&self, id: GNodeId, offset: usize, limit: usize) -> R<Page<Node>> {
        self.reads.descendants_page(id, offset, limit)
    }
    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> R<Option<Vec<Node>>> {
        self.reads.file_tokens(org, repo, path)
    }
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> R<Vec<RepoInfo>> {
        self.reads.describe(org, repo)
    }
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> R<Vec<RepoInfo>> {
        self.reads.describe_by_scan(org, repo)
    }
    fn search_symbols(&self, q: &SymbolQuery) -> R<Vec<SymbolHit>> {
        self.reads.search_symbols(q)
    }
    fn search(&self, q: &Query) -> R<Vec<Hit>> {
        self.reads.search(q)
    }
}

impl Store for Replica {
    fn snapshot(&self) -> R<Box<dyn StoreRead + Send + '_>> {
        self.reads.snapshot()
    }
    fn snapshot_stats(&self) -> SnapshotStats {
        self.reads.snapshot_stats()
    }
    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> R<IngestStats> {
        self.synced(
            self.writes
                .index_bytes_opts(org, repo, path, bytes, language, origin, opts),
        )
    }
    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> R<IngestStats> {
        self.synced(
            self.writes
                .ingest_file_with_origin(org, repo, path, language, ex, origin),
        )
    }
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> R<Vec<R<IngestStats>>> {
        self.synced(self.writes.index_batch(org, repo, files, opts))
    }
    fn prepare(
        &self,
        org: &str,
        repo: &str,
        file: &BatchFile<'_>,
        opts: IndexOptions,
    ) -> R<PreparedFile> {
        self.writes.prepare(org, repo, file, opts)
    }
    fn index_prepared(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> R<Vec<R<IngestStats>>> {
        self.synced(self.writes.index_prepared(org, repo, files, opts))
    }
    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> R<Vec<String>> {
        self.synced(self.writes.prune_files(org, repo, keep, dry_run))
    }
    fn vacuum(&self) -> R<VacuumStats> {
        self.synced(self.writes.vacuum())
    }
}

pub fn replica(tb: &ClusterTestbed, id: u64) -> Replica {
    Replica {
        reads: tb.client(id),
        writes: tb.client(tb.leader()),
    }
}

/// The read surfaces the crash tests compare: counts per kind and
/// `describe`.
pub fn summary(s: &dyn StoreRead) -> (Vec<usize>, Vec<RepoInfo>) {
    let counts = [
        NodeKind::Org,
        NodeKind::Repo,
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Token,
    ]
    .iter()
    .map(|k| s.count_nodes(*k).unwrap())
    .collect();
    (counts, s.describe(None, None).unwrap())
}

pub fn oracle(dir: &Path) -> Box<dyn Store> {
    open_store(&dir.join("oracle.redb"), exts()).unwrap()
}

pub fn small_file(i: usize) -> (String, Vec<u8>) {
    (
        format!("src/f{i}.rs"),
        format!("fn f{i}() -> u32 {{ {i} }}\n").into_bytes(),
    )
}

/// (id, addr, role) of every member.
pub fn member_list(c: &RemoteStore) -> Vec<(u64, String, String)> {
    c.admin_members()
        .unwrap()
        .members
        .into_iter()
        .map(|m| (m.node_id, m.addr, m.role))
        .collect()
}

/// A per-test watchdog: if the returned guard is still alive after
/// `limit`, the whole test binary exits with a message naming `what`
/// (a hung test fails loudly instead of holding CI until its own timeout).
/// Drop the guard (end of the test) to disarm it.
pub fn watchdog(what: &'static str, limit: Duration) -> Watchdog {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
            eprintln!("WATCHDOG: test {what} still running after {limit:?}; failing the run");
            std::process::exit(101);
        }
    });
    Watchdog { _tx: tx }
}

pub struct Watchdog {
    _tx: std::sync::mpsc::Sender<()>,
}

/// The default [`watchdog`] limit of a cluster test.
pub const TEST_LIMIT: Duration = Duration::from_secs(300);
