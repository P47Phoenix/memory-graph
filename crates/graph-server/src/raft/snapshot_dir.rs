//! [`SnapshotDir`]: the snapshot files of one node (ADR 0004 D6/D7).
//!
//! Layout: `<snapshots_dir>/snap-<term>-<index>.redb` (an exported store
//! whose own `RAFT_SM` marker is the snapshot's last log id) plus
//! `snap-<term>-<index>.meta` ([`SnapshotSidecar`], JSON: term, index,
//! membership, sha256, size, extractors hash, store format). One pair is
//! kept: older pairs are removed only after a new one is complete.
//!
//! Crash safety: a build exports to its own `snap-build-<nanos>-<n>.redb.tmp`
//! (unique per build, so concurrent builds never touch each other's file and
//! every error path removes it), renames it to its final name, then writes
//! the meta through a temp file and a rename.
//! The current snapshot is the highest-index meta whose data file exists
//! with the recorded size; a crash between the renames leaves a data file
//! without a meta (ignored, and the previous pair is still there because it
//! is only removed afterwards). Pruning older pairs never touches a
//! `*.tmp` file, which may be a concurrent build's export in progress
//! (#185); temp files orphaned by a crash are swept when the directory is
//! opened.
//!
//! Concurrency: builds (blocking tasks) and installs can run at the same
//! time. Each promotion (rename, fsync, meta, prune) holds one lock, so a
//! prune never removes another promotion's renamed file before its meta
//! exists. A promotion older than the current pair (a build that exported
//! before an install of a later snapshot finished) is discarded and the
//! newer pair kept: openraft's snapshot only ever moves forward, and a
//! newer committed snapshot is always a valid answer.
//!
//! A build at an index that already has a complete pair reuses it, unless that pair was made by another build
//! (older store format or other extractors, #151): the rebuilt pair is then
//! named `snap-<term>-<index>-r<nanos>` rather than overwriting the existing
//! file, so a meta never describes a different file of the same name and a
//! file being streamed is never replaced. The current pair is chosen by the
//! meta's contents, not its name.
use super::types::{LogId, SnapshotFile, SnapshotMeta, StoredMembership, TypeConfig};
use crate::slot::StoreSlot;
use graph_store::{StoreError, V2Store};
use openraft::Snapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The `.meta` next to a snapshot file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSidecar {
    pub term: u64,
    pub index: u64,
    pub last_log_id: Option<LogId>,
    pub membership: StoredMembership,
    pub snapshot_id: String,
    /// Lowercase hex SHA-256 of the data file.
    pub sha256: String,
    pub size: u64,
    pub extractors_hash: String,
    pub store_format_version: u64,
}

impl SnapshotSidecar {
    pub fn meta(&self) -> SnapshotMeta {
        SnapshotMeta {
            last_log_id: self.last_log_id,
            last_membership: self.membership.clone(),
            snapshot_id: self.snapshot_id.clone(),
        }
    }
}

/// `snap-<term>-<index>` for a last log id (`snap-0-0` for an empty one).
pub fn stem(last: Option<&LogId>) -> String {
    match last {
        Some(id) => format!("snap-{}-{}", id.leader_id.term, id.index),
        None => "snap-0-0".into(),
    }
}

/// The `.meta` path of a snapshot data file.
pub fn meta_path_of(data: &Path) -> PathBuf {
    data.with_extension("meta")
}

/// Read the sidecar of a data file.
pub fn read_sidecar(data: &Path) -> Result<SnapshotSidecar, StoreError> {
    let p = meta_path_of(data);
    let text = std::fs::read_to_string(&p)
        .map_err(|e| StoreError::Storage(format!("reading `{}`: {e}", p.display())))?;
    serde_json::from_str(&text).map_err(|e| StoreError::Corrupt(format!("`{}`: {e}", p.display())))
}

