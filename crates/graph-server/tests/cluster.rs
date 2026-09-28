//! Stage B (ADR 0004 D5-D7, epic story 21): replication through the
//! in-process `ClusterTestbed`. Every wait has a hard timeout; failures are
//! injected through deterministic hooks (failpoints, the fault plan, a fake
//! disk probe), never by sleeping and hoping.
use graph_client::RemoteStore;
use graph_core::{Extraction, Extractor, Node, NodeId as GNodeId, NodeKind};
use graph_server::raft::log_store::AppendEvent;
use graph_server::testing::{ClusterTestbed, TestServer, CLUSTER_WAIT, TEST_RAFT};
use graph_server::{InitMode, RaftSettings, ServeConfig};
use graph_store::conformance::run_differential;
use graph_store::{
    open_store, BatchFile, Hit, IndexOptions, IngestStats, Page, PreparedFile, Query, RepoInfo,
    SnapshotStats, Store, StoreError, StoreRead, SymbolHit, SymbolQuery, VacuumStats,
    ORIGIN_DIRECTORY,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type R<T> = Result<T, StoreError>;

fn exts() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(graph_lang_rust::RustExtractor),
        Box::new(graph_lang_csharp::CSharpExtractor),
        Box::new(graph_lang_javascript::JavaScriptExtractor),
    ]
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus")
}

