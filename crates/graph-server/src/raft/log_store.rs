//! [`RedbLogStore`]: the Raft log and hard state in their own redb file
//! (`<db>.raft.redb`, ADR 0004 D6/D7).
//!
//! Tables: `raft_log` (`index -> encoded entry`) and `raft_meta` (`vote`,
//! `committed`, `last_purged`, each serde_json). redb's default durability is
//! `Immediate` (every commit fsyncs), so an entry is on disk when `append`
//! commits, and only then is openraft's `LogFlushed` callback invoked: a
//! write acknowledged by the leader is in the log on disk (D7).
//!
//! Entry encoding (own framing, not serde: an `IndexChunk` carries up to 8
//! MiB of source bytes and serde_json would render them as a number list):
//! `kind:u8 | term:u64 | node_id:u64 | index:u64 | payload`, little endian,
//! where kind 0 = blank (no payload), 1 = normal (payload = the prost
//! `LogCommand` bytes), 2 = membership (payload = serde_json `Membership`).
//! Golden-byte tested below.
use super::types::{Entry, LogId, LogRequest, NodeId, StorageError, StorageIOError, TypeConfig};
use graph_store::StoreError;
use openraft::storage::{LogFlushed, LogState, RaftLogStorage};
use openraft::{
    AnyError, CommittedLeaderId, EntryPayload, Membership, RaftLogReader, RaftTypeConfig, Vote,
};
use redb::{
    Database, ReadTransaction, ReadableTable, ReadableTableMetadata, StorageBackend,
    TableDefinition, WriteTransaction,
};
use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard};

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");
const K_VOTE: &str = "vote";
const K_COMMITTED: &str = "committed";
const K_PURGED: &str = "last_purged";

/// Free space a purge may leave in `raft.redb` before it is compacted
/// (beyond the live pages again): see [`RedbLogStore::compact_if_sparse`].
const COMPACT_SLACK: u64 = 1 << 20;

/// The most live log a compaction may rewrite (the stall bound): above it
/// the purge skips compacting and a later purge, with less left, tries
/// again. See [`RedbLogStore::compact_if_sparse`].
pub const COMPACT_MAX_LIVE: u64 = 64 << 20;

/// How long dropping the last handle of a log store waits for a
/// background compaction to end (it cannot be interrupted, and the file
/// stays open until it ends).
pub const COMPACT_DRAIN: std::time::Duration = std::time::Duration::from_secs(120);

const KIND_BLANK: u8 = 0;
const KIND_NORMAL: u8 = 1;
const KIND_MEMBERSHIP: u8 = 2;

/// `<db>.raft.redb` for a store at `db`.
pub fn log_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".raft.redb");
    PathBuf::from(p)
}

/// What a test-only observer of [`RedbLogStore::append`] sees, in order
/// (`durability_order_log_flushed_after_commit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendEvent {
    /// The redb commit of the entries up to `last_index` returned.
    Committed { last_index: u64 },
    /// openraft's `LogFlushed` callback for them is about to be invoked.
    Flushed { last_index: u64 },
}

pub type AppendObserver = Arc<dyn Fn(AppendEvent) + Send + Sync>;

/// Test-only hook run while a compaction holds the write side of the lock.
#[cfg(test)]
type CompactGate = Arc<dyn Fn() + Send + Sync>;

/// What [`RedbLogStore::probe`] found in a log file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogProbe {
    /// The file exists.
    pub exists: bool,
    /// A vote was saved (the node was initialized, voted or was contacted
    /// by a leader).
    pub vote: bool,
    /// Entries in the log.
    pub entries: u64,
    /// A purge point was saved.
    pub purged: bool,
}

impl LogProbe {
    /// No Raft state at all.
    pub fn is_blank(&self) -> bool {
        !self.vote && self.entries == 0 && !self.purged
    }
}

fn open_err(path: &Path, e: redb::DatabaseError) -> StoreError {
    match e {
        redb::DatabaseError::DatabaseAlreadyOpen => StoreError::Locked(path.display().to_string()),
        e => StoreError::OpenFailed {
            path: path.display().to_string(),
            reason: e.to_string(),
        },
    }
}

/// Whether a background compaction runs, waitable.
#[derive(Default)]
struct Busy {
    running: Mutex<bool>,
    idle: Condvar,
}

impl Busy {
    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mark it running; false if it already was.
    fn start(&self) -> bool {
        !std::mem::replace(&mut *self.lock(), true)
    }

    fn stop(&self) {
        *self.lock() = false;
        self.idle.notify_all();
    }

    fn is_running(&self) -> bool {
        *self.lock()
    }

