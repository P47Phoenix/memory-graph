//! [`SnapshotDir`]: the snapshot files of one node (ADR 0004 D6/D7).
//!
//! Layout: `<snapshots_dir>/snap-<term>-<index>.redb` (an exported store
//! whose own `RAFT_SM` marker is the snapshot's last log id) plus
//! `snap-<term>-<index>.meta` ([`SnapshotSidecar`], JSON: term, index,
//! membership, sha256, size, extractors hash, store format). One pair is
//! kept: older pairs are removed only after a new one is complete.
//!
//! Crash safety: a build exports to `snap-build.redb.tmp`, renames it to
//! its final name, then writes the meta through a temp file and a rename.
//! The current snapshot is the highest-index meta whose data file exists
//! with the recorded size; a crash between the renames leaves a data file
//! without a meta (ignored, and the previous pair is still there because it
//! is only removed afterwards). Names are unique per (term, index) and a
//! build at an index that already has a complete pair reuses it, so a meta
//! never describes a different file of the same name.
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
        out.sort_by_key(|a| std::cmp::Reverse(a.0.index));
        out
    }

    /// The current snapshot: the highest-index complete pair.
    pub fn current(&self) -> Option<(SnapshotSidecar, PathBuf)> {
        self.pairs().into_iter().next()
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
    fn prune_except(&self, keep: &Path) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let keep_meta = meta_path_of(keep);
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("snap-") && p != keep && p != keep_meta {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Put `data` (a complete store file in this directory) in place as
    /// the snapshot `last`/`membership`: rename to its final name, then
    /// write its meta, then remove older pairs.
    fn promote(
        &self,
        data: &Path,
        last: Option<LogId>,
        membership: StoredMembership,
        snapshot_id: String,
    ) -> Result<(SnapshotSidecar, PathBuf), StoreError> {
        let io = |e: std::io::Error| StoreError::Storage(format!("snapshot: {e}"));
        let (sha256, size) = sha256_file(data)?;
        let st = stem(last.as_ref());
        let final_path = self.data_path(&st);
        if data != final_path {
            replace_file(data, &final_path).map_err(io)?;
        }
        let side = SnapshotSidecar {
            term: last.map_or(0, |l| l.leader_id.term),
            index: last.map_or(0, |l| l.index),
            last_log_id: last,
            membership,
            snapshot_id,
            sha256,
            size,
            extractors_hash: self.extractors_hash.clone(),
            store_format_version: graph_store::SCHEMA_VERSION,
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
        Ok((side, final_path))
    }

    /// Build a snapshot of the store in `slot` (blocking): export in one
    /// read transaction, read the copy's own marker, and promote. A build at
    /// an index that already has a complete pair reuses it.
    pub fn build(&self, slot: &StoreSlot) -> Result<Snapshot<TypeConfig>, StoreError> {
        let tmp = self.dir.join("snap-build.redb.tmp");
        let _ = std::fs::remove_file(&tmp);
        slot.with_store(|s| s.export_snapshot(&tmp))?;
        let (last, membership) = {
            let copy = V2Store::open(&tmp)?;
            super::state_machine::StoreStateMachine::read_applied(&copy)?
        };
        if let Some((side, path)) = self.current() {
            if side.last_log_id == last && side.last_log_id.is_some() {
                let _ = std::fs::remove_file(&tmp);
                return Ok(Snapshot {
                    meta: side.meta(),
                    snapshot: Box::new(SnapshotFile { path }),
                });
            }
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let snapshot_id = format!("{}-{nanos}", stem(last.as_ref()));
        let r = self.promote(&tmp, last, membership, snapshot_id);
        let _ = std::fs::remove_file(&tmp);
        let (side, path) = r?;
        self.built.fetch_add(1, Ordering::SeqCst);
        let hook = self
            .on_built
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(f) = hook {
            f(&side, &path);
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
            let tmp = self.dir.join("snap-copy.redb.tmp");
            std::fs::copy(&data.path, &tmp)
                .map_err(|e| StoreError::Storage(format!("snapshot copy: {e}")))
                .and_then(|_| {
                    self.promote(
                        &tmp,
                        meta.last_log_id,
                        meta.last_membership.clone(),
                        meta.snapshot_id.clone(),
                    )
                })
        };
        cleanup();
        promoted?;
        self.installed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