/// Every file under `dir`, sorted, as `(relative path, bytes)`.
fn files_under(dir: &Path) -> Vec<(String, Vec<u8>)> {
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

fn index_files(s: &dyn Store, org: &str, repo: &str, files: &[(String, Vec<u8>)]) {
    for chunk in files.chunks(64) {
        let batch: Vec<BatchFile<'_>> = chunk
            .iter()
            .map(|(p, b)| BatchFile {
                path: p,
                bytes: b,
                language: None,
                origin: Some(ORIGIN_DIRECTORY),
            })
            .collect();
        s.index_batch(org, repo, &batch, IndexOptions::default())
            .unwrap();
    }
}

/// A > 1 MiB JavaScript file (the corpus has none that large).
fn big_js() -> Vec<u8> {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < (1 << 20) + 4096 {
        s.push_str(&format!("function f{i}(a) {{ return a + {i}; }}\n"));
        i += 1;
    }
    s.into_bytes()
}

/// Three languages from the vendored corpus (Rust, C#, TypeScript/HTML).
const CORPUS_REPOS: [&str; 3] = ["anyhow", "rebus-rabbitmq", "conduit-ui"];

/// The corpus repos plus one file over 1 MiB.
fn index_corpus_subset(s: &dyn Store) {
    for repo in CORPUS_REPOS {
        index_files(s, "corpus", repo, &files_under(&corpus_root().join(repo)));
    }
    index_files(s, "corpus", "big", &[("big.js".to_string(), big_js())]);
}

/// Wait until node `c` (a client of it) has applied at least `index`.
fn wait_node_applied(c: &RemoteStore, index: u64) {
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
struct Replica {
    reads: RemoteStore,
    writes: RemoteStore,
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

fn replica(tb: &ClusterTestbed, id: u64) -> Replica {
    Replica {
        reads: tb.client(id),
        writes: tb.client(tb.leader()),
    }
}

/// The read surfaces the crash tests compare: counts per kind and
/// `describe`.
fn summary(s: &dyn StoreRead) -> (Vec<usize>, Vec<RepoInfo>) {
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

fn oracle(dir: &Path) -> Box<dyn Store> {
    open_store(&dir.join("oracle.redb"), exts()).unwrap()
}

/// Settings for tests that snapshot and purge often.
fn snappy() -> RaftSettings {
    RaftSettings {
        snapshot_log_entries: 5,
        log_keep_entries: 2,
        purge_batch_size: 1,
        ..TEST_RAFT
    }
}

fn small_file(i: usize) -> (String, Vec<u8>) {
    (
        format!("src/f{i}.rs"),
        format!("fn f{i}() -> u32 {{ {i} }}\n").into_bytes(),
    )
}

/// What a corpus store answers, for comparing replicas: counts and
/// `describe`, a spread of searches at several grains and symbol searches
/// (limited), and the exact tokens of every 16th file of every corpus repo
/// plus the big one. (`run_differential` walks every node from the roots,
/// which over a whole corpus is minutes of RPCs, so it runs on the
/// replicas after the corpus is pruned.)
type TokenRow = (
    String,
    Option<graph_core::Span>,
    Option<graph_core::TokenClass>,
);

#[derive(Debug, PartialEq)]
struct CorpusAnswers {
    summary: (Vec<usize>, Vec<RepoInfo>),
    searches: Vec<Vec<Hit>>,
    symbols: Vec<Vec<SymbolHit>>,
    tokens: Vec<Option<Vec<TokenRow>>>,
}

fn corpus_answers(s: &dyn StoreRead) -> CorpusAnswers {
    use graph_store::Grain;
    let mut searches = Vec::new();
    for text in ["fn", "class", "Error", "impl"] {
        for grain in [Grain::Token, Grain::Symbol, Grain::Class, Grain::File] {
            let mut q = Query::new(text);
            q.grain = grain;
            q.limit = Some(100);
            searches.push(s.search(&q).unwrap());
        }
    }
    let symbols = ["new*", "Error", "f1*"]
        .iter()
        .map(|pat| {
            let mut q = SymbolQuery::new(*pat);
            q.limit = Some(100);
            s.search_symbols(&q).unwrap()
        })
        .collect();
    let tok = |repo: &str, path: &str| {
        s.file_tokens("corpus", repo, path).unwrap().map(|v| {
            v.into_iter()
                .map(|n| (n.name, n.span, n.token_class))
                .collect::<Vec<_>>()
        })
    };
    let mut tokens = Vec::new();
    for repo in CORPUS_REPOS {
        for (path, _) in files_under(&corpus_root().join(repo)).iter().step_by(16) {
            tokens.push(tok(repo, path));
        }
    }
    let big = tok("big", "big.js");
    assert!(
        big.as_ref().is_some_and(|t| t.len() > 10_000),
        "the > 1 MiB file"
    );
    tokens.push(big);
    CorpusAnswers {
        summary: summary(s),
        searches,
        symbols,
        tokens,
    }
}

/// Remove the corpus again (a replicated `prune` of every repo, then a
/// replicated `vacuum`).
fn prune_corpus(s: &dyn Store) {
    for repo in CORPUS_REPOS.iter().chain(["big"].iter()) {
        let removed = s
            .prune_files("corpus", repo, &HashSet::new(), false)
            .unwrap();
        assert!(!removed.is_empty(), "{repo}");
    }
    s.vacuum().unwrap();
}

#[test]
fn three_nodes_replicate_and_answer_identically() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let leader = tb.client(tb.leader());
    index_corpus_subset(&leader);
    let d = tempfile::tempdir().unwrap();
    let embedded = oracle(d.path());
    index_corpus_subset(embedded.as_ref());
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    // Every replica holds the same corpus as the embedded oracle.
    let want = corpus_answers(embedded.as_ref());
    for id in tb.ids() {
        assert!(
            corpus_answers(&tb.client(id)) == want,
            "node {id} answers differently from the embedded oracle"
        );
    }
    // Prune and vacuum replicate too; then the full differential harness
    // (its own seed, every grain, traversal) on the replicas.
    prune_corpus(&leader);
    prune_corpus(embedded.as_ref());
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    for id in tb.ids() {
        assert_eq!(
            summary(&tb.client(id)),
            summary(embedded.as_ref()),
            "node {id} after prune"
        );
    }
    run_differential(&replica(&tb, 1), &replica(&tb, 3));
    run_differential(embedded.as_ref(), &replica(&tb, 2));
}

#[test]
fn leader_loss_elects_and_writes_resume() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let old = tb.leader();
    let files: Vec<_> = (0..5).map(small_file).collect();
    index_files(&tb.client(old), "o", "r", &files);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let survivors: Vec<u64> = tb.ids().into_iter().filter(|i| *i != old).collect();
    // Readers on the survivors, LOCAL, for the whole election.
    let stop = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = survivors
        .iter()
        .map(|id| {
            let c = tb.client(*id);
            let (stop, failures, reads) =
                (Arc::clone(&stop), Arc::clone(&failures), Arc::clone(&reads));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match c.count_nodes(NodeKind::File) {
                        Ok(n) => {
                            assert!((5..=8).contains(&n), "{n} files");
                            reads.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => {
                            eprintln!("read failed: {e}");
                            failures.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            })
        })
        .collect();
    tb.node_mut(old).stop();
    let new = tb.leader();
    assert_ne!(new, old);
    let more: Vec<_> = (5..8).map(small_file).collect();
    index_files(&tb.client(new), "o", "r", &more);
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(failures.load(Ordering::SeqCst), 0, "LOCAL reads failed");
    assert!(reads.load(Ordering::SeqCst) > 0);
    tb.node_mut(old).restart();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_eq!(summary(&tb.client(old)), summary(&tb.client(new)));
    assert_eq!(tb.client(old).count_nodes(NodeKind::File).unwrap(), 8);
}

#[test]
fn laggard_catches_up_by_install_snapshot() {
    let mut tb = ClusterTestbed::with_config(3, exts(), |_, c| c.raft = Some(snappy()));
    tb.form();
    let leader = tb.leader();
    let laggard = tb.ids().into_iter().find(|i| *i != leader).unwrap();
    let c = tb.client(leader);
    index_files(&c, "o", "r", &[small_file(0)]);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let behind = tb
        .node(laggard)
        .raft()
        .unwrap()
        .metrics()
        .last_log_index
        .unwrap();
    tb.node_mut(laggard).stop();
    // One entry per file: enough to snapshot and purge past the laggard.
    for i in 1..20 {
        c.index_bytes("o", "r", &small_file(i).0, &small_file(i).1, None)
            .unwrap();
    }
    let raft = tb.node(leader).raft().unwrap().raft.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        raft.wait(Some(CLUSTER_WAIT))
            .metrics(
                |m| m.purged.is_some_and(|p| p.index > behind),
                "the leader purged past the laggard",
            )
            .await
            .unwrap();
    });
    tb.node_mut(laggard).restart();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert!(
        tb.node(laggard).raft().unwrap().snapshots_installed() >= 1,
        "the laggard caught up through InstallSnapshot"
    );
    assert_eq!(summary(&tb.client(laggard)), summary(&tb.client(leader)));
    assert_eq!(tb.client(laggard).count_nodes(NodeKind::File).unwrap(), 20);
    run_differential(&replica(&tb, laggard), &replica(&tb, leader));
}

/// (id, addr, role) of every member.
fn member_list(c: &RemoteStore) -> Vec<(u64, String, String)> {
    c.admin_members()
        .unwrap()
        .members
        .into_iter()
        .map(|m| (m.node_id, m.addr, m.role))
        .collect()
}

#[test]
fn restart_with_persisted_state() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let files: Vec<_> = (0..10).map(small_file).collect();
    index_files(&tb.client(tb.leader()), "o", "r", &files);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let before = summary(&tb.client(1));
    let status = tb.client(1).admin_status().unwrap();
    let members = member_list(&tb.client(1));
    for id in tb.ids() {
        tb.node_mut(id).stop();
    }
    for id in tb.ids() {
        tb.node_mut(id).restart();
    }
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    for id in tb.ids() {
        let c = tb.client(id);
        assert_eq!(summary(&c), before, "node {id}");
        let st = c.admin_status().unwrap();
        assert_eq!(st.cluster_id, status.cluster_id, "node {id}");
        assert!(!st.cluster_id.is_empty());
        assert_eq!(member_list(&c), members, "node {id}");
    }
}