    /// Wait until no compaction runs; false on timeout.
    fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let g = self.lock();
        let (g, _) = self
            .idle
            .wait_timeout_while(g, timeout, |r| *r)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !*g
    }
}

/// Clears [`Busy`] when the background compaction ends, also when its
/// task is dropped unrun (a runtime shutting down).
struct BusyGuard(Arc<Busy>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Shared by the handles callers hold (not by the background
/// compaction's clone): when the last one goes, the store is closing, and
/// that drop waits for a running compaction, whose clone keeps the file
/// open, so a reopen right after never finds it locked.
struct Owner {
    busy: Arc<Busy>,
    closing: Arc<AtomicBool>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        if !self.busy.wait_idle(COMPACT_DRAIN) {
            tracing::warn!(
                "raft log: a compaction still runs {COMPACT_DRAIN:?} after the log was closed"
            );
        }
    }
}

#[derive(Clone)]
pub struct RedbLogStore {
    /// Every transaction holds the read side; the post-purge compaction
    /// takes the write side, so no transaction of ours is open while redb
    /// moves pages.
    db: Arc<RwLock<Database>>,
    path: PathBuf,
    /// The file size right after the last compaction (0: none yet).
    compacted_bytes: Arc<AtomicU64>,
    /// A background compaction is running.
    compacting: Arc<Busy>,
    /// The last caller-held handle was dropped: a compaction skips.
    closing: Arc<AtomicBool>,
    /// `None` only in the background compaction's own clone.
    owner: Option<Arc<Owner>>,
    /// See [`COMPACT_MAX_LIVE`] (tests lower it).
    max_live: Arc<AtomicU64>,
    observer: Option<AppendObserver>,
    #[cfg(test)]
    compact_gate: Arc<std::sync::Mutex<Option<CompactGate>>>,
    /// Test-only: why the last `compact_if_sparse` returned before
    /// compacting (`None`: it compacted, or none ran yet).
    #[cfg(test)]
    last_skip: Arc<std::sync::Mutex<Option<&'static str>>>,
}

/// A redb transaction together with the read guard of the store's lock it
/// was begun under. Fields drop in declaration order, so the transaction
/// always ends before the guard is released: the ordering the compaction
/// relies on is enforced by the type, not by convention.
pub(crate) struct Guarded<'a, T> {
    txn: T,
    _guard: RwLockReadGuard<'a, Database>,
}

impl<T> std::ops::Deref for Guarded<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.txn
    }
}

impl Guarded<'_, WriteTransaction> {
    pub(crate) fn commit(self) -> Result<(), redb::CommitError> {
        let Guarded { txn, _guard } = self;
        txn.commit()
    }

    pub(crate) fn abort(self) -> Result<(), redb::StorageError> {
        let Guarded { txn, _guard } = self;
        txn.abort()
    }
}

impl Debug for RedbLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedbLogStore")
    }
}