/// SHA-256 (lowercase hex) and length of a file, read in 1 MiB blocks.
pub fn sha256_file(path: &Path) -> Result<(String, u64), StoreError> {
    let io = |e: std::io::Error| StoreError::Storage(format!("reading `{}`: {e}", path.display()));
    let mut f = std::fs::File::open(path).map_err(io)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf).map_err(io)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex(&h.finalize()), n))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Rename `from` over `to` (removing `to` first where the platform's
/// rename refuses to replace an existing file).
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => {
            std::fs::remove_file(to)?;
            std::fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

pub struct SnapshotDir {
    dir: PathBuf,
    extractors_hash: String,
    /// Snapshots installed from a leader since start (tests, status).
    installed: AtomicU64,
    /// Snapshots built since start.
    built: AtomicU64,
    /// Told about every new build (the backup uploader); must not block.
    on_built: std::sync::RwLock<Option<OnBuilt>>,
    /// Test failpoint: fail a promotion after the data file's rename and
    /// before its meta is written (a crash there).
    #[cfg(test)]
    pub(crate) fail_before_meta: std::sync::atomic::AtomicBool,
    /// Held across a whole promotion (rename, meta, prune).
    promote_lock: std::sync::Mutex<()>,
    /// Makes temp names unique within this process.
    temp_seq: AtomicU64,
}

/// A temp file removed when dropped (every error path); a no-op once the
/// file has been renamed into place.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What a promotion did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Promoted {
    /// The file became the current pair.
    New,
    /// A newer pair already existed; the file was discarded.
    Stale,
    /// A build found a complete pair of this build at the same log id
    /// already in place (a concurrent build won the race, #217); the file
    /// was discarded and that pair returned.
    Reused,
}

/// Called with each newly built snapshot pair (not a reused one).
pub type OnBuilt = std::sync::Arc<dyn Fn(&SnapshotSidecar, &Path) + Send + Sync>;

impl SnapshotDir {
    /// Create the directory if needed and drop leftover temp files.
    pub fn open(dir: &Path, extractors_hash: &str) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| StoreError::Storage(format!("creating `{}`: {e}", dir.display())))?;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            extractors_hash: extractors_hash.to_string(),
            installed: AtomicU64::new(0),
            built: AtomicU64::new(0),
            on_built: std::sync::RwLock::new(None),
            #[cfg(test)]
            fail_before_meta: Default::default(),
            promote_lock: std::sync::Mutex::new(()),
            temp_seq: AtomicU64::new(0),
        })
    }

    /// Set (or clear) the hook told about each new build.
    pub fn set_on_built(&self, f: Option<OnBuilt>) {
        *self
            .on_built
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = f;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn extractors_hash(&self) -> &str {
        &self.extractors_hash
    }

    pub fn installed(&self) -> u64 {
        self.installed.load(Ordering::SeqCst)
    }

    pub fn built(&self) -> u64 {
        self.built.load(Ordering::SeqCst)
    }

    /// A fresh temp path for a snapshot being received.
    pub fn incoming_path(&self) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let n: u32 = rand::random();
        self.dir.join(format!("incoming-{nanos}-{n:08x}.redb.tmp"))
    }

    /// A fresh `<prefix>-<nanos>-<n>.redb.tmp` path in this directory.
    fn temp_path(&self, prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let n = self.temp_seq.fetch_add(1, Ordering::SeqCst);
        self.dir.join(format!("{prefix}-{nanos}-{n:08x}.redb.tmp"))
    }

    fn data_path(&self, stem: &str) -> PathBuf {
        self.dir.join(format!("{stem}.redb"))
    }

    /// The complete snapshot pairs, highest index first.
    fn pairs(&self) -> Vec<(SnapshotSidecar, PathBuf)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("snap-") && name.ends_with(".meta")) {
                continue;
            }
            let data = e.path().with_extension("redb");
            let Ok(side) = read_sidecar(&data) else {
                tracing::warn!(meta = %e.path().display(), "unreadable snapshot meta; ignored");
                continue;
            };
            match std::fs::metadata(&data) {
                Ok(m) if m.len() == side.size => out.push((side, data)),
                _ => tracing::warn!(
                    file = %data.display(),
                    "snapshot file missing or not the size its meta records; ignored"
                ),
            }
        }
        // At one index (a rebuild that crashed before pruning), a pair made
        // by this build comes first.
        out.sort_by_key(|a| (std::cmp::Reverse(a.0.index), self.is_outdated(&a.0)));
        out
    }

    /// The current snapshot: the highest-index complete pair.
    pub fn current(&self) -> Option<(SnapshotSidecar, PathBuf)> {
        self.pairs().into_iter().next()
    }

    /// Whether a pair was made by another build: an older store format (a
    /// snapshot from before an upgrade) or another extractor version set.
    /// Such a pair is never reused by a build, and is replaced at start-up
    /// (`node::repair_outdated_snapshot`) so no follower is sent it (#151).
    pub fn is_outdated(&self, side: &SnapshotSidecar) -> bool {
        side.store_format_version != graph_store::SCHEMA_VERSION
            || side.extractors_hash != self.extractors_hash
    }

    pub fn current_snapshot(&self) -> Option<Snapshot<TypeConfig>> {
        self.current().map(|(side, path)| Snapshot {
            meta: side.meta(),
            snapshot: Box::new(SnapshotFile { path }),
        })
    }

    /// Remove every snapshot file but `keep` (and its meta). Errors are
    /// ignored: a file still being streamed to a follower on Windows may
    /// refuse, and a leftover is harmless (the next build retries).
    ///
    /// Temp files (`*.tmp`) are never removed here: one may belong to a
    /// concurrent build or install still writing it (#185). Each owner
    /// removes its own temp file, and [`SnapshotDir::open`] sweeps any
    /// left by a crash.
    fn prune_except(&self, keep: &Path) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let keep_meta = meta_path_of(keep);
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let prunable = name.starts_with("snap-") && !name.ends_with(".tmp");
            if prunable && p != keep && p != keep_meta {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Put `data` (a complete store file in this directory) in place as
    /// the snapshot `last`/`membership`: rename to its final name, then
    /// write its meta, then remove older pairs, all under `promote_lock`.
    ///
    /// If the current pair is newer than `last`, `data` is removed and the
    /// current pair returned with `Promoted::Stale`: a newer pair is never
    /// pruned for an older one.
    pub(super) fn promote(
        &self,
        data: &Path,
        last: Option<LogId>,
        membership: StoredMembership,
        snapshot_id: String,
    ) -> Result<(SnapshotSidecar, PathBuf, Promoted), StoreError> {
        self.promote_with(data, last, membership, snapshot_id, false)
    }

    /// [`Self::promote`] for a build: if a complete pair of this build
    /// (not [outdated](Self::is_outdated)) already exists at `last`, `data`
    /// is removed and that pair returned with `Promoted::Reused`. The check
    /// runs under `promote_lock`, so two builds at one index never both
    /// promote, and the second's prune never removes the file the first
    /// already handed to openraft (#217). An outdated pair at `last` is
    /// still replaced (#151).
    pub(super) fn promote_built(
        &self,
        data: &Path,
        last: Option<LogId>,
        membership: StoredMembership,
        snapshot_id: String,
    ) -> Result<(SnapshotSidecar, PathBuf, Promoted), StoreError> {
        self.promote_with(data, last, membership, snapshot_id, true)
    }

    fn promote_with(
        &self,
        data: &Path,
        last: Option<LogId>,
        membership: StoredMembership,
        snapshot_id: String,
        reuse_same_index: bool,
    ) -> Result<(SnapshotSidecar, PathBuf, Promoted), StoreError> {
        let _guard = self
            .promote_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((side, path)) = self.current() {
            if reuse_same_index
                && last.is_some()
                && side.last_log_id == last
                && !self.is_outdated(&side)
            {
                let _ = std::fs::remove_file(data);
                return Ok((side, path, Promoted::Reused));
            }
            if side.last_log_id > last {
                tracing::info!(
                    current = ?side.last_log_id,
                    incoming = ?last,
                    "snapshot older than the current one; discarded"
                );
                let _ = std::fs::remove_file(data);
                return Ok((side, path, Promoted::Stale));
            }
        }
        let io = |e: std::io::Error| StoreError::Storage(format!("snapshot: {e}"));
        let (sha256, size) = sha256_file(data)?;
        let st = stem(last.as_ref());
        let mut final_path = self.data_path(&st);
        if data != final_path && final_path.exists() {
            // A pair at this index already exists (a rebuild of a snapshot
            // made by another build, #151): never overwrite its file, which
            // its meta describes and openraft may be streaming. The new pair
            // gets its own name; the old one is pruned once it is complete.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            final_path = self.data_path(&format!("{st}-r{nanos}"));
        }
        if data != final_path {
            replace_file(data, &final_path).map_err(io)?;
        }
        #[cfg(test)]
        if self.fail_before_meta.load(Ordering::SeqCst) {
            return Err(StoreError::Storage("failpoint: before the meta".into()));
        }
        // What the file is, not what this binary writes: a received file in
        // an older format is upgraded when `check_marker` opens it, and if it
        // were not, the sidecar says so and the start-up repair replaces it.
        let store_format_version =
            graph_store::detect_format(&final_path)?.unwrap_or(graph_store::SCHEMA_VERSION);
        let side = SnapshotSidecar {
            term: last.map_or(0, |l| l.leader_id.term),
            index: last.map_or(0, |l| l.index),
            last_log_id: last,
            membership,
            snapshot_id,
            sha256,
            size,
            extractors_hash: self.extractors_hash.clone(),
            store_format_version,
        };
        let meta = meta_path_of(&final_path);
        // Durable: the data file is synced before its meta exists, and the
        // meta through a synced temp file and a rename, then the directory
        // (a meta must never describe bytes that a power cut lost).
        std::fs::OpenOptions::new()
            .write(true)
            .open(&final_path)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        crate::paths::durable_write(
            &meta,
            serde_json::to_string_pretty(&side)
                .expect("sidecar serializes")
                .as_bytes(),
        )
        .map_err(io)?;
        self.prune_except(&final_path);
        Ok((side, final_path, Promoted::New))
    }

    /// Build a snapshot of the store in `slot` (blocking): export in one
    /// read transaction, read the copy's own marker, and promote. A build at
    /// an index that already has a complete pair reuses it.
    pub fn build(&self, slot: &StoreSlot) -> Result<Snapshot<TypeConfig>, StoreError> {
        let tmp = TempFile(self.temp_path("snap-build"));
        slot.with_store(|s| s.export_snapshot(&tmp.0))?;
        let (last, membership) = {
            let copy = V2Store::open(&tmp.0)?;
            super::state_machine::StoreStateMachine::read_applied(&copy)?
        };
        // Reusing a pair already at this index is decided inside the
        // promote, under its lock (#217).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let snapshot_id = format!("{}-{nanos}", stem(last.as_ref()));
        let (side, path, promoted) = self.promote_built(&tmp.0, last, membership, snapshot_id)?;
        drop(tmp);
        if promoted == Promoted::New {
            self.built.fetch_add(1, Ordering::SeqCst);
            let hook = self
                .on_built
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(f) = hook {
                f(&side, &path);
            }
        }
        Ok(Snapshot {
            meta: side.meta(),
            snapshot: Box::new(SnapshotFile { path }),
        })
    }

    /// A snapshot file's own `RAFT_SM` marker must be the snapshot's last
    /// log id: the store's applied state after the install is read from
    /// that marker, so a file that says otherwise would leave the store and
    /// openraft disagreeing about what was applied.
    fn check_marker(path: &Path, meta: &SnapshotMeta) -> Result<(), StoreError> {
        let (last, _) = {
            let copy = V2Store::open(path)?;
            super::state_machine::StoreStateMachine::read_applied(&copy)?
        };
        if last != meta.last_log_id {
            return Err(StoreError::Rejected(format!(
                "snapshot file `{}` is at {last:?} but its metadata says {:?}",
                path.display(),
                meta.last_log_id
            )));
        }
        Ok(())
    }

    /// Install a received snapshot file (blocking): validate it, swap the
    /// store to a copy of it (`swap`, the slot's install under its write
    /// lock), and only then make it the current snapshot. A failed install
    /// leaves the current snapshot untouched and removes the received file.
    pub fn install_with(
        &self,
        store_path: &Path,
        meta: &SnapshotMeta,
        data: &SnapshotFile,
        swap: impl FnOnce(&Path) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let received = data.path.starts_with(&self.dir)
            && data
                .path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("incoming-"));
        let cleanup = || {
            if received {
                let _ = std::fs::remove_file(&data.path);
            }
        };
        let valid = match graph_store::detect_format(&data.path) {
            Ok(Some(_)) => Self::check_marker(&data.path, meta),
            Ok(None) => Err(StoreError::Rejected(format!(
                "`{}` is not a store file",
                data.path.display()
            ))),
            Err(e) => Err(e),
        };
        if let Err(e) = valid {
            cleanup();
            return Err(e);
        }
        // The swap consumes its source: install a copy (beside the store,
        // so the swap is a same-volume rename). Two copies are inherent,
        // not an oversight: the store and the retained snapshot file are
        // two files that diverge as soon as the store applies the next
        // entry, so the one received file cannot become both by renames
        // (a hard link would share the bytes and let the store's writes
        // corrupt the snapshot). An install therefore needs room for the
        // received file plus the staged copy (the Raft service checks
        // that against the disk guard before receiving).
        let mut staged = store_path.as_os_str().to_owned();
        staged.push(".install.tmp");
        let staged = PathBuf::from(staged);
        let _ = std::fs::remove_file(&staged);
        let r = std::fs::copy(&data.path, &staged)
            .map_err(|e| StoreError::Storage(format!("staging the snapshot: {e}")))
            .and_then(|_| swap(&staged));
        let _ = std::fs::remove_file(&staged);
        if let Err(e) = r {
            cleanup();
            return Err(e);
        }
        let promoted = if received {
            self.promote(
                &data.path,
                meta.last_log_id,
                meta.last_membership.clone(),
                meta.snapshot_id.clone(),
            )
        } else {
            // Not ours to move (a test's file): copy it in.
            let tmp = TempFile(self.temp_path("snap-copy"));
            std::fs::copy(&data.path, &tmp.0)
                .map_err(|e| StoreError::Storage(format!("snapshot copy: {e}")))
                .and_then(|_| {
                    self.promote(
                        &tmp.0,
                        meta.last_log_id,
                        meta.last_membership.clone(),
                        meta.snapshot_id.clone(),
                    )
                })
        };
        cleanup();
        let (side, _, promoted) = promoted?;
        if promoted == Promoted::Stale {
            // Unreachable under openraft: it drops a full snapshot at or
            // below `committed`, and a build covers at most `applied`, so no
            // pair can be newer than an install. Only a direct caller (the
            // race test) gets here; no `debug_assert!` for that reason.
            tracing::warn!(
                store_at = ?meta.last_log_id,
                pair_at = ?side.last_log_id,
                "installed a snapshot older than the current pair; pair kept"
            );
            return Ok(());
        }
        self.installed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