/// Write one batch through the leader and return its log index, checking
/// it is the index the test armed a failpoint for.
fn write_at(tb: &ClusterTestbed, target: u64, files: &[(String, Vec<u8>)]) {
    let c = tb.client(tb.leader());
    index_files(&c, "o", "r", files);
    assert_eq!(
        c.applied_index().load(Ordering::SeqCst),
        target,
        "the batch landed at the armed index"
    );
}

/// The shared body of the two exactly-once crash tests: arm a failpoint on
/// node 2 at the next index, write, see node 2 stop, kill and restart it
/// without the failpoint, and compare it to an embedded oracle.
fn crash_and_replay(arm: impl Fn(&mut ServeConfig, u64), disarm: impl Fn(&mut ServeConfig)) {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let warm: Vec<_> = (0..3).map(small_file).collect();
    index_files(&tb.client(tb.leader()), "o", "r", &warm);
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    assert_ne!(tb.leader(), 2, "node 1 bootstrapped and leads");
    let target = tb.leader_last_log_index() + 1;
    tb.node_mut(2).stop();
    arm(tb.node_mut(2).config_mut(), target);
    tb.node_mut(2).restart();
    tb.wait_applied(target - 1, CLUSTER_WAIT);
    let batch: Vec<_> = (3..7).map(small_file).collect();
    write_at(&tb, target, &batch);
    tb.node(2).wait_fatal(CLUSTER_WAIT);
    let m = tb.node(2).raft().unwrap().metrics();
    assert!(
        m.last_log_index.unwrap_or(0) >= target,
        "the entry is in node 2's log"
    );
    assert_eq!(tb.node(2).applied_index(), target - 1, "and not applied");
    tb.node_mut(2).kill();
    disarm(tb.node_mut(2).config_mut());
    tb.node_mut(2).restart();
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    let c2 = tb.client(2);
    let st = c2.admin_status().unwrap();
    assert_eq!(
        st.applied_index, st.committed_index,
        "last_applied equals committed"
    );
    let d = tempfile::tempdir().unwrap();
    let o = oracle(d.path());
    index_files(o.as_ref(), "o", "r", &warm);
    index_files(o.as_ref(), "o", "r", &batch);
    assert_eq!(summary(&c2), summary(o.as_ref()));
    assert_eq!(
        c2.count_nodes(NodeKind::File).unwrap(),
        7,
        "no duplicate files"
    );
}