pub fn encode_entry(e: &Entry) -> Vec<u8> {
    let (kind, payload): (u8, Vec<u8>) = match &e.payload {
        EntryPayload::Blank => (KIND_BLANK, Vec::new()),
        EntryPayload::Normal(r) => (KIND_NORMAL, r.command.clone()),
        EntryPayload::Membership(m) => (
            KIND_MEMBERSHIP,
            serde_json::to_vec(m).expect("Membership serializes"),
        ),
    };
    let mut out = Vec::with_capacity(25 + payload.len());
    out.push(kind);
    out.extend_from_slice(&e.log_id.leader_id.term.to_le_bytes());
    out.extend_from_slice(&e.log_id.leader_id.node_id.to_le_bytes());
    out.extend_from_slice(&e.log_id.index.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn decode_entry(bytes: &[u8]) -> Result<Entry, StoreError> {
    if bytes.len() < 25 {
        return Err(StoreError::Corrupt(format!(
            "raft log entry is {} bytes, expected at least 25",
            bytes.len()
        )));
    }
    let u = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
    let log_id = LogId::new(CommittedLeaderId::new(u(1), u(9)), u(17));
    let payload = &bytes[25..];
    let payload = match bytes[0] {
        KIND_BLANK => EntryPayload::Blank,
        KIND_NORMAL => EntryPayload::Normal(LogRequest {
            command: payload.to_vec(),
        }),
        KIND_MEMBERSHIP => EntryPayload::Membership(
            serde_json::from_slice::<Membership<NodeId, openraft::impls::BasicNode>>(payload)
                .map_err(|e| StoreError::Corrupt(format!("raft log membership entry: {e}")))?,
        ),
        k => {
            return Err(StoreError::Corrupt(format!(
                "raft log entry kind {k} is unknown"
            )))
        }
    };
    Ok(Entry { log_id, payload })
}

fn read_err(e: impl std::error::Error + 'static) -> StorageError {
    StorageIOError::read_logs(AnyError::new(&e)).into()
}

fn write_err(e: impl std::error::Error + 'static) -> StorageError {
    StorageIOError::write_logs(AnyError::new(&e)).into()
}

impl RedbLogStore {
    /// Open or create the log file at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with(path, None)
    }

    /// [`open`](Self::open), over a test-only redb [`StorageBackend`]
    /// (the power-cut tests) instead of the file when `backend` is `Some`.
    pub fn open_with(
        path: &Path,
        backend: Option<Box<dyn StorageBackend>>,
    ) -> Result<Self, StoreError> {
        let opened = match backend {
            Some(b) => Database::builder().create_with_backend(crate::powercut::DynBackend(b)),
            None => Database::create(path),
        };
        let db = opened.map_err(|e| open_err(path, e))?;
        let wt = db.begin_write()?;
        wt.open_table(LOG)?;
        wt.open_table(META)?;
        wt.commit()?;
        let compacting = Arc::new(Busy::default());
        let closing = Arc::new(AtomicBool::new(false));
        let s = Self {
            db: Arc::new(RwLock::new(db)),
            path: path.to_path_buf(),
            compacted_bytes: Arc::new(AtomicU64::new(0)),
            owner: Some(Arc::new(Owner {
                busy: Arc::clone(&compacting),
                closing: Arc::clone(&closing),
            })),
            compacting,
            closing,
            max_live: Arc::new(AtomicU64::new(COMPACT_MAX_LIVE)),
            observer: None,
            #[cfg(test)]
            compact_gate: Arc::default(),
            #[cfg(test)]
            last_skip: Arc::default(),
        };
        Ok(s)
    }

    /// Encoded bytes of the entries above `after`, up to and including
    /// `upto`, in the log right now (the snapshot policy's byte trigger,
    /// `--snapshot-log-bytes`). It reads every value in the range under
    /// the log's read lock (a compaction waits meanwhile), so the policy
    /// asks only for the entries applied since its last call, and for
    /// those above a new snapshot when one lands.
    pub fn bytes_between(&self, after: u64, upto: u64) -> Result<u64, StoreError> {
        if upto <= after {
            return Ok(0);
        }
        let rt = self.read_txn()?;
        let t = rt.open_table(LOG)?;
        let mut n = 0u64;
        for row in t.range::<u64>((Bound::Excluded(after), Bound::Included(upto)))? {
            let (_, v) = row?;
            n += v.value().len() as u64;
        }
        Ok(n)
    }

    /// What the log at `path` holds, read without creating anything (a
    /// missing file is [`LogProbe::default`]): the start-up consistency
    /// check between `node.json`, the log and the store (ADR 0004 D6).
    pub fn probe(path: &Path) -> Result<LogProbe, StoreError> {
        if !path.exists() {
            return Ok(LogProbe::default());
        }
        let db = Database::open(path).map_err(|e| open_err(path, e))?;
        let rt = db.begin_read()?;
        let entries = match rt.open_table(LOG) {
            Ok(t) => t.len()?,
            Err(redb::TableError::TableDoesNotExist(_)) => 0,
            Err(e) => return Err(e.into()),
        };
        let (vote, purged) = match rt.open_table(META) {
            Ok(t) => (t.get(K_VOTE)?.is_some(), t.get(K_PURGED)?.is_some()),
            Err(redb::TableError::TableDoesNotExist(_)) => (false, false),
            Err(e) => return Err(e.into()),
        };
        Ok(LogProbe {
            exists: true,
            vote,
            entries,
            purged,
        })
    }

    fn guard(&self) -> RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|p| p.into_inner())
    }

    /// A read transaction bound to the read side of the lock
    /// ([`Guarded`]: the compiler keeps the guard alive for as long as the
    /// transaction, so [`Self::compact_if_sparse`] can rely on no
    /// transaction of ours being open while it holds the write side).
    pub(crate) fn read_txn(&self) -> Result<Guarded<'_, ReadTransaction>, redb::TransactionError> {
        let guard = self.guard();
        let txn = guard.begin_read()?;
        Ok(Guarded { txn, _guard: guard })
    }

    /// A write transaction bound to the read side of the lock (see
    /// [`Self::read_txn`]); end it with [`Guarded::commit`].
    pub(crate) fn write_txn(
        &self,
    ) -> Result<Guarded<'_, WriteTransaction>, redb::TransactionError> {
        let guard = self.guard();
        let txn = guard.begin_write()?;
        Ok(Guarded { txn, _guard: guard })
    }

    /// Test-only: `compact_if_sparse` calls this while it holds the write
    /// side of the lock, before compacting (a test holds it open). While a
    /// gate is set the sparsity checks (floor, twice live, the cap) do not
    /// skip: a gated test is about the locking, and must reach the gate
    /// whatever redb's page accounting says on the machine at hand (#237).
    #[cfg(test)]
    pub(crate) fn set_compact_gate(&self, gate: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .compact_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = gate;
    }

    /// Shrink the file once a purge left it mostly free pages: redb reuses
    /// freed pages but never returns them to the file system, so without
    /// this `raft.redb` keeps the high-water mark of every entry ever
    /// between two snapshots (measured: a corpus index left 3.4x the source
    /// on disk after the snapshot purged the whole log).
    ///
    /// Cheap when there is nothing to gain: a file that has not grown by
    /// [`COMPACT_SLACK`] since the last compaction is left alone after one
    /// `stat`. Otherwise it compacts only when the file is at least twice
    /// its live pages plus [`COMPACT_SLACK`], so a steady state does not
    /// compact on every purge. `Database::compact` commits with two-phase
    /// commits (crash safe: an interrupted compaction leaves a valid file).
    ///
    /// **Stall bound.** It holds the write side of the lock, so every log
    /// read and write (`append`, `save_vote`, `save_committed`) waits for
    /// it, and the Raft core awaits those. `purge` runs it in the
    /// background, so the core never awaits the compaction itself, but it
    /// may await an append queued behind it. So it runs only when the live
    /// log after the purge is at most [`COMPACT_MAX_LIVE`] (64 MiB): redb
    /// then rewrites at most that much plus a few fsynced commits, well
    /// under a second on a disk writing 100 MB/s, below the 1 s minimum
    /// election timeout. A larger live log is skipped (logged at info) and
    /// the next purge, with less left, tries again. Returns whether it
    /// compacted; a failure is logged, never fatal (the log is intact
    /// either way).
    pub fn compact_if_sparse(&self) -> bool {
        #[cfg(test)]
        self.record_skip(None);
        if self.closing.load(Ordering::SeqCst) {
            return self.skip("closing");
        }
        let file = self.file_bytes();
        let floor = self.compacted_bytes.load(Ordering::Relaxed);
        if file < floor.saturating_add(COMPACT_SLACK) && !self.gated() {
            return self.skip("under the floor");
        }
        let live = {
            // A purge's pages are released only by later commits: two
            // empty ones (as `compact` does itself) make `allocated_pages`
            // exact.
            let release = || -> Result<(), redb::Error> {
                for _ in 0..2 {
                    self.write_txn()?.commit()?;
                }
                Ok(())
            };
            let stats = release().and_then(|()| {
                let wt = self.write_txn()?;
                let s = wt.stats();
                wt.abort()?;
                Ok(s?)
            });
            match stats {
                Ok(s) => s.allocated_pages() * s.page_size() as u64,
                Err(e) => {
                    tracing::warn!(error = %e, "raft log: reading page stats failed");
                    return self.skip("page stats failed");
                }
            }
        };
        // Not worth it while the file is within twice its live data (redb
        // keeps a floor: its region layout and power-of-two value
        // allocations).
        if file < live.saturating_mul(2).saturating_add(COMPACT_SLACK) && !self.gated() {
            return self.skip("within twice live");
        }
        let max_live = self.max_live.load(Ordering::Relaxed);
        if live > max_live && !self.gated() {
            tracing::info!(
                file,
                live,
                max_live,
                "raft log: compaction skipped, the live log is too large to rewrite without \
                 stalling appends (a later purge retries)"
            );
            return self.skip("above the cap");
        }
        let mut db = self.db.write().unwrap_or_else(|p| p.into_inner());
        #[cfg(test)]
        {
            let gate = self
                .compact_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(g) = gate {
                g();
            }
        }
        if self.closing.load(Ordering::SeqCst) {
            return self.skip("closing");
        }
        // `compact` itself loops until a pass makes no progress.
        let started = std::time::Instant::now();
        if let Err(e) = db.compact() {
            tracing::warn!(error = %e, "raft log: compaction skipped");
        }
        drop(db);
        let after = self.file_bytes();
        self.compacted_bytes.store(after, Ordering::Relaxed);
        tracing::info!(
            before = file,
            after,
            live,
            took_ms = started.elapsed().as_millis() as u64,
            "raft log compacted after a purge"
        );
        true
    }

    // Skip bookkeeping for `compact_if_sparse` (the reasons are test-only).

    /// `compact_if_sparse` gives up for `_reason`: records it (in tests)
    /// and returns false.
    fn skip(&self, _reason: &'static str) -> bool {
        #[cfg(test)]
        self.record_skip(Some(_reason));
        false
    }

    /// Test-only: set (or with `None`, clear) the reason `last_skip` reports.
    #[cfg(test)]
    fn record_skip(&self, reason: Option<&'static str>) {
        *self
            .last_skip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
    }

    /// Test-only: why the last `compact_if_sparse` skipped, if it did.
    #[cfg(test)]
    pub(crate) fn last_skip(&self) -> Option<&'static str> {
        *self
            .last_skip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A test gate is set (see [`Self::set_compact_gate`]); always false
    /// outside tests.
    fn gated(&self) -> bool {
        #[cfg(test)]
        {
            self.compact_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Whether a background compaction (started by a purge) is running.
    pub fn is_compacting(&self) -> bool {
        self.compacting.is_running()
    }

    /// Wait (blocking) until no background compaction runs; false on
    /// timeout. Shutdown calls it, so the file can be reopened after.
    pub fn wait_compaction(&self, timeout: std::time::Duration) -> bool {
        self.compacting.wait_idle(timeout)
    }

    /// Test-only: lower [`COMPACT_MAX_LIVE`].
    #[doc(hidden)]
    pub fn set_compact_max_live(&self, bytes: u64) {
        self.max_live.store(bytes, Ordering::Relaxed);
    }

    /// Record every append's commit and flush callback (tests only).
    pub fn set_observer(&mut self, observer: Option<AppendObserver>) {
        self.observer = observer;
    }

    /// The last committed index this node persisted (`save_committed`).
    pub fn committed_index(&self) -> Option<u64> {
        self.meta::<Option<LogId>>(K_COMMITTED)
            .ok()
            .flatten()
            .flatten()
            .map(|l| l.index)
    }

    /// The log file's size on disk.
    pub fn file_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    fn meta<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError> {
        let rt = self.read_txn().map_err(read_err)?;
        let t = rt.open_table(META).map_err(read_err)?;
        match t.get(key).map_err(read_err)? {
            None => Ok(None),
            Some(v) => serde_json::from_slice(v.value())
                .map(Some)
                .map_err(read_err),
        }
    }

    fn set_meta<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<(), StorageError> {
        let bytes = serde_json::to_vec(value).map_err(write_err)?;
        let wt = self.write_txn().map_err(write_err)?;
        {
            let mut t = wt.open_table(META).map_err(write_err)?;
            t.insert(key, bytes.as_slice()).map_err(write_err)?;
        }
        wt.commit().map_err(write_err)
    }

    fn last_log_id(&self) -> Result<Option<LogId>, StorageError> {
        let rt = self.read_txn().map_err(read_err)?;
        let t = rt.open_table(LOG).map_err(read_err)?;
        let last = t.last().map_err(read_err)?;
        let out = match &last {
            None => None,
            Some((_, v)) => Some(decode_entry(v.value()).map_err(read_err)?.log_id),
        };
        drop(last);
        Ok(out)
    }

    /// Delete `range` of indexes in one transaction.
    fn delete_range(&self, lo: Bound<u64>, hi: Bound<u64>) -> Result<(), StorageError> {
        let wt = self.write_txn().map_err(write_err)?;
        {
            let mut t = wt.open_table(LOG).map_err(write_err)?;
            let keys: Vec<u64> = t
                .range::<u64>((lo, hi))
                .map_err(write_err)?
                .map(|r| r.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()
                .map_err(write_err)?;
            for k in keys {
                t.remove(k).map_err(write_err)?;
            }
        }
        wt.commit().map_err(write_err)
    }

    /// Put `e` in the log directly (unit tests: openraft's flush callback
    /// cannot be built outside openraft).
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, e: &Entry) {
        let wt = self.write_txn().unwrap();
        wt.open_table(LOG)
            .unwrap()
            .insert(e.log_id.index, encode_entry(e).as_slice())
            .unwrap();
        wt.commit().unwrap();
    }

    /// The last log index: the last entry's, else the purge point's.
    pub fn last_index(&self) -> Result<Option<u64>, StoreError> {
        let io = |e: StorageError| StoreError::Storage(format!("raft log: {e}"));
        let last = self.last_log_id().map_err(io)?;
        let purged: Option<LogId> = self.meta(K_PURGED).map_err(io)?;
        Ok(last.or(purged).map(|l| l.index))
    }

    /// Entries in the log right now (tests and status).
    pub fn len(&self) -> Result<u64, StoreError> {
        let rt = self.read_txn()?;
        Ok(rt.open_table(LOG)?.len()?)
    }

    pub fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.len()? == 0)
    }
}

