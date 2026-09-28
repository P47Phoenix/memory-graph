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
use redb::{Database, ReadableTable, ReadableTableMetadata, StorageBackend, TableDefinition};
use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");
const K_VOTE: &str = "vote";
const K_COMMITTED: &str = "committed";
const K_PURGED: &str = "last_purged";

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

#[derive(Clone)]
pub struct RedbLogStore {
    db: Arc<Database>,
    path: PathBuf,
    /// Encoded bytes the log held when opened plus every append since (the
    /// snapshot policy's byte trigger, `--snapshot-log-bytes`).
    appended_bytes: Arc<AtomicU64>,
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
        Self::open_with(path, None)
    }

    /// [`open`](Self::open), over a test-only redb [`StorageBackend`]
    /// (the power-cut tests) instead of the file when `backend` is `Some`.
    pub fn open_with(
        path: &Path,
        backend: Option<Box<dyn StorageBackend>>,
    ) -> Result<Self, StoreError> {
        let opened = match backend {
            Some(b) => Database::builder().create_with_backend(b),
            None => Database::create(path),
        };
        let db = opened.map_err(|e| open_err(path, e))?;
        let wt = db.begin_write()?;
        wt.open_table(LOG)?;
        wt.open_table(META)?;
        wt.commit()?;
        let s = Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
            appended_bytes: Arc::new(AtomicU64::new(0)),
            observer: None,
        };
        // The byte trigger survives a restart: start from what the log
        // holds (the snapshot policy subtracts what lies at or below the
        // last snapshot, see `bytes_after`).
        let held = s.bytes_after(0)?;
        s.appended_bytes.store(held, Ordering::Relaxed);
        Ok(s)
    }

    /// Encoded bytes of the entries above `index` in the log right now.
    pub fn bytes_after(&self, index: u64) -> Result<u64, StoreError> {
        let rt = self.db.begin_read()?;
        let t = rt.open_table(LOG)?;
        let mut n = 0u64;
        for row in t.range::<u64>((Bound::Excluded(index), Bound::Unbounded))? {
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
        let rt = self.db.begin_read().map_err(read_err)?;
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
        let wt = self.db.begin_write().map_err(write_err)?;
        {
            let mut t = wt.open_table(META).map_err(write_err)?;
            t.insert(key, bytes.as_slice()).map_err(write_err)?;
        }
        wt.commit().map_err(write_err)
    }

    fn last_log_id(&self) -> Result<Option<LogId>, StorageError> {
        let rt = self.db.begin_read().map_err(read_err)?;
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
        let wt = self.db.begin_write().map_err(write_err)?;
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
        let rt = self.db.begin_read()?;
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
        let rt = self.db.begin_read().map_err(read_err)?;
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
                let wt = s.db.begin_write().map_err(write_err)?;
                let mut bytes = 0u64;
                let mut last = 0u64;
                {
                    let mut t = wt.open_table(LOG).map_err(write_err)?;
                    for e in &entries {
                        let enc = encode_entry(e);
                        bytes += enc.len() as u64;
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
                s.appended_bytes.fetch_add(bytes, Ordering::Relaxed);
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
            let wt = s.db.begin_write().map_err(write_err)?;
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
        .await
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
}
