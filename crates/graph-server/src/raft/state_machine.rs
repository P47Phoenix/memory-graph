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
//! (`LogResponse::Failed`), the same on every replica; the refused write
//! rolls back and the marker alone then moves to the entry in its own
//! transaction, so the store's marker always equals openraft's
//! `last_applied`. An I/O-class error (`Storage`, `Corrupt`, `Locked`) is
//! returned to openraft as a `StorageError`: it stops the node rather than
//! let one replica silently diverge; a restart replays from the marker.
//!
//! Snapshots: `build_snapshot` = `export_snapshot` to `<db>.snapshot.redb`
//! (one read transaction, so the copy is consistent and carries its own
//! marker, from which the metadata is read) plus a `<db>.snapshot.meta`
//! JSON; `install_snapshot` = `StoreSlot::install_snapshot` (close, rename,
//! reopen under the slot's write lock) and, only once that succeeded, the
//! installed file becomes the current snapshot and its meta is written. The
//! data file and the meta are each put in place by rename, data first; a
//! crash between the two leaves a meta whose `last_log_id` differs from the
//! file's own marker, and `get_current_snapshot` checks exactly that and
//! answers "no snapshot" (logged) rather than pair them.
//!
//! Where `rename` cannot replace an existing file (`replace_file`'s
//! fallback), the target is removed and then renamed into place: that pair
//! is not atomic, and a crash between the two leaves no current snapshot
//! file. `read_meta` treats a missing data file (or meta) as "no snapshot",
//! which is safe: openraft then builds a fresh one or ships the log.
//!
//! A snapshot build runs `export_snapshot` under the slot's **read** lock
//! for the length of the copy, so it blocks `compact` and a snapshot
//! install (which take the write lock) until it finishes; reads and writes
//! proceed.
//!
//! A mid-batch I/O error: `apply` gets entries in batches, and an I/O-class
//! error on one entry fails the whole call, so the responses already
//! produced for earlier entries of that batch (which did commit, with their
//! markers) are dropped. openraft stops the node; the clients of those
//! entries see an error (UNAVAILABLE/INTERNAL) for a write that landed.
//! That is safe to retry: `IndexChunk` is idempotent through file
//! fingerprints (an unchanged file is a no-op), `Prune` and `Vacuum` by
//! construction, and `IngestExtraction` when the extraction is identical,
//! which a retry of the same request is (ADR 0004 D2/D7).
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

