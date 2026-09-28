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
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");
const K_VOTE: &str = "vote";
const K_COMMITTED: &str = "committed";
const K_PURGED: &str = "last_purged";

/// Free space a purge may leave in `raft.redb` before it is compacted
/// (beyond the live pages again): see [`RedbLogStore::compact_if_sparse`].
const COMPACT_SLACK: u64 = 1 << 20;

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

#[derive(Clone)]
pub struct RedbLogStore {
    /// Every transaction holds the read side; the post-purge compaction
    /// takes the write side, so no transaction of ours is open while redb
    /// moves pages.
    db: Arc<RwLock<Database>>,
    path: PathBuf,
    /// Encoded bytes appended since this process opened the log (the
    /// snapshot policy's byte trigger, `--snapshot-log-bytes`).
    appended_bytes: Arc<AtomicU64>,
    /// The file size right after the last compaction (0: none yet).
    compacted_bytes: Arc<AtomicU64>,
    observer: Option<AppendObserver>,
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
        let db = Database::create(path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => {
                StoreError::Locked(path.display().to_string())
            }
            e => StoreError::OpenFailed {
                path: path.display().to_string(),
                reason: e.to_string(),
            },
        })?;
        let wt = db.begin_write()?;
        wt.open_table(LOG)?;
        wt.open_table(META)?;
        wt.commit()?;
        Ok(Self {
            db: Arc::new(RwLock::new(db)),
            path: path.to_path_buf(),
            appended_bytes: Arc::new(AtomicU64::new(0)),
            compacted_bytes: Arc::new(AtomicU64::new(0)),
            observer: None,
        })
    }

    /// The database for one transaction. The guard must outlive the
    /// transaction (bind it first), so [`Self::compact_if_sparse`] can
    /// rely on no transaction of ours being open.
    fn db(&self) -> RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Shrink the file once a purge left it mostly free pages: redb reuses
    /// freed pages but never returns them to the file system, so without
    /// this `raft.redb` keeps the high-water mark of every entry ever
    /// between two snapshots (measured: a corpus index left 3.4x the source
    /// on disk after the snapshot purged the whole log).
    ///
    /// Runs only when the file is at least twice its live pages plus
    /// [`COMPACT_SLACK`], so a steady state does not compact on every
    /// purge. `Database::compact` commits with two-phase commits (crash
    /// safe: an interrupted compaction leaves a valid file); it holds the
    /// write side of the lock, so appends wait for it, which is bounded by
    /// the entries left after the purge (`--log-keep-entries`). Returns
    /// whether it compacted; a failure is logged, never fatal (the log is
    /// intact either way).
    pub fn compact_if_sparse(&self) -> bool {
        let file = self.file_bytes();
        let live = {
            let db = self.db();
            // A purge's pages are released only by later commits: two
            // empty ones (as `compact` does itself) make `allocated_pages`
            // exact. Purges are rare (one per snapshot), so the two small
            // fsyncs are cheap.
            let release = || -> Result<(), redb::Error> {
                for _ in 0..2 {
                    db.begin_write()?.commit()?;
                }
                Ok(())
            };
            let stats = release().and_then(|()| {
                let wt = db.begin_write()?;
                let s = wt.stats();
                wt.abort()?;
                Ok(s?)
            });
            match stats {
                Ok(s) => s.allocated_pages() * s.page_size() as u64,
                Err(e) => {
                    tracing::warn!(error = %e, "raft log: reading page stats failed");
                    return false;
                }
            }
        };
        // Not worth it while the file is within twice its live data, nor
        // when it has barely grown since the last compaction (redb keeps a
        // floor: its region layout and power-of-two value allocations).
        let floor = self.compacted_bytes.load(Ordering::Relaxed);
        if file < live.saturating_mul(2).saturating_add(COMPACT_SLACK)
            || file < floor.saturating_add(COMPACT_SLACK)
        {
            return false;
        }
        let mut db = self.db.write().unwrap_or_else(|p| p.into_inner());
        let mut rounds = 0;
        loop {
            match db.compact() {
                Ok(true) if rounds < 8 => rounds += 1,
                Ok(_) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "raft log: compaction skipped");
                    break;
                }
            }
        }
        drop(db);
        let after = self.file_bytes();
        self.compacted_bytes.store(after, Ordering::Relaxed);
        tracing::info!(
            before = file,
            after,
            live,
            "raft log compacted after a purge"
        );
        true
    }

    /// Record every append's commit and flush callback (tests only).
    pub fn set_observer(&mut self, observer: Option<AppendObserver>) {
        self.observer = observer;
    }

    /// Encoded entry bytes appended since the log was opened.
    pub fn appended_bytes(&self) -> u64 {
        self.appended_bytes.load(Ordering::Relaxed)
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
        let db = self.db();
        let rt = db.begin_read().map_err(read_err)?;
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
        let db = self.db();
        let wt = db.begin_write().map_err(write_err)?;
        {
            let mut t = wt.open_table(META).map_err(write_err)?;
            t.insert(key, bytes.as_slice()).map_err(write_err)?;
        }
        wt.commit().map_err(write_err)
    }

    fn last_log_id(&self) -> Result<Option<LogId>, StorageError> {
        let db = self.db();
        let rt = db.begin_read().map_err(read_err)?;
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
        let db = self.db();
        let wt = db.begin_write().map_err(write_err)?;
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

    /// Entries in the log right now (tests and status).
    pub fn len(&self) -> Result<u64, StoreError> {
        let db = self.db();
        let rt = db.begin_read()?;
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
        let db = self.db();
        let rt = db.begin_read().map_err(read_err)?;
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
        self.set_meta(K_VOTE, vote)
            .map_err(|e| StorageIOError::write_vote(AnyError::new(&e)).into())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError> {
        self.meta(K_VOTE)
            .map_err(|e| StorageIOError::read_vote(AnyError::new(&e)).into())
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        self.set_meta(K_COMMITTED, &committed)
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
        let write = || -> Result<(u64, u64), StorageError> {
            let db = self.db();
            let wt = db.begin_write().map_err(write_err)?;
            let mut bytes = 0u64;
            let mut last = 0u64;
            {
                let mut t = wt.open_table(LOG).map_err(write_err)?;
                for e in entries {
                    let enc = encode_entry(&e);
                    bytes += enc.len() as u64;
                    last = e.log_id.index;
                    t.insert(e.log_id.index, enc.as_slice())
                        .map_err(write_err)?;
                }
            }
            // `Immediate` durability: the commit returns once the entries
            // are fsynced, and only then is the flush reported (D7).
            wt.commit().map_err(write_err)?;
            Ok((bytes, last))
        };
        match write() {
            Ok((bytes, last_index)) => {
                self.appended_bytes.fetch_add(bytes, Ordering::Relaxed);
                if let Some(o) = &self.observer {
                    o(AppendEvent::Committed { last_index });
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
        self.delete_range(Bound::Included(log_id.index), Bound::Unbounded)
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        // The purge point and the deletion commit together.
        let bytes = serde_json::to_vec(&log_id).map_err(write_err)?;
        let db = self.db();
        let wt = db.begin_write().map_err(write_err)?;
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
        wt.commit().map_err(write_err)?;
        drop(db);
        self.compact_if_sparse();
        Ok(())
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
    #[test]
    fn purge_compacts_a_mostly_free_log_and_keeps_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("raft.redb");
        let mut log = RedbLogStore::open(&path).unwrap();
        // 24 entries of 1 MiB each, written the way `append` writes them
        // (one fsynced transaction per batch).
        for batch in 0..6u64 {
            let db = log.db();
            let wt = db.begin_write().unwrap();
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
        let before = log.file_bytes();
        assert!(before > 24 << 20, "{before}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let purged = LogId::new(CommittedLeaderId::new(1, 1), 22);
        rt.block_on(log.purge(purged)).unwrap();
        let after = log.file_bytes();
        // redb rounds a 1 MiB value up to a 2 MiB allocation and keeps its
        // region layout: measured 84 MB -> 11.7 MB.
        assert!(
            after < 16 << 20,
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
}
