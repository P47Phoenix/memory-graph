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
//! Snapshots live in a [`SnapshotDir`] (`snapshots/snap-<term>-<index>.redb`
//! plus a `.meta`; see that module for the crash-safety argument):
//! `build_snapshot` = `export_snapshot` (one read transaction, so the copy
//! is consistent and carries its own marker, from which the metadata is
//! read); `install_snapshot` = `StoreSlot::install_snapshot` (close, rename,
//! reopen under the slot's write lock) and, only once that succeeded, the
//! received file becomes the current snapshot. A build runs
//! `export_snapshot` under the slot's **read** lock for the length of the
//! copy, so it blocks `compact` and a snapshot install (which take the
//! write lock) until it finishes; reads and writes proceed.
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
use super::snapshot_dir::SnapshotDir;
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
use std::sync::Arc;

/// Test-only failpoints of the state machine ([`crate::TestingHooks`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SmFailpoints {
    /// Fail `apply` (an I/O-class error: openraft stops the node) just
    /// before the entry at this index is applied: it is in the log and
    /// committed, and nothing of it is in the store.
    pub fail_before_apply: Option<u64>,
}

pub struct StoreStateMachine {
    slot: Arc<StoreSlot>,
    snapshots: Arc<SnapshotDir>,
    failpoints: SmFailpoints,
    obs: Arc<crate::observe::Observability>,
    testing_apply_gate: Option<TestingApplyGate>,
}

/// Testing only, not a supported API (#[doc(hidden)]): called (on the
/// blocking pool) with each entry's log index just before `apply` applies
/// it; a test blocks in it to hold the applied index below the committed
/// one while the node keeps receiving and committing entries (heartbeats
/// included): a slow apply, deterministically.
#[doc(hidden)]
pub type TestingApplyGate = Arc<dyn Fn(u64) + Send + Sync>;

pub use super::snapshot_dir::replace_file;

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
            | StoreError::Binary(_)
            | StoreError::TooLarge(_)
            | StoreError::InvalidSpan(_)
            | StoreError::Schema(_)
            | StoreError::Protocol(_)
    )
}

impl StoreStateMachine {
    pub fn new(slot: Arc<StoreSlot>, snapshots: Arc<SnapshotDir>) -> Self {
        Self {
            slot,
            snapshots,
            failpoints: SmFailpoints::default(),
            obs: Arc::default(),
            testing_apply_gate: None,
        }
    }

    /// Testing only: see [`TestingApplyGate`].
    #[doc(hidden)]
    pub fn with_testing_apply_gate(mut self, gate: Option<TestingApplyGate>) -> Self {
        self.testing_apply_gate = gate;
        self
    }

    /// Record apply timings (and spans) into `obs`.
    pub fn with_obs(mut self, obs: Arc<crate::observe::Observability>) -> Self {
        self.obs = obs;
        self
    }