#[test]
fn crash_after_log_before_apply_replays_once() {
    crash_and_replay(
        |c, at| c.testing.fail_before_apply = Some(at),
        |c| c.testing.fail_before_apply = None,
    );
}

#[test]
fn kill_during_apply_reapplies_exactly_once() {
    crash_and_replay(
        |c, at| c.testing.fail_in_apply_txn = Some(at),
        |c| c.testing.fail_in_apply_txn = None,
    );
}

#[test]
fn acked_write_survives_killing_every_node() {
    let mut tb = ClusterTestbed::new(3, exts());
    tb.form();
    let files: Vec<_> = (0..6).map(small_file).collect();
    // Acknowledged: the call returned.
    index_files(&tb.client(tb.leader()), "o", "r", &files);
    let acked = tb.client(tb.leader()).admin_status().unwrap().applied_index;
    for id in tb.ids() {
        tb.node_mut(id).kill();
    }
    for id in tb.ids() {
        tb.node_mut(id).restart();
    }
    tb.wait_applied(acked, CLUSTER_WAIT);
    for id in tb.ids() {
        let c = tb.client(id);
        for (path, _) in &files {
            assert!(
                c.file_tokens("o", "r", path).unwrap().is_some(),
                "node {id} lost {path}"
            );
        }
    }
}

#[test]
fn durability_order_log_flushed_after_commit() {
    type Events = Arc<Mutex<Vec<AppendEvent>>>;
    let logs: Vec<Events> = (0..3).map(|_| Events::default()).collect();
    let l2 = logs.clone();
    let mut tb = ClusterTestbed::with_config(3, exts(), move |id, c| {
        let log = Arc::clone(&l2[id as usize - 1]);
        c.append_observer = Some(Arc::new(move |e| log.lock().unwrap().push(e)));
    });
    tb.form();
    for i in 0..5 {
        let f = small_file(i);
        tb.client(tb.leader())
            .index_bytes("o", "r", &f.0, &f.1, None)
            .unwrap();
    }
    tb.wait_applied(tb.leader_last_log_index(), CLUSTER_WAIT);
    for (i, log) in logs.iter().enumerate() {
        let ev = log.lock().unwrap().clone();
        assert!(ev.len() >= 10, "node {} saw appends: {ev:?}", i + 1);
        for pair in ev.chunks(2) {
            match pair {
                [AppendEvent::Committed { last_index: a }, AppendEvent::Flushed { last_index: b }] =>
                {
                    assert_eq!(a, b)
                }
                other => panic!("node {}: flushed before commit: {other:?}", i + 1),
            }
        }
    }
}