impl RaftLogReader<TypeConfig> for RedbLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, StorageError> {
        let rt = self.read_txn().map_err(read_err)?;
        let t = rt.open_table(LOG).map_err(read_err)?;
        let mut out = Vec::new();
        for row in t
            .range::<u64>((range.start_bound().cloned(), range.end_bound().cloned()))
            .map_err(read_err)?
        {
            let (_, v) = row.map_err(read_err)?;
            out.push(decode_entry(v.value()).map_err(read_err)?);
        }
        Ok(out)
    }
}

impl RaftLogStorage<TypeConfig> for RedbLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError> {
        let last_purged_log_id: Option<LogId> = self.meta(K_PURGED)?;
        let last_log_id = match self.last_log_id()? {
            Some(id) => Some(id),
            None => last_purged_log_id,
        };
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError> {
        let vote = *vote;
        self.blocking(move |s| {
            s.set_meta(K_VOTE, &vote)
                .map_err(|e| StorageIOError::write_vote(AnyError::new(&e)).into())
        })
        .await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError> {
        self.meta(K_VOTE)
            .map_err(|e| StorageIOError::read_vote(AnyError::new(&e)).into())
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        self.blocking(move |s| s.set_meta(K_COMMITTED, &committed))
            .await
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        self.meta(K_COMMITTED)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = <TypeConfig as RaftTypeConfig>::Entry> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<Entry> = entries.into_iter().collect();
        // The redb commit fsyncs: off the runtime workers (openraft awaits
        // this on its own task, so nothing else of the node waits on it).
        let written = self
            .blocking(move |s| {
                let wt = s.write_txn().map_err(write_err)?;
                let mut last = 0u64;
                {
                    let mut t = wt.open_table(LOG).map_err(write_err)?;
                    for e in &entries {
                        let enc = encode_entry(e);
                        last = e.log_id.index;
                        t.insert(e.log_id.index, enc.as_slice())
                            .map_err(write_err)?;
                    }
                }
                // `Immediate` durability: the commit returns once the
                // entries are fsynced, and only then is the flush reported
                // (D7).
                wt.commit().map_err(write_err)?;
                if let Some(o) = &s.observer {
                    o(AppendEvent::Committed { last_index: last });
                }
                Ok(last)
            })
            .await;
        match written {
            Ok(last_index) => {
                // The observer sees the flush report as a wrapper around the
                // callback: after the commit, right before openraft learns.
                if let Some(o) = &self.observer {
                    o(AppendEvent::Flushed { last_index });
                }
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(e) => {
                callback.log_io_completed(Err(std::io::Error::other(e.to_string())));
                Err(e)
            }
        }
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), StorageError> {
        self.blocking(move |s| s.delete_range(Bound::Included(log_id.index), Bound::Unbounded))
            .await
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        self.blocking(move |s| {
            // The purge point and the deletion commit together.
            let bytes = serde_json::to_vec(&log_id).map_err(write_err)?;
            let wt = s.write_txn().map_err(write_err)?;
            {
                let mut meta = wt.open_table(META).map_err(write_err)?;
                meta.insert(K_PURGED, bytes.as_slice()).map_err(write_err)?;
                let mut t = wt.open_table(LOG).map_err(write_err)?;
                let keys: Vec<u64> = t
                    .range::<u64>(..=log_id.index)
                    .map_err(write_err)?
                    .map(|r| r.map(|(k, _)| k.value()))
                    .collect::<Result<_, _>>()
                    .map_err(write_err)?;
                for k in keys {
                    t.remove(k).map_err(write_err)?;
                }
            }
            wt.commit().map_err(write_err)
        })
        .await?;
        // In the background: the Raft core awaits `purge`, and a
        // compaction may take long enough to delay its heartbeats past an
        // election timeout. At most one runs at a time.
        // Its clone holds no `Owner`: dropping the caller's last handle
        // waits for it (see `Owner`), so the file is closed on return.
        if self.compacting.start() {
            let mut s = self.clone();
            s.owner = None;
            let done = BusyGuard(Arc::clone(&self.compacting));
            tokio::task::spawn_blocking(move || {
                s.compact_if_sparse();
                // Release the database before saying it is done.
                drop(s);
                drop(done);
            });
        }
        Ok(())
    }
}

impl RedbLogStore {
    /// Run a committing (fsyncing) body on the blocking pool.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(RedbLogStore) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let s = self.clone();
        tokio::task::spawn_blocking(move || f(s))
            .await
            .map_err(|e| write_err(std::io::Error::other(format!("log task: {e}"))))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_golden_bytes() {
        let e = Entry {
            log_id: LogId::new(CommittedLeaderId::new(2, 1), 5),
            payload: EntryPayload::Normal(LogRequest {
                command: vec![0xAA, 0xBB],
            }),
        };
        let bytes = encode_entry(&e);
        let mut want = vec![1u8];
        want.extend_from_slice(&2u64.to_le_bytes());
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&5u64.to_le_bytes());
        want.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(bytes, want);
        assert_eq!(decode_entry(&bytes).unwrap(), e);
        let blank = Entry {
            log_id: LogId::new(CommittedLeaderId::new(0, 0), 0),
            payload: EntryPayload::Blank,
        };
        assert_eq!(encode_entry(&blank), vec![0u8; 25]);
        assert_eq!(decode_entry(&encode_entry(&blank)).unwrap(), blank);
        assert!(decode_entry(&[1, 2, 3]).is_err());
        let mut bad = encode_entry(&blank);
        bad[0] = 9;
        assert!(decode_entry(&bad).is_err());
    }

    #[test]
    fn membership_entry_round_trips() {
        let m = Membership::new(
            vec![[1u64].into_iter().collect()],
            [(1u64, openraft::impls::BasicNode::new("h:1"))]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
        );
        let e = Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 1),
            payload: EntryPayload::Membership(m),
        };
        assert_eq!(decode_entry(&encode_entry(&e)).unwrap(), e);
    }

    /// A purge that frees most of the file compacts it: `raft.redb` does
    /// not keep the high-water mark of the log (the size gate in
    /// `graph-cli/tests/size_gate.rs` checks the same end to end).
    /// 24 entries of 1 MiB each, written the way `append` writes them (one
    /// fsynced transaction per batch of 4).
    fn fill_24_mib(log: &RedbLogStore) {
        for batch in 0..6u64 {
            let wt = log.write_txn().unwrap();
            {
                let mut t = wt.open_table(LOG).unwrap();
                for i in 1..=4u64 {
                    let index = batch * 4 + i;
                    let e = Entry {
                        log_id: LogId::new(CommittedLeaderId::new(1, 1), index),
                        payload: EntryPayload::Normal(LogRequest {
                            command: vec![index as u8; 1 << 20],
                        }),
                    };
                    t.insert(index, encode_entry(&e).as_slice()).unwrap();
                }
            }
            wt.commit().unwrap();
        }
    }

    fn wait_compacted(log: &RedbLogStore) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while log.is_compacting() {
            assert!(
                std::time::Instant::now() < deadline,
                "the background compaction did not finish within 60 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn purge_compacts_a_mostly_free_log_and_keeps_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let mut log = RedbLogStore::open(&path).unwrap();
        fill_24_mib(&log);
        let before = log.file_bytes();
        assert!(before > 24 << 20, "{before}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let purged = LogId::new(CommittedLeaderId::new(1, 1), 22);
        rt.block_on(log.purge(purged)).unwrap();
        wait_compacted(&log);
        let after = log.file_bytes();
        // redb rounds a 1 MiB value up to a 2 MiB allocation and keeps its
        // region layout: measured 84 MB -> 11.7 MB (the two live entries
        // take 4 MiB of it).
        assert!(
            after < 12 << 20,
            "a purge of 22 of 24 MiB entries leaves {after} B (was {before} B)"
        );
        // Compacting again right away is not worth it.
        assert!(!log.compact_if_sparse());
        // The rest of the log and the purge point survive, also a reopen.
        drop(log);
        let mut log = RedbLogStore::open(&path).unwrap();
        let st = rt.block_on(log.get_log_state()).unwrap();
        assert_eq!(st.last_purged_log_id, Some(purged));
        assert_eq!(st.last_log_id.map(|l| l.index), Some(24));
        let rest = rt.block_on(log.try_get_log_entries(0..100)).unwrap();
        assert_eq!(
            rest.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            [23, 24]
        );
    }

    /// The compaction after a purge runs in the background: `purge`
    /// returns while it is held open (so the Raft core, which awaits
    /// `purge`, keeps sending heartbeats); a log reader on a clone of the
    /// store that holds a read transaction while it starts neither
    /// deadlocks nor sees a torn log; later appends and reads work.
    #[test]
    fn a_compaction_runs_beside_the_raft_core_without_deadlock() {
        use std::sync::mpsc;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let mut log = RedbLogStore::open(&path).unwrap();
        fill_24_mib(&log);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        // A reader (the log reader openraft hands to replication) holds a
        // read transaction when the compaction wants the write side.
        let reader = log.clone();
        let rtxn = reader.read_txn().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        log.set_compact_gate(Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        })));
        let purged = LogId::new(CommittedLeaderId::new(1, 1), 22);
        // `purge` returns although the compaction cannot even start yet.
        rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(20), log.purge(purged))
                .await
                .expect("purge returned while the compaction waits")
                .unwrap()
        });
        assert!(
            log.is_compacting(),
            "the compaction ended before the gate: {:?}",
            log.last_skip()
        );
        // The held read transaction still reads the whole log as of its
        // start (before the purge).
        assert_eq!(rtxn.open_table(LOG).unwrap().len().unwrap(), 24);
        drop(rtxn);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the compaction took the write side once the reader let go");
        release_tx.send(()).unwrap();
        wait_compacted(&log);
        // Appends and reads go on.
        log.insert_for_test(&Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 25),
            payload: EntryPayload::Blank,
        });
        let rest = rt.block_on(log.try_get_log_entries(0..100)).unwrap();
        assert_eq!(
            rest.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            [23, 24, 25]
        );
        log.set_compact_gate(None);
    }

    /// The stall bound: a purge that leaves more live log than the cap
    /// does not compact (the write lock would be held for a long rewrite);
    /// once the cap allows it, the next attempt does.
    #[test]
    fn a_live_log_above_the_cap_is_not_compacted() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let mut log = RedbLogStore::open(&path).unwrap();
        fill_24_mib(&log);
        // 4 entries (8 MiB of pages) stay live; the cap is 1 MiB.
        log.set_compact_max_live(1 << 20);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(log.purge(LogId::new(CommittedLeaderId::new(1, 1), 20)))
            .unwrap();
        wait_compacted(&log);
        let before = log.file_bytes();
        assert!(before > 24 << 20, "not compacted: {before} B");
        assert!(!log.compact_if_sparse(), "skipped above the cap");
        assert_eq!(log.file_bytes(), before);
        log.set_compact_max_live(COMPACT_MAX_LIVE);
        assert!(log.compact_if_sparse(), "compacts within the cap");
        assert!(log.file_bytes() < before / 2);
    }

    /// CI (Linux): dropping the log right after a purge, while its
    /// background compaction runs, must close the file before the drop
    /// returns, or a reopen finds it `Locked`. Deterministic: the gate
    /// holds the compaction open until the drop has begun waiting.
    #[test]
    fn dropping_the_log_during_a_compaction_closes_it_before_returning() {
        use std::sync::mpsc;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut log = RedbLogStore::open(&path).unwrap();
        fill_24_mib(&log);
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        log.set_compact_gate(Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        })));
        rt.block_on(log.purge(LogId::new(CommittedLeaderId::new(1, 1), 22)))
            .unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the compaction started");
        let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
        let dropper = std::thread::spawn(move || {
            drop(log);
            dropped_tx.send(()).unwrap();
        });
        // The drop cannot return while the compaction holds the file.
        assert!(dropped_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err());
        release_tx.send(()).unwrap();
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the drop returned once the compaction ended");
        dropper.join().unwrap();
        let log = RedbLogStore::open(&path).expect("reopened, not Locked");
        assert_eq!(log.len().unwrap(), 2);
        drop(log);
        // Without the gate: drop at once after each purge, reopen at once.
        for round in 0..5u64 {
            let mut log = RedbLogStore::open(&path).unwrap();
            fill_24_mib(&log);
            rt.block_on(log.purge(LogId::new(CommittedLeaderId::new(1, 1), 22)))
                .unwrap();
            drop(log);
            RedbLogStore::open(&path)
                .unwrap_or_else(|e| panic!("round {round}: reopen after a purge: {e:?}"));
        }
    }

    /// The snapshot policy's byte trigger reads what the log holds, so it
    /// survives a restart: a reopened log answers the same ranges.
    #[test]
    fn bytes_between_counts_the_entries_in_the_range() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let log = RedbLogStore::open(&path).unwrap();
        assert_eq!(log.bytes_between(0, u64::MAX).unwrap(), 0);
        let e = |i| Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), i),
            payload: EntryPayload::Normal(LogRequest {
                command: vec![7; 100],
            }),
        };
        log.insert_for_test(&e(1));
        log.insert_for_test(&e(2));
        log.insert_for_test(&e(3));
        drop(log);
        let log = RedbLogStore::open(&path).unwrap();
        assert_eq!(log.bytes_between(1, u64::MAX).unwrap(), 2 * 125);
        assert_eq!(log.bytes_between(1, 2).unwrap(), 125);
        assert_eq!(log.bytes_between(0, 3).unwrap(), 3 * 125);
        assert_eq!(log.bytes_between(3, 9).unwrap(), 0);
        assert_eq!(log.bytes_between(2, 1).unwrap(), 0);
    }
}
