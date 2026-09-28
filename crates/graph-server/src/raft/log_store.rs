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

#[derive(Clone)]
pub struct RedbLogStore {
    db: Arc<Database>,
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
        Ok(Self { db: Arc::new(db) })
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
        let write = || -> Result<(), StorageError> {
            let wt = self.db.begin_write().map_err(write_err)?;
            {
                let mut t = wt.open_table(LOG).map_err(write_err)?;
                for e in entries {
                    t.insert(e.log_id.index, encode_entry(&e).as_slice())
                        .map_err(write_err)?;
                }
            }
            // `Immediate` durability: the commit returns once the entries
            // are fsynced, and only then is the flush reported (D7).
            wt.commit().map_err(write_err)
        };
        match write() {
            Ok(()) => {
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
        // Record the purge point first, then delete: a crash in between
        // leaves entries that are simply below the purge point.
        self.set_meta(K_PURGED, &log_id)?;
        self.delete_range(Bound::Unbounded, Bound::Included(log_id.index))
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