#[test]
fn snapshot_file_is_consistent_and_restorable() {
    let tb = ClusterTestbed::new(1, exts());
    let c = tb.client(1);
    index_corpus_subset(&c);
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("backup.redb");
    let info = c.admin_trigger_snapshot(Some(&out)).unwrap();
    assert_eq!(std::fs::metadata(&out).unwrap().len(), info.size);
    assert!(info.last_applied_index > 0);
    let src = c.admin_status().unwrap();
    let cfg = ServeConfig::for_data_dir(
        d.path().join("restored"),
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap {
            restore: Some(out.clone()),
        },
        Some(1),
    );
    let restored = TestServer::try_start_config(cfg, exts()).unwrap();
    let r = RemoteStore::connect(graph_client::ClientConfig::new(restored.endpoint())).unwrap();
    let st = r.admin_status().unwrap();
    assert_ne!(st.cluster_id, src.cluster_id, "a restore is a new cluster");
    assert!(
        st.last_log_index < 5 && st.last_log_index < src.last_log_index,
        "the log starts fresh: {} vs {}",
        st.last_log_index,
        src.last_log_index
    );
    assert!(
        corpus_answers(&r) == corpus_answers(&c),
        "the restored node answers differently from the source"
    );
}

#[test]
fn bootstrap_is_idempotent_on_restart() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("n1");
    let cfg = |init| ServeConfig::for_data_dir(&dir, "127.0.0.1:0".parse().unwrap(), init, Some(1));
    let boot = InitMode::Bootstrap { restore: None };
    let mut s = TestServer::try_start_config(cfg(boot.clone()), exts()).unwrap();
    let c = RemoteStore::connect(graph_client::ClientConfig::new(s.endpoint())).unwrap();
    c.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    let first = c.admin_status().unwrap();
    drop(c);
    s.stop();
    for init in [boot, InitMode::Restart, InitMode::Uninitialized] {
        let s = TestServer::try_start_config(cfg(init.clone()), exts()).unwrap();
        let c = RemoteStore::connect(graph_client::ClientConfig::new(s.endpoint())).unwrap();
        let st = c.admin_status().unwrap();
        assert_eq!(st.cluster_id, first.cluster_id, "{init:?}");
        assert_eq!(st.members, first.members, "{init:?}");
        assert!(st.last_log_index >= first.last_log_index, "{init:?}");
        assert_eq!(c.count_nodes(NodeKind::File).unwrap(), 1, "{init:?}");
    }
}

#[test]
fn empty_dir_without_flags_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("n");
    let cfg = ServeConfig::for_data_dir(
        &dir,
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Restart,
        Some(1),
    );
    let e = TestServer::try_start_config(cfg, exts())
        .err()
        .expect("refused");
    assert!(
        matches!(e, StoreError::Rejected(ref m) if m.contains("--bootstrap")),
        "{e:?}"
    );
    assert!(!dir.join("node.json").exists() && !dir.join("graph.redb").exists());
}

#[test]
fn node_id_mismatch_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("n");
    let addr = "127.0.0.1:0".parse().unwrap();
    let boot = InitMode::Bootstrap { restore: None };
    drop(
        TestServer::try_start_config(
            ServeConfig::for_data_dir(&dir, addr, boot.clone(), Some(1)),
            exts(),
        )
        .unwrap(),
    );
    let e =
        TestServer::try_start_config(ServeConfig::for_data_dir(&dir, addr, boot, Some(2)), exts())
            .err()
            .expect("refused");
    let m = e.to_string();
    assert!(m.contains("node 1") && m.contains("--node-id 2"), "{m}");
    // Without --node-id the recorded one is used.
    drop(
        TestServer::try_start_config(
            ServeConfig::for_data_dir(&dir, addr, InitMode::Restart, None),
            exts(),
        )
        .unwrap(),
    );
}

