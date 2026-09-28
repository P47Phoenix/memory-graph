//! [`StoreStateMachine`]: the store as openraft's state machine (ADR 0004
//! D5/D7).
//!
//! Apply: every entry becomes exactly one store transaction through the
//! marked writes (`index_prepared_marked` and friends, chunking forced to
//! one transaction), which also records the entry's log id in `RAFT_SM`.
//! An entry at or below the stored marker is skipped but still answered
//! (openraft wants one response per entry), so a restart replay is
//! exactly-once. `applied_state` reads the marker back.
//!
//! Errors: a deterministic refusal by the store (a NUL in a language, an
//! invalid span: `Rejected` and its kin) is the entry's outcome
//! (`LogResponse::Failed`), the same on every replica, and the marker does
//! not move for it. An I/O-class error (`Storage`, `Corrupt`, `Locked`) is
//! returned to openraft as a `StorageError`: it stops the node rather than
//! let one replica silently diverge; a restart replays from the marker.
//!
//! Snapshots: `build_snapshot` = `export_snapshot` to `<db>.snapshot.redb`
//! (one read transaction, so the copy is consistent and carries its own
//! marker, from which the metadata is read) plus a `<db>.snapshot.meta`
//! JSON; `install_snapshot` = `StoreSlot::install_snapshot` (close, rename,
//! reopen under the slot's write lock) and the installed file becomes the
//! current snapshot.
use super::types::{
    log_id_of, marker_of, Entry, ErrDetail, LogId, LogResponse, SnapshotFile, SnapshotMeta,
    StorageError, StorageIOError, StoredMembership, TypeConfig,
};
use crate::slot::StoreSlot;
use graph_proto::pb;
use graph_store::{IndexOptions, RaftMarker, StoreError, V2Store};
use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine};
use openraft::{AnyError, EntryPayload, RaftTypeConfig, Snapshot};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct StoreStateMachine {
    slot: Arc<StoreSlot>,
}

/// Where a store's current snapshot and its metadata live.
pub fn snapshot_path(db: &Path) -> PathBuf {
    with_suffix(db, ".snapshot.redb")
}

fn meta_path(db: &Path) -> PathBuf {
    with_suffix(db, ".snapshot.meta")
}

fn incoming_path(db: &Path) -> PathBuf {
    with_suffix(db, ".snapshot.incoming.redb")
}

fn with_suffix(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn sm_err(e: impl std::error::Error + 'static) -> StorageError {
    StorageIOError::write_state_machine(AnyError::new(&e)).into()
}

fn sm_read_err(e: impl std::error::Error + 'static) -> StorageError {
    StorageIOError::read_state_machine(AnyError::new(&e)).into()
}

/// Whether a store error is a deterministic refusal (the entry's outcome)
/// rather than an I/O failure (fatal for this node).
fn is_refusal(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::Rejected(_)
            | StoreError::NotUtf8(_)
            | StoreError::TooLarge(_)
            | StoreError::InvalidSpan(_)
            | StoreError::Schema(_)
            | StoreError::Protocol(_)
    )
}

impl StoreStateMachine {
    pub fn new(slot: Arc<StoreSlot>) -> Self {
        Self { slot }
    }