/// Rename `from` over `to` (removing `to` first where the platform's
/// rename refuses to replace an existing file).
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => {
            std::fs::remove_file(to)?;
            std::fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
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
            Err(StoreError::AlreadyApplied { .. }) => Ok(LogResponse::Skipped),
            Err(e) if is_refusal(&e) => {
                tracing::warn!(index = marker.index, error = %e, "entry refused by the store");
                // The refused write rolled back; the entry is still applied
                // (its outcome is the refusal), so move the marker to it in
                // a transaction of its own. Otherwise openraft's
                // `last_applied` would run ahead of the store's marker: a
                // snapshot's `last_log_id` would be too low and a restart
                // after a purge would find `applied_state < last_purged`.
                Self::mark_refused(store, marker)?;
                Ok(LogResponse::Failed(ErrDetail::of(&e)))
            }
            Err(e) => Err(e),
        }
    }

    /// Stamp `marker` alone after a refused entry. Already-applied (a
    /// concurrent replay stamped it) is fine; an I/O error is fatal.
    fn mark_refused(store: &V2Store, marker: RaftMarker) -> Result<(), StoreError> {
        match store.mark_only(marker, None) {
            Ok(()) => Ok(()),
            Err(StoreError::AlreadyApplied { .. }) => Ok(()),
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

    /// The current snapshot's metadata, validated against the file it
    /// describes: a meta whose `last_log_id` differs from the snapshot
    /// file's own marker (a crash between the two renames of a build or an
    /// install) is treated as no snapshot, and logged, so a meta is never
    /// paired with a file it does not describe.
    fn read_meta(db: &Path) -> Result<Option<SnapshotMeta>, StorageError> {
        let path = meta_path(db);
        let file = snapshot_path(db);
        if !path.exists() || !file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(sm_read_err)?;
        let meta: SnapshotMeta = serde_json::from_str(&text).map_err(sm_read_err)?;
        let on_file = match V2Store::open(&file).and_then(|s| Self::read_applied(&s)) {
            Ok((last, _)) => last,
            Err(e) => {
                tracing::warn!(error = %e, file = %file.display(), "snapshot file unreadable; treated as no snapshot");
                return Ok(None);
            }
        };
        if on_file != meta.last_log_id {
            tracing::warn!(
                meta = ?meta.last_log_id,
                file = ?on_file,
                "snapshot meta does not match its file (interrupted build or install); treated as no snapshot"
            );
            return Ok(None);
        }
        Ok(Some(meta))
    }

    /// Write the meta to a temp file and rename it into place, so a crash
    /// never leaves a half-written meta.
    fn write_meta(db: &Path, meta: &SnapshotMeta) -> Result<(), StorageError> {
        let text = serde_json::to_string_pretty(meta).map_err(sm_err)?;
        let tmp = with_suffix(db, ".snapshot.meta.tmp");
        std::fs::write(&tmp, text).map_err(sm_err)?;
        replace_file(&tmp, &meta_path(db)).map_err(sm_err)
    }

    /// Blocking body of `install_snapshot`.
    fn install(
        slot: &StoreSlot,
        meta: &SnapshotMeta,
        data: &SnapshotFile,
    ) -> Result<(), StorageError> {
        Self::install_with(slot.path(), meta, data, |staged| {
            slot.install_snapshot(staged)
        })
    }

    /// `install` with the store swap as a parameter (tests inject a
    /// failure). The received file becomes the current snapshot, and its
    /// meta is written, only after the swap succeeded: a failed install
    /// leaves the previous current snapshot and meta untouched.
    fn install_with(
        db: &Path,
        meta: &SnapshotMeta,
        data: &SnapshotFile,
        swap: impl FnOnce(&Path) -> Result<(), StoreError>,
    ) -> Result<(), StorageError> {
        let cleanup_incoming = || {
            if data.path == incoming_path(db) {
                let _ = std::fs::remove_file(&data.path);
            }
        };
        let valid = graph_store::detect_format(&data.path)
            .map_err(sm_err)
            .and_then(|v| {
                v.ok_or_else(|| {
                    sm_err(StoreError::Rejected(format!(
                        "`{}` is not a store file",
                        data.path.display()
                    )))
                })
            });
        if let Err(e) = valid {
            cleanup_incoming();
            return Err(e);
        }
        // The swap consumes its source: install a copy.
        let staged = with_suffix(db, ".snapshot.install.redb");
        let _ = std::fs::remove_file(&staged);
        let r = std::fs::copy(&data.path, &staged)
            .map_err(sm_err)
            .and_then(|_| swap(&staged).map_err(sm_err));
        let _ = std::fs::remove_file(&staged);
        if let Err(e) = r {
            cleanup_incoming();
            return Err(e);
        }
        // Promote: data file first, then its meta (each by rename).
        let current = snapshot_path(db);
        if data.path != current {
            let tmp = with_suffix(db, ".snapshot.promote.redb");
            let _ = std::fs::remove_file(&tmp);
            let promoted = std::fs::copy(&data.path, &tmp)
                .and_then(|_| replace_file(&tmp, &current))
                .map_err(sm_err);
            cleanup_incoming();
            promoted?;
        }
        Self::write_meta(db, meta)
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
        // Data file first, then its meta, each by rename; a crash between
        // the two leaves a meta that no longer matches the file, which
        // `read_meta` detects and treats as no snapshot.
        replace_file(&tmp, &current).map_err(sm_err)?;
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

    fn log_id(index: u64) -> LogId {
        LogId::new(openraft::CommittedLeaderId::new(1, 1), index)
    }

    fn blank(index: u64) -> Entry {
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Blank,
        }
    }

    /// A normal entry the store refuses deterministically (undecodable).
    fn refused(index: u64) -> Entry {
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Normal(super::super::types::LogRequest {
                command: vec![0x0a, 0xff],
            }),
        }
    }

    fn open_slot(db: &Path) -> Arc<StoreSlot> {
        StoreSlot::open(db, vec![], None, Duration::from_secs(900)).unwrap()
    }

    #[test]
    fn a_refused_entry_moves_the_marker_to_its_log_id() {
        let d = tempfile::tempdir().unwrap();
        let slot = open_slot(&d.path().join("g.redb"));
        let out = StoreStateMachine::apply_all(&slot, vec![blank(1), refused(2)]).unwrap();
        assert_eq!(out[0], LogResponse::Marked);
        assert!(matches!(out[1], LogResponse::Failed(_)), "{:?}", out[1]);
        let marker = slot.with_store(|s| s.raft_marker()).unwrap().unwrap();
        assert_eq!(log_id_of(marker), log_id(2));
        // A replay of the refused entry is a skip, not a second refusal.
        let again = StoreStateMachine::apply_all(&slot, vec![refused(2)]).unwrap();
        assert_eq!(again, vec![LogResponse::Skipped]);
    }

    /// The review's blocker scenario: a refusal is the last applied entry,
    /// then a snapshot is built, the log purged up to it, and the node
    /// restarted: the store's applied state, openraft's last applied and
    /// the snapshot's last log id all agree, and none is below the purge.
    #[tokio::test]
    async fn refusal_then_snapshot_purge_restart_keeps_applied_state_consistent() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let lp = crate::raft::log_store::log_path(&db);
        {
            let slot = open_slot(&db);
            let mut sm = StoreStateMachine::new(Arc::clone(&slot));
            let out = sm.apply(vec![blank(1), refused(2)]).await.unwrap();
            assert!(matches!(out[1], LogResponse::Failed(_)));
            let mut b = sm.get_snapshot_builder().await;
            let snap = b.build_snapshot().await.unwrap();
            assert_eq!(snap.meta.last_log_id, Some(log_id(2)));
            let mut log = RedbLogStore::open(&lp).unwrap();
            openraft::storage::RaftLogStorage::purge(&mut log, log_id(2))
                .await
                .unwrap();
            slot.close();
        }
        // Restart.
        let slot = open_slot(&db);
        let mut sm = StoreStateMachine::new(slot);
        let (applied, _) = sm.applied_state().await.unwrap();
        let mut log = RedbLogStore::open(&lp).unwrap();
        let state = openraft::storage::RaftLogStorage::get_log_state(&mut log)
            .await
            .unwrap();
        let snap = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(applied, Some(log_id(2)));
        assert_eq!(state.last_purged_log_id, Some(log_id(2)));
        assert_eq!(snap.meta.last_log_id, Some(log_id(2)));
    }

    /// A store file at `path` whose marker is `index`.
    fn marked_file(path: &Path, index: u64) {
        let s = V2Store::open(path).unwrap();
        s.mark_only(marker_of(&log_id(index)), None).unwrap();
    }

    fn meta_at(index: u64) -> SnapshotMeta {
        SnapshotMeta {
            last_log_id: Some(log_id(index)),
            last_membership: StoredMembership::default(),
            snapshot_id: format!("s{index}"),
        }
    }

    /// A slot whose current snapshot (built) is at index 1.
    fn slot_with_snapshot_at_1(db: &Path) -> Arc<StoreSlot> {
        let slot = open_slot(db);
        StoreStateMachine::apply_all(&slot, vec![blank(1)]).unwrap();
        let snap = SnapshotBuilder::build(&slot).unwrap();
        assert_eq!(snap.meta.last_log_id, Some(log_id(1)));
        slot
    }

    #[test]
    fn a_failed_install_leaves_the_current_snapshot_and_meta_untouched() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let _slot = slot_with_snapshot_at_1(&db);
        let meta_before = std::fs::read(meta_path(&db)).unwrap();
        let incoming = incoming_path(&db);
        marked_file(&incoming, 5);
        let r = StoreStateMachine::install_with(
            &db,
            &meta_at(5),
            &SnapshotFile {
                path: incoming.clone(),
            },
            |_| Err(StoreError::Storage("injected install failure".into())),
        );
        assert!(r.is_err());
        assert_eq!(std::fs::read(meta_path(&db)).unwrap(), meta_before);
        let meta = StoreStateMachine::read_meta(&db).unwrap().unwrap();
        assert_eq!(meta.last_log_id, Some(log_id(1)));
        assert!(!incoming.exists(), "the received file is cleaned up");
    }

    #[test]
    fn a_successful_install_promotes_the_file_then_its_meta() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let _slot = slot_with_snapshot_at_1(&db);
        let incoming = incoming_path(&db);
        marked_file(&incoming, 5);
        let mut swapped = false;
        StoreStateMachine::install_with(&db, &meta_at(5), &SnapshotFile { path: incoming }, |_| {
            swapped = true;
            Ok(())
        })
        .unwrap();
        assert!(swapped);
        let meta = StoreStateMachine::read_meta(&db).unwrap().unwrap();
        assert_eq!(meta.last_log_id, Some(log_id(5)));
    }

    /// The crash window of a build or install (data renamed, meta not yet):
    /// the stale meta is never paired with the new file.
    #[test]
    fn a_meta_that_does_not_match_its_file_is_no_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let _slot = slot_with_snapshot_at_1(&db);
        std::fs::remove_file(snapshot_path(&db)).unwrap();
        marked_file(&snapshot_path(&db), 5);
        assert!(StoreStateMachine::read_meta(&db).unwrap().is_none());
    }
}