    pub fn with_failpoints(mut self, failpoints: SmFailpoints) -> Self {
        self.failpoints = failpoints;
        self
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
    fn apply_all(
        slot: &StoreSlot,
        entries: Vec<Entry>,
        failpoints: SmFailpoints,
        obs: &crate::observe::Observability,
        gate: Option<&TestingApplyGate>,
    ) -> Result<Vec<LogResponse>, StorageError> {
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            if let Some(g) = gate {
                g(e.log_id.index);
            }
            if failpoints.fail_before_apply == Some(e.log_id.index) {
                tracing::warn!(index = e.log_id.index, "testing: failpoint before apply");
                let err = StoreError::Storage(format!(
                    "testing: failpoint before applying entry {}",
                    e.log_id.index
                ));
                return Err(StorageIOError::apply(e.log_id, AnyError::new(&err)).into());
            }
            // One `apply` span per entry: index, command kind and file
            // count (recorded where the payload is decoded), duration.
            let span = tracing::debug_span!(
                "apply",
                index = e.log_id.index,
                kind = tracing::field::Empty,
                files = tracing::field::Empty,
                duration_ms = tracing::field::Empty,
            );
            let _enter = span.enter();
            let started = std::time::Instant::now();
            let r = slot.with_store(|s| Self::apply_one(s, &e));
            let secs = started.elapsed().as_secs_f64();
            span.record("duration_ms", secs * 1000.0);
            obs.observe_apply(secs);
            match &r {
                Ok(_) => tracing::debug!("applied"),
                Err(err) => tracing::error!(error = %err, "apply failed"),
            }
            let r = r.map_err(|err| {
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
        let span = tracing::Span::current();
        match &e.payload {
            EntryPayload::Blank => span.record("kind", "blank"),
            EntryPayload::Membership(_) => span.record("kind", "membership"),
            EntryPayload::Normal(_) => &span,
        };
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
        let span = tracing::Span::current();
        let (kind, files) = match &cmd {
            Cmd::IndexChunk(c) => ("index_chunk", c.files.len()),
            Cmd::IngestExtraction(_) => ("ingest_extraction", 1),
            Cmd::Prune(_) => ("prune", 0),
            Cmd::Vacuum(_) => ("vacuum", 0),
            Cmd::Noop(_) => ("noop", 0),
        };
        span.record("kind", kind);
        span.record("files", files);
        match cmd {
            Cmd::IndexChunk(c) => {
                let opts = IndexOptions {
                    reindex: c.reindex,
                    ..Default::default()
                };
                let prepared = c
                    .files
                    .iter()
                    .map(|f| {
                        graph_store::Store::prepare(
                            store,
                            &c.org,
                            &c.repo,
                            &f.as_batch_file()
                                .map_err(|e| StoreError::Protocol(e.to_string()))?,
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
        let fp = self.failpoints;
        let obs = Arc::clone(&self.obs);
        let gate = self.testing_apply_gate.clone();
        // Parsing and committing block; keep them off the runtime workers.
        tokio::task::spawn_blocking(move || {
            Self::apply_all(&slot, entries, fp, &obs, gate.as_ref())
        })
        .await
        .map_err(|e| sm_err(std::io::Error::other(format!("apply task: {e}"))))?
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            slot: Arc::clone(&self.slot),
            snapshots: Arc::clone(&self.snapshots),
        }
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<SnapshotFile>, StorageError> {
        Ok(Box::new(SnapshotFile {
            path: self.snapshots.incoming_path(),
        }))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<SnapshotFile>,
    ) -> Result<(), StorageError> {
        let slot = Arc::clone(&self.slot);
        let snaps = Arc::clone(&self.snapshots);
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || {
            snaps
                .install_with(slot.path(), &meta, &snapshot, |staged| {
                    slot.install_snapshot(staged)
                })
                .map_err(sm_err)
        })
        .await
        .map_err(|e| sm_err(std::io::Error::other(format!("install task: {e}"))))?
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError> {
        let snaps = Arc::clone(&self.snapshots);
        tokio::task::spawn_blocking(move || snaps.current_snapshot())
            .await
            .map_err(|e| sm_read_err(std::io::Error::other(format!("snapshot task: {e}"))))
    }
}

pub struct SnapshotBuilder {
    slot: Arc<StoreSlot>,
    snapshots: Arc<SnapshotDir>,
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError> {
        let slot = Arc::clone(&self.slot);
        let snaps = Arc::clone(&self.snapshots);
        tokio::task::spawn_blocking(move || snaps.build(&slot).map_err(sm_err))
            .await
            .map_err(|e| sm_err(std::io::Error::other(format!("snapshot task: {e}"))))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::log_store::RedbLogStore;
    use openraft::testing::{StoreBuilder, Suite};
    use std::path::Path;
    use std::time::Duration;

    /// One fresh store pair per test, in its own temp dir.
    struct Builder;

    fn sm_at(d: &Path) -> (Arc<StoreSlot>, StoreStateMachine) {
        let db = d.join("g.redb");
        let slot = StoreSlot::open(&db, vec![], None, Duration::from_secs(900)).unwrap();
        let snaps = Arc::new(SnapshotDir::open(&d.join("snapshots"), "h").unwrap());
        (Arc::clone(&slot), StoreStateMachine::new(slot, snaps))
    }

    impl StoreBuilder<TypeConfig, RedbLogStore, StoreStateMachine, tempfile::TempDir> for Builder {
        async fn build(
            &self,
        ) -> Result<(tempfile::TempDir, RedbLogStore, StoreStateMachine), StorageError> {
            let d = tempfile::tempdir().unwrap();
            let log = RedbLogStore::open(&d.path().join("raft.redb")).unwrap();
            let (_, sm) = sm_at(d.path());
            Ok((d, log, sm))
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

    const NO_FP: SmFailpoints = SmFailpoints {
        fail_before_apply: None,
    };

    #[test]
    fn a_refused_entry_moves_the_marker_to_its_log_id() {
        let d = tempfile::tempdir().unwrap();
        let (slot, _) = sm_at(d.path());
        let out = StoreStateMachine::apply_all(
            &slot,
            vec![blank(1), refused(2)],
            NO_FP,
            &Default::default(),
            None,
        )
        .unwrap();
        assert_eq!(out[0], LogResponse::Marked);
        assert!(matches!(out[1], LogResponse::Failed(_)), "{:?}", out[1]);
        let marker = slot.with_store(|s| s.raft_marker()).unwrap().unwrap();
        assert_eq!(log_id_of(marker), log_id(2));
        // A replay of the refused entry is a skip, not a second refusal.
        let again =
            StoreStateMachine::apply_all(&slot, vec![refused(2)], NO_FP, &Default::default(), None)
                .unwrap();
        assert_eq!(again, vec![LogResponse::Skipped]);
    }

    #[test]
    fn the_before_apply_failpoint_stops_before_the_entry() {
        let d = tempfile::tempdir().unwrap();
        let (slot, _) = sm_at(d.path());
        let fp = SmFailpoints {
            fail_before_apply: Some(2),
        };
        assert!(StoreStateMachine::apply_all(
            &slot,
            vec![blank(1), blank(2)],
            fp,
            &Default::default(),
            None
        )
        .is_err());
        let marker = slot.with_store(|s| s.raft_marker()).unwrap().unwrap();
        assert_eq!(log_id_of(marker), log_id(1), "entry 1 applied, 2 not");
    }

    /// A refusal is the last applied entry, then a snapshot is built, the
    /// log purged up to it, and the node restarted: the store's applied
    /// state, openraft's last applied and the snapshot's last log id all
    /// agree, and none is below the purge.
    #[tokio::test]
    async fn refusal_then_snapshot_purge_restart_keeps_applied_state_consistent() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("raft.redb");
        {
            let (slot, mut sm) = sm_at(d.path());
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
        let (_slot, mut sm) = sm_at(d.path());
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
    fn with_snapshot_at_1(d: &Path) -> (Arc<StoreSlot>, Arc<SnapshotDir>) {
        let (slot, _) = sm_at(d);
        StoreStateMachine::apply_all(&slot, vec![blank(1)], NO_FP, &Default::default(), None)
            .unwrap();
        let snaps = Arc::new(SnapshotDir::open(&d.join("snapshots"), "h").unwrap());
        let snap = snaps.build(&slot).unwrap();
        assert_eq!(snap.meta.last_log_id, Some(log_id(1)));
        (slot, snaps)
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_build_names_the_pair_by_term_and_index_and_keeps_one() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        assert_eq!(
            files_in(snaps.dir()),
            vec!["snap-1-1.meta".to_string(), "snap-1-1.redb".to_string()]
        );
        let (side, path) = snaps.current().unwrap();
        assert_eq!(
            (side.term, side.index, side.extractors_hash.as_str()),
            (1, 1, "h")
        );
        let (sha, size) = super::super::snapshot_dir::sha256_file(&path).unwrap();
        assert_eq!((side.sha256, side.size), (sha, size));
        StoreStateMachine::apply_all(&slot, vec![blank(2)], NO_FP, &Default::default(), None)
            .unwrap();
        snaps.build(&slot).unwrap();
        assert_eq!(
            files_in(snaps.dir()),
            vec!["snap-1-2.meta".to_string(), "snap-1-2.redb".to_string()],
            "the older pair is removed once the new one is complete"
        );
        // Building again at the same index reuses the pair.
        let before = snaps.built();
        snaps.build(&slot).unwrap();
        assert_eq!(snaps.built(), before);
    }

    #[test]
    fn a_failed_install_leaves_the_current_snapshot_and_meta_untouched() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let meta_before = std::fs::read(snaps.dir().join("snap-1-1.meta")).unwrap();
        let incoming = snaps.incoming_path();
        marked_file(&incoming, 5);
        let r = snaps.install_with(
            slot.path(),
            &meta_at(5),
            &SnapshotFile {
                path: incoming.clone(),
            },
            |_| Err(StoreError::Storage("injected install failure".into())),
        );
        assert!(r.is_err());
        assert_eq!(
            std::fs::read(snaps.dir().join("snap-1-1.meta")).unwrap(),
            meta_before
        );
        let (side, _) = snaps.current().unwrap();
        assert_eq!(side.last_log_id, Some(log_id(1)));
        assert!(!incoming.exists(), "the received file is cleaned up");
        assert_eq!(snaps.installed(), 0);
    }

    /// A received file whose own marker is not the snapshot's last log id
    /// is refused before any swap, and cleaned up.
    #[test]
    fn an_install_whose_file_marker_differs_from_its_meta_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let incoming = snaps.incoming_path();
        marked_file(&incoming, 4);
        let mut swapped = false;
        let r = snaps.install_with(
            slot.path(),
            &meta_at(5),
            &SnapshotFile {
                path: incoming.clone(),
            },
            |_| {
                swapped = true;
                Ok(())
            },
        );
        assert!(
            matches!(r, Err(StoreError::Rejected(ref m)) if m.contains("metadata")),
            "{r:?}"
        );
        assert!(!swapped, "nothing was swapped");
        assert!(!incoming.exists());
        assert_eq!(snaps.current().unwrap().0.index, 1);
        assert_eq!(snaps.installed(), 0);
    }

    /// Dev review 5: a crash between an install's store swap (store at 5)
    /// and the promotion of the received file (snapshot still at 1), with
    /// a log that never held entries 2..=5: the start-up repair builds a
    /// snapshot of the store; with a log that does hold them, or a
    /// snapshot that is current, it does nothing.
    #[test]
    fn a_store_ahead_of_snapshot_and_log_gets_a_fresh_snapshot_at_start() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        // The swapped-in store: marker 5, entries 2..=5 never in the log.
        StoreStateMachine::apply_all(&slot, vec![blank(5)], NO_FP, &Default::default(), None)
            .unwrap();
        let log = RedbLogStore::open(&d.path().join("raft.redb")).unwrap();
        assert!(crate::raft::node::repair_stale_snapshot(&slot, &snaps, &log).unwrap());
        assert_eq!(snaps.current().unwrap().0.index, 5);
        // Now current: nothing to do.
        assert!(!crate::raft::node::repair_stale_snapshot(&slot, &snaps, &log).unwrap());
        // A store ahead of its snapshot whose log reaches the marker (the
        // normal case after a restart) is left alone.
        StoreStateMachine::apply_all(&slot, vec![blank(6)], NO_FP, &Default::default(), None)
            .unwrap();
        log.insert_for_test(&blank(6));
        assert!(!crate::raft::node::repair_stale_snapshot(&slot, &snaps, &log).unwrap());
        assert_eq!(snaps.current().unwrap().0.index, 5);
    }

    /// #151: a current snapshot made by another build (an older store
    /// format, or another extractor version set hash) is rebuilt at start,
    /// even at the same index (a build never reuses it); a current one is
    /// left alone, and so is an outdated one the store is behind.
    #[test]
    fn an_outdated_snapshot_is_rebuilt_at_start() {
        use crate::raft::node::repair_outdated_snapshot;
        use crate::testing::age_snapshot;
        let old = *graph_store::UPGRADABLE_SCHEMA_VERSIONS.last().unwrap();
        for (format, hash) in [
            (Some(old), None),
            (None, Some("other")),
            (Some(old), Some("x")),
        ] {
            let d = tempfile::tempdir().unwrap();
            let (slot, snaps) = with_snapshot_at_1(d.path());
            assert!(!repair_outdated_snapshot(&slot, &snaps, ok).unwrap());
            let (_, path) = snaps.current().unwrap();
            age_snapshot(&path, format, hash).unwrap();
            let (side, _) = snaps.current().unwrap();
            assert!(snaps.is_outdated(&side), "{format:?} {hash:?}");
            let before = snaps.built();
            assert!(repair_outdated_snapshot(&slot, &snaps, ok).unwrap());
            assert_eq!(snaps.built(), before + 1);
            let (side, path) = snaps.current().unwrap();
            assert_eq!(side.index, 1, "rebuilt at the store's applied index");
            assert_eq!(side.store_format_version, graph_store::SCHEMA_VERSION);
            assert_eq!(side.extractors_hash, "h");
            assert_eq!(
                graph_store::detect_format(&path).unwrap(),
                Some(graph_store::SCHEMA_VERSION)
            );
            assert!(!repair_outdated_snapshot(&slot, &snaps, ok).unwrap());
        }
        // A store behind its outdated snapshot (not produced by any repair)
        // is left for the policy rather than rebuilt at a lower index.
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        StoreStateMachine::apply_all(&slot, vec![blank(3)], NO_FP, &Default::default(), None)
            .unwrap();
        snaps.build(&slot).unwrap();
        let (_, path) = snaps.current().unwrap();
        age_snapshot(&path, None, Some("other")).unwrap();
        std::fs::create_dir_all(d.path().join("behind")).unwrap();
        let fresh = sm_at(&d.path().join("behind")).0;
        StoreStateMachine::apply_all(&fresh, vec![blank(2)], NO_FP, &Default::default(), None)
            .unwrap();
        assert!(!repair_outdated_snapshot(&fresh, &snaps, ok).unwrap());
        assert_eq!(snaps.current().unwrap().0.index, 3);
    }

    fn ok() -> Result<(), StoreError> {
        Ok(())
    }

    /// #151 dev review: the start-up rebuild runs the disk guard first, and
    /// a refusal leaves the outdated pair as it was.
    #[test]
    fn an_outdated_snapshot_rebuild_checks_the_disk_first() {
        use crate::raft::node::repair_outdated_snapshot;
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let (_, path) = snaps.current().unwrap();
        crate::testing::age_snapshot(&path, None, Some("other")).unwrap();
        let before = files_in(snaps.dir());
        let r = repair_outdated_snapshot(&slot, &snaps, || {
            Err(StoreError::Storage("disk full".into()))
        });
        assert!(r.is_err());
        assert_eq!(files_in(snaps.dir()), before);
        assert!(snaps.is_outdated(&snaps.current().unwrap().0));
    }

    /// #151 dev review: a rebuild at the index of an outdated pair never
    /// overwrites its file; a crash after the new file's rename and before
    /// its meta leaves the old pair intact and current, and the next
    /// rebuild completes and prunes it.
    #[test]
    fn a_rebuild_at_the_same_index_survives_a_crash_before_its_meta() {
        use crate::raft::node::repair_outdated_snapshot;
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let (_, path) = snaps.current().unwrap();
        crate::testing::age_snapshot(&path, None, Some("other")).unwrap();
        let old_bytes = std::fs::read(&path).unwrap();
        let old_meta = std::fs::read(snaps.dir().join("snap-1-1.meta")).unwrap();
        snaps
            .fail_before_meta
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(repair_outdated_snapshot(&slot, &snaps, ok).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            old_bytes,
            "old file untouched"
        );
        assert_eq!(
            std::fs::read(snaps.dir().join("snap-1-1.meta")).unwrap(),
            old_meta
        );
        let (side, cur) = snaps.current().unwrap();
        assert_eq!((side.index, cur), (1, path.clone()));
        assert_eq!(side.extractors_hash, "other");
        // The orphaned data file (no meta) is ignored; a retry completes.
        snaps
            .fail_before_meta
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(repair_outdated_snapshot(&slot, &snaps, ok).unwrap());
        let (side, cur) = snaps.current().unwrap();
        assert_eq!((side.index, side.extractors_hash.as_str()), (1, "h"));
        assert_ne!(cur, path);
        let (sha, size) = super::super::snapshot_dir::sha256_file(&cur).unwrap();
        assert_eq!((side.sha256, side.size), (sha, size));
        let files = files_in(snaps.dir());
        assert_eq!(files.len(), 2, "one pair left: {files:?}");
    }

    /// #185: a promote (here an install's) while a build is still writing
    /// its temp file prunes older pairs but leaves the temp file alone; the
    /// build then finishes and promotes, and a crash's leftover temp file is
    /// swept when the directory is reopened.
    #[test]
    fn a_promote_never_prunes_a_build_in_progress() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let (side, current) = snaps.current().unwrap();
        // A build mid-export: its temp file exists, not yet promoted.
        let building = snaps.dir().join("snap-build.redb.tmp");
        slot.with_store(|s| s.export_snapshot(&building)).unwrap();
        let building_bytes = std::fs::read(&building).unwrap();
        // A concurrent install promotes (and prunes).
        let received = d.path().join("received.redb");
        std::fs::copy(&current, &received).unwrap();
        snaps
            .install_with(
                &d.path().join("store-not-swapped.redb"),
                &side.meta(),
                &SnapshotFile { path: received },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(snaps.installed(), 1);
        assert_eq!(
            std::fs::read(&building).unwrap(),
            building_bytes,
            "the build's temp file survives the prune"
        );
        // The build finishes and promotes its file.
        let (built, path, promoted) = snaps
            .promote(
                &building,
                side.last_log_id,
                side.membership.clone(),
                "finished".into(),
            )
            .unwrap();
        assert_eq!(built.snapshot_id, "finished");
        assert_eq!(promoted, super::super::snapshot_dir::Promoted::New);
        assert!(!building.exists());
        let (cur, cur_path) = snaps.current().unwrap();
        assert_eq!((cur.snapshot_id.as_str(), cur_path), ("finished", path));
        assert_eq!(files_in(snaps.dir()).len(), 2, "one pair left");
        // A temp file orphaned by a crash is swept at the next open.
        std::fs::write(&building, b"partial").unwrap();
        let reopened = SnapshotDir::open(snaps.dir(), "h").unwrap();
        assert!(!building.exists());
        assert_eq!(reopened.current().unwrap().0.snapshot_id, "finished");
    }

    /// #185 review: a build (index 2) racing an install of an older
    /// snapshot (index 1) on two threads always ends with the newest pair
    /// current, a meta that matches its file, and no other files.
    #[test]
    fn a_build_racing_an_older_install_keeps_the_newest_pair() {
        for _ in 0..8 {
            let d = tempfile::tempdir().unwrap();
            let (slot, snaps) = with_snapshot_at_1(d.path());
            let (side1, path1) = snaps.current().unwrap();
            let received = d.path().join("received.redb");
            std::fs::copy(&path1, &received).unwrap();
            StoreStateMachine::apply_all(&slot, vec![blank(2)], NO_FP, &Default::default(), None)
                .unwrap();
            let store_path = d.path().join("store-not-swapped.redb");
            std::thread::scope(|sc| {
                let build = sc.spawn(|| snaps.build(&slot).map(|s| s.meta.last_log_id));
                let install = sc.spawn(|| {
                    snaps.install_with(
                        &store_path,
                        &side1.meta(),
                        &SnapshotFile {
                            path: received.clone(),
                        },
                        |_| Ok(()),
                    )
                });
                let built = build.join().unwrap().unwrap();
                assert!(built >= Some(log_id(2)));
                install.join().unwrap().unwrap();
            });
            let (side, cur) = snaps.current().unwrap();
            assert_eq!(side.last_log_id, Some(log_id(2)));
            let (sha, size) = super::super::snapshot_dir::sha256_file(&cur).unwrap();
            assert_eq!((side.sha256, side.size), (sha, size));
            let files = files_in(snaps.dir());
            assert_eq!(files.len(), 2, "one pair, no orphans: {files:?}");
            assert!(files.iter().all(|f| !f.ends_with(".tmp")));
        }
    }

    /// #185 review: temp files of every kind left by a crash are swept when
    /// the directory is opened; the complete pair is kept.
    #[test]
    fn open_sweeps_every_kind_of_orphaned_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let (_slot, snaps) = with_snapshot_at_1(d.path());
        let dir = snaps.dir().to_path_buf();
        let before = files_in(&dir);
        for name in [
            "snap-1-1.meta.tmp",
            "incoming-123-0000abcd.redb.tmp",
            "snap-build-123-00000000.redb.tmp",
            "snap-copy-123-00000001.redb.tmp",
        ] {
            std::fs::write(dir.join(name), b"orphan").unwrap();
        }
        drop(snaps);
        let reopened = SnapshotDir::open(&dir, "h").unwrap();
        assert_eq!(files_in(&dir), before);
        assert_eq!(reopened.current().unwrap().0.index, 1);
    }

    /// #151: a received snapshot in an older, upgradable store format is
    /// installed; the store and the promoted pair are in this build's format.
    #[test]
    fn an_upgradable_older_format_snapshot_installs_and_upgrades() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let old = *graph_store::UPGRADABLE_SCHEMA_VERSIONS.last().unwrap();
        // Build a pair at 5 elsewhere and age it: the leader's file.
        let src = tempfile::tempdir().unwrap();
        let (lslot, lsnaps) = with_snapshot_at_1(src.path());
        StoreStateMachine::apply_all(&lslot, vec![blank(5)], NO_FP, &Default::default(), None)
            .unwrap();
        lsnaps.build(&lslot).unwrap();
        let (_, lpath) = lsnaps.current().unwrap();
        drop(lslot);
        crate::testing::age_snapshot(&lpath, Some(old), None).unwrap();
        assert_eq!(graph_store::detect_format(&lpath).unwrap(), Some(old));
        let incoming = snaps.incoming_path();
        std::fs::copy(&lpath, &incoming).unwrap();
        snaps
            .install_with(
                slot.path(),
                &meta_at(5),
                &SnapshotFile { path: incoming },
                |staged| slot.install_snapshot(staged),
            )
            .unwrap();
        assert_eq!(
            slot.with_store(|s| s.raft_marker()).unwrap().unwrap().index,
            5
        );
        let (side, path) = snaps.current().unwrap();
        assert_eq!(side.index, 5);
        assert_eq!(side.store_format_version, graph_store::SCHEMA_VERSION);
        assert_eq!(
            graph_store::detect_format(&path).unwrap(),
            Some(graph_store::SCHEMA_VERSION)
        );
    }

    #[test]
    fn a_successful_install_promotes_the_file_then_its_meta() {
        let d = tempfile::tempdir().unwrap();
        let (slot, snaps) = with_snapshot_at_1(d.path());
        let incoming = snaps.incoming_path();
        marked_file(&incoming, 5);
        let mut swapped = false;
        snaps
            .install_with(
                slot.path(),
                &meta_at(5),
                &SnapshotFile { path: incoming },
                |_| {
                    swapped = true;
                    Ok(())
                },
            )
            .unwrap();
        assert!(swapped);
        let (side, _) = snaps.current().unwrap();
        assert_eq!(side.last_log_id, Some(log_id(5)));
        assert_eq!(snaps.installed(), 1);
        assert_eq!(
            files_in(snaps.dir()),
            vec!["snap-1-5.meta".to_string(), "snap-1-5.redb".to_string()]
        );
    }

    /// The crash windows of a build or install: a data file without its
    /// meta, or a meta whose file is not the recorded size, is no snapshot;
    /// the previous complete pair still is.
    #[test]
    fn an_incomplete_pair_is_no_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let (_slot, snaps) = with_snapshot_at_1(d.path());
        // A newer data file with no meta (crash between the renames).
        marked_file(&snaps.dir().join("snap-1-9.redb"), 9);
        assert_eq!(snaps.current().unwrap().0.index, 1);
        // The current file truncated: no snapshot at all.
        std::fs::write(snaps.dir().join("snap-1-1.redb"), b"x").unwrap();
        assert!(snaps.current().is_none());
    }
}