    /// The state the marker records: `(last applied, membership)`.
    pub fn read_applied(store: &V2Store) -> Result<(Option<LogId>, StoredMembership), StoreError> {
        let last = store.raft_marker()?.map(log_id_of);
        let membership = match store.raft_membership()? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("raft membership in the store: {e}")))?,
            None => StoredMembership::default(),
        };
        Ok((last, membership))
    }

    /// Apply `entries` in order; the blocking body of `apply`.
    fn apply_all(slot: &StoreSlot, entries: Vec<Entry>) -> Result<Vec<LogResponse>, StorageError> {
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let r = slot.with_store(|s| Self::apply_one(s, &e)).map_err(|err| {
                StorageError::from(StorageIOError::apply(e.log_id, AnyError::new(&err)))
            })?;
            out.push(r);
        }
        Ok(out)
    }

    /// One entry: `Ok(response)` including a `Failed` refusal; `Err` only
    /// for an I/O-class error.
    fn apply_one(store: &V2Store, e: &Entry) -> Result<LogResponse, StoreError> {
        let marker = marker_of(&e.log_id);
        if store
            .raft_marker()?
            .is_some_and(|cur| cur.index >= marker.index)
        {
            tracing::debug!(index = marker.index, "entry already applied; skipped");
            return Ok(LogResponse::Skipped);
        }
        let r = match &e.payload {
            EntryPayload::Blank => store.mark_only(marker, None).map(|()| LogResponse::Marked),
            EntryPayload::Membership(m) => {
                let stored = StoredMembership::new(Some(e.log_id), m.clone());
                let bytes = serde_json::to_vec(&stored)
                    .map_err(|e| StoreError::Protocol(format!("membership: {e}")))?;
                store
                    .mark_only(marker, Some(&bytes))
                    .map(|()| LogResponse::Marked)
            }
            EntryPayload::Normal(req) => match req.decode() {
                Ok(cmd) => Self::apply_command(store, cmd, marker),
                Err(e) => Err(e),
            },
        };
        match r {
            Ok(resp) => Ok(resp),
            // A concurrent replay stamped it first: same as skipped.
            Err(StoreError::Rejected(m)) if m.starts_with("raft marker ") => {
                Ok(LogResponse::Skipped)
            }
            Err(e) if is_refusal(&e) => {
                tracing::warn!(index = marker.index, error = %e, "entry refused by the store");
                Ok(LogResponse::Failed(ErrDetail::of(&e)))
            }
            Err(e) => Err(e),
        }
    }

    fn apply_command(
        store: &V2Store,
        cmd: pb::LogCommand,
        marker: RaftMarker,
    ) -> Result<LogResponse, StoreError> {
        use pb::log_command::Cmd;
        let Some(cmd) = cmd.cmd else {
            return Err(StoreError::Protocol("empty LogCommand".into()));
        };
        match cmd {
            Cmd::IndexChunk(c) => {
                let opts = IndexOptions { reindex: c.reindex };
                let prepared = c
                    .files
                    .iter()
                    .map(|f| {
                        graph_store::Store::prepare(
                            store,
                            &c.org,
                            &c.repo,
                            &f.as_batch_file(),
                            opts,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let results =
                    store.index_prepared_marked(&c.org, &c.repo, prepared, opts, marker, None)?;
                Ok(LogResponse::Index(
                    results
                        .into_iter()
                        .map(|r| r.map_err(|e| ErrDetail::of(&e)))
                        .collect(),
                ))
            }
            Cmd::IngestExtraction(c) => {
                let ex: graph_core::Extraction = c
                    .extraction
                    .ok_or_else(|| {
                        StoreError::Protocol("IngestExtraction.extraction is missing".into())
                    })?
                    .try_into()
                    .map_err(|e: graph_proto::ConvertError| StoreError::Protocol(e.to_string()))?;
                let stats = store.ingest_file_marked(
                    &c.org,
                    &c.repo,
                    &c.path,
                    &c.language,
                    &ex,
                    c.origin.as_deref(),
                    marker,
                    None,
                )?;
                Ok(LogResponse::Ingest(stats))
            }
            Cmd::Prune(c) => {
                let keep: HashSet<String> = c.keep.into_iter().collect();
                let removed = store.prune_files_marked(&c.org, &c.repo, &keep, marker, None)?;
                Ok(LogResponse::Prune(removed))
            }
            Cmd::Vacuum(_) => {
                let stats = store.vacuum_marked(marker, None)?;
                Ok(LogResponse::Vacuum(stats.into()))
            }
            Cmd::Noop(_) => store.mark_only(marker, None).map(|()| LogResponse::Marked),
        }
    }

    fn read_meta(db: &Path) -> Result<Option<SnapshotMeta>, StorageError> {
        let path = meta_path(db);
        if !path.exists() || !snapshot_path(db).exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(sm_read_err)?;
        serde_json::from_str(&text).map(Some).map_err(sm_read_err)
    }

    fn write_meta(db: &Path, meta: &SnapshotMeta) -> Result<(), StorageError> {
        let text = serde_json::to_string_pretty(meta).map_err(sm_err)?;
        std::fs::write(meta_path(db), text).map_err(sm_err)
    }

    /// Blocking body of `install_snapshot`.
    fn install(
        slot: &StoreSlot,
        meta: &SnapshotMeta,
        data: &SnapshotFile,
    ) -> Result<(), StorageError> {
        let db = slot.path();
        graph_store::detect_format(&data.path)
            .map_err(sm_err)?
            .ok_or_else(|| {
                sm_err(StoreError::Rejected(format!(
                    "`{}` is not a store file",
                    data.path.display()
                )))
            })?;
        // Keep the received file as the current snapshot, and install a
        // copy (the install consumes its source).
        let current = snapshot_path(db);
        if data.path != current {
            std::fs::copy(&data.path, &current).map_err(sm_err)?;
        }
        Self::write_meta(db, meta)?;
        let staged = with_suffix(db, ".snapshot.install.redb");
        let _ = std::fs::remove_file(&staged);
        std::fs::copy(&current, &staged).map_err(sm_err)?;
        let r = slot.install_snapshot(&staged).map_err(sm_err);
        let _ = std::fs::remove_file(&staged);
        if data.path == incoming_path(db) {
            let _ = std::fs::remove_file(&data.path);
        }
        r
    }
}

impl RaftStateMachine<TypeConfig> for StoreStateMachine {
    type SnapshotBuilder = SnapshotBuilder;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), StorageError> {
        self.slot
            .with_store(Self::read_applied)
            .map_err(sm_read_err)
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<LogResponse>, StorageError>
    where
        I: IntoIterator<Item = <TypeConfig as RaftTypeConfig>::Entry> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<Entry> = entries.into_iter().collect();
        let slot = Arc::clone(&self.slot);
        // Parsing and committing block; keep them off the runtime workers.
        tokio::task::spawn_blocking(move || Self::apply_all(&slot, entries))
            .await
            .map_err(|e| sm_err(std::io::Error::other(format!("apply task: {e}"))))?
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            slot: Arc::clone(&self.slot),
        }
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<SnapshotFile>, StorageError> {
        let path = incoming_path(self.slot.path());
        let _ = std::fs::remove_file(&path);
        Ok(Box::new(SnapshotFile { path }))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<SnapshotFile>,
    ) -> Result<(), StorageError> {
        let slot = Arc::clone(&self.slot);
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || Self::install(&slot, &meta, &snapshot))
            .await
            .map_err(|e| sm_err(std::io::Error::other(format!("install task: {e}"))))?
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError> {
        let db = self.slot.path();
        Ok(Self::read_meta(db)?.map(|meta| Snapshot {
            meta,
            snapshot: Box::new(SnapshotFile {
                path: snapshot_path(db),
            }),
        }))
    }
}

pub struct SnapshotBuilder {
    slot: Arc<StoreSlot>,
}

impl SnapshotBuilder {
    /// Blocking body of `build_snapshot`.
    fn build(slot: &StoreSlot) -> Result<Snapshot<TypeConfig>, StorageError> {
        let db = slot.path();
        let tmp = with_suffix(db, ".snapshot.tmp.redb");
        let _ = std::fs::remove_file(&tmp);
        slot.with_store(|s| s.export_snapshot(&tmp))
            .map_err(sm_err)?;
        // The copy's own marker is the snapshot's state (the live store
        // may already have moved on).
        let (last_log_id, last_membership) = {
            let copy = V2Store::open(&tmp).map_err(sm_read_err)?;
            StoreStateMachine::read_applied(&copy).map_err(sm_read_err)?
        };
        let current = snapshot_path(db);
        let _ = std::fs::remove_file(&current);
        std::fs::rename(&tmp, &current).map_err(sm_err)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let snapshot_id = match &last_log_id {
            Some(id) => format!(
                "{}-{}-{}-{nanos}",
                id.leader_id.term, id.leader_id.node_id, id.index
            ),
            None => format!("empty-{nanos}"),
        };
        let meta = SnapshotMeta {
            last_log_id,
            last_membership,
            snapshot_id,
        };
        StoreStateMachine::write_meta(db, &meta)?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(SnapshotFile { path: current }),
        })
    }
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError> {
        let slot = Arc::clone(&self.slot);
        tokio::task::spawn_blocking(move || Self::build(&slot))
            .await
            .map_err(|e| sm_err(std::io::Error::other(format!("snapshot task: {e}"))))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::log_store::RedbLogStore;
    use openraft::testing::{StoreBuilder, Suite};
    use std::time::Duration;

    /// One fresh store pair per test, in its own temp dir.
    struct Builder;

    impl StoreBuilder<TypeConfig, RedbLogStore, StoreStateMachine, tempfile::TempDir> for Builder {
        async fn build(
            &self,
        ) -> Result<(tempfile::TempDir, RedbLogStore, StoreStateMachine), StorageError> {
            let d = tempfile::tempdir().unwrap();
            let db = d.path().join("g.redb");
            let log = RedbLogStore::open(&crate::raft::log_store::log_path(&db)).unwrap();
            let slot = StoreSlot::open(&db, vec![], None, Duration::from_secs(900)).unwrap();
            Ok((d, log, StoreStateMachine::new(slot)))
        }
    }

    /// openraft's own storage conformance suite over the log store and the
    /// state machine (ADR 0004 test plan, unit layer).
    #[test]
    fn openraft_storage_suite() {
        Suite::test_all(Builder).unwrap();
    }

    #[test]
    fn refusals_are_entry_outcomes_and_io_errors_are_not() {
        assert!(is_refusal(&StoreError::Rejected("x".into())));
        assert!(is_refusal(&StoreError::InvalidSpan("x".into())));
        assert!(!is_refusal(&StoreError::Storage("x".into())));
        assert!(!is_refusal(&StoreError::Corrupt("x".into())));
        assert!(!is_refusal(&StoreError::Locked("x".into())));
    }
}