#[test]
fn install_snapshot_refuses_other_extractors_hash() {
    use graph_proto::pb;
    let d = tempfile::tempdir().unwrap();
    let s = TestServer::try_start_config(
        ServeConfig::for_data_dir(
            d.path().join("n"),
            "127.0.0.1:0".parse().unwrap(),
            InitMode::Bootstrap { restore: None },
            Some(1),
        ),
        exts(),
    )
    .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = format!("http://{}", s.endpoint());
    let header = |hash: &str, format: u64| pb::InstallSnapshotRequest {
        msg: Some(pb::install_snapshot_request::Msg::Header(
            pb::InstallSnapshotHeader {
                vote: Some(pb::RaftVote {
                    term: 99,
                    node_id: 7,
                    committed: true,
                }),
                last_log_id: None,
                membership_json: b"{}".to_vec(),
                snapshot_id: "x".into(),
                store_format_version: format,
                extractors_hash: hash.into(),
                size: 3,
                sha256: "00".into(),
            },
        )),
    };
    let chunk = pb::InstallSnapshotRequest {
        msg: Some(pb::install_snapshot_request::Msg::Chunk(vec![1, 2, 3])),
    };
    for (hash, format) in [
        ("another-extractor-set", graph_store::SCHEMA_VERSION),
        ("whatever", graph_store::SCHEMA_VERSION + 1),
    ] {
        let st = rt
            .block_on(async {
                let ch = tonic::transport::Endpoint::from_shared(endpoint.clone())
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut c = pb::raft_client::RaftClient::new(ch);
                c.install_snapshot(tokio_stream::iter(vec![
                    header(hash, format),
                    chunk.clone(),
                ]))
                .await
            })
            .unwrap_err();
        assert_eq!(st.code(), tonic::Code::FailedPrecondition, "{st:?}");
    }
    // The node is untouched and still leads.
    let c = RemoteStore::connect(graph_client::ClientConfig::new(s.endpoint())).unwrap();
    c.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    let st = c.admin_status().unwrap();
    assert_eq!(st.role, "leader");
    let leftovers: Vec<_> = std::fs::read_dir(d.path().join("n/snapshots"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn disk_guard_refuses_write_with_resource_exhausted() {
    let free = Arc::new(AtomicU64::new(u64::MAX));
    let f2 = Arc::clone(&free);
    let d = tempfile::tempdir().unwrap();
    let mut cfg = ServeConfig::for_data_dir(
        d.path().join("n"),
        "127.0.0.1:0".parse().unwrap(),
        InitMode::Bootstrap { restore: None },
        Some(1),
    );
    cfg.min_free_disk = 1 << 30;
    cfg.free_space_probe = Some(Arc::new(move |_| Some(f2.load(Ordering::SeqCst))));
    let s = TestServer::try_start_config(cfg, exts()).unwrap();
    let mut ccfg = graph_client::ClientConfig::new(s.endpoint());
    ccfg.write_deadline = Duration::from_secs(2);
    let c = RemoteStore::connect(ccfg).unwrap();
    c.index_bytes("o", "r", "a.rs", b"fn a() {}", None).unwrap();
    let before = c.admin_status().unwrap().applied_index;
    free.store(1 << 20, Ordering::SeqCst);
    let e = c
        .index_bytes("o", "r", "b.rs", b"fn b() {}", None)
        .unwrap_err();
    assert!(
        graph_proto::error::is_disk_full(&e.to_string()),
        "a disk-full refusal: {e:?}"
    );
    // The typed status code, straight from the wire.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", s.endpoint()))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut w = graph_proto::pb::write_client::WriteClient::new(ch);
        w.vacuum(graph_proto::pb::VacuumRequest {})
            .await
            .unwrap_err()
            .code()
    });
    assert_eq!(code, tonic::Code::ResourceExhausted);
    assert!(
        c.admin_trigger_snapshot(None).is_err(),
        "no snapshot either"
    );
    assert_eq!(
        c.admin_status().unwrap().applied_index,
        before,
        "nothing logged"
    );
    free.store(u64::MAX, Ordering::SeqCst);
    c.index_bytes("o", "r", "b.rs", b"fn b() {}", None).unwrap();
    assert_eq!(c.count_nodes(NodeKind::File).unwrap(), 2);
}
