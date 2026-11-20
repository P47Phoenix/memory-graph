//! The server's disk guard (ADR 0004 D7): before accepting a write and
//! before building a snapshot, check the free space on the data volume
//! against `--min-free-disk` plus room for one snapshot copy (a snapshot is
//! a full copy of the store). Refusal is `StoreError::Storage("disk full
//! ...")`, which the wire maps to `RESOURCE_EXHAUSTED`.
//!
//! The probe is a copy of graph-cli's `diskinfo::sample_dir` (the free
//! bytes only): moving it into a shared crate would make graph-server
//! depend on graph-cli's module layout or add a crate for twenty lines.
use graph_store::StoreError;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Free bytes for this user on the volume holding a directory, if the
/// platform says; tests inject a fake one.
pub type FreeSpaceProbe = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// The real probe (`GetDiskFreeSpaceExW` / `statvfs`).
pub fn system_probe() -> FreeSpaceProbe {
    Arc::new(free_bytes)
}

#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the three out
    // pointers are valid u64s; the return value is checked.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

#[cfg(unix)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(dir).ok()?;
    let frsize = if s.f_frsize > 0 {
        s.f_frsize
    } else {
        s.f_bsize
    };
    Some(s.f_bavail.saturating_mul(frsize))
}

#[cfg(not(any(windows, unix)))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// The guard of one node.
#[derive(Clone)]
pub struct DiskGuard {
    /// Keep at least this much free (0: the guard is off).
    pub min_free: u64,
    probe: FreeSpaceProbe,
    /// The directory whose volume is checked.
    dir: PathBuf,
    store: PathBuf,
    log: PathBuf,
}

impl DiskGuard {
    pub fn new(min_free: u64, probe: FreeSpaceProbe, dir: &Path, store: &Path, log: &Path) -> Self {
        Self {
            min_free,
            probe,
            dir: dir.to_path_buf(),
            store: store.to_path_buf(),
            log: log.to_path_buf(),
        }
    }

    fn size(p: &Path) -> u64 {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    /// `Ok` when the volume has `min_free` plus one snapshot copy (the
    /// store's size) free; an unreadable volume passes (never refuse on a
    /// platform that cannot say).
    pub fn check(&self, what: &str) -> Result<(), StoreError> {
        let store = Self::size(&self.store);
        self.check_need(
            what,
            store,
            &format!("one snapshot copy of the {store}-byte store"),
        )
    }

    /// `Ok` when the volume has `min_free` plus `extra` bytes free (a
    /// snapshot install: the received file and the staged copy of it).
    pub fn check_extra(&self, what: &str, extra: u64) -> Result<(), StoreError> {
        self.check_need(what, extra, &format!("{extra} bytes for the {what}"))
    }

    /// The check before a snapshot build: like [`check`](Self::check), but
    /// with the guard off (`min_free` 0, the `--db` default) it still
    /// requires room for the copy itself. A build is a full copy of the
    /// store; one started without that room fills the disk, fails with
    /// ENOSPC and leaves nothing behind but a log that cannot be purged
    /// (issue #330), so it is postponed instead.
    pub fn check_snapshot(&self) -> Result<(), StoreError> {
        let store = Self::size(&self.store);
        self.check_need_always(
            "snapshot",
            store,
            &format!("one snapshot copy of the {store}-byte store"),
        )
    }

    fn check_need(&self, what: &str, extra: u64, why: &str) -> Result<(), StoreError> {
        if self.min_free == 0 {
            return Ok(());
        }
        self.check_need_always(what, extra, why)
    }

    fn check_need_always(&self, what: &str, extra: u64, why: &str) -> Result<(), StoreError> {
        let Some(free) = (self.probe)(&self.dir) else {
            return Ok(());
        };
        let need = self.min_free.saturating_add(extra);
        if free < need {
            return Err(StoreError::Storage(format!(
                "disk full: {what} refused: {free} bytes free on the volume of `{}`, \
                 need {need} (--min-free-disk {} + {why}; the Raft log is {} bytes)",
                self.dir.display(),
                self.min_free,
                Self::size(&self.log),
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_below_min_free_plus_a_snapshot_copy() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("g.redb");
        std::fs::write(&store, vec![0u8; 100]).unwrap();
        let log = d.path().join("raft.redb");
        let g = DiskGuard::new(1000, Arc::new(|_| Some(1099)), d.path(), &store, &log);
        let e = g.check("write").unwrap_err();
        assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e}");
        let g = DiskGuard::new(1000, Arc::new(|_| Some(1100)), d.path(), &store, &log);
        assert!(g.check("write").is_ok());
        let g = DiskGuard::new(0, Arc::new(|_| Some(0)), d.path(), &store, &log);
        assert!(g.check("write").is_ok(), "off at 0");
        let g = DiskGuard::new(1000, Arc::new(|_| None), d.path(), &store, &log);
        assert!(g.check("write").is_ok(), "unknown passes");
    }

    /// Issue #330: with the guard off (the `--db` default) a snapshot
    /// build still needs room for its copy; writes are not checked.
    #[test]
    fn a_snapshot_build_needs_room_for_its_copy_even_with_the_guard_off() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("g.redb");
        std::fs::write(&store, vec![0u8; 100]).unwrap();
        let log = d.path().join("raft.redb");
        let g = DiskGuard::new(0, Arc::new(|_| Some(99)), d.path(), &store, &log);
        assert!(g.check("write").is_ok(), "writes: off at 0");
        let e = g.check_snapshot().unwrap_err();
        assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e}");
        let g = DiskGuard::new(0, Arc::new(|_| Some(100)), d.path(), &store, &log);
        assert!(g.check_snapshot().is_ok());
        let g = DiskGuard::new(10, Arc::new(|_| Some(109)), d.path(), &store, &log);
        assert!(g.check_snapshot().is_err(), "min_free adds to the copy");
        let g = DiskGuard::new(0, Arc::new(|_| None), d.path(), &store, &log);
        assert!(g.check_snapshot().is_ok(), "unknown passes");
    }

    #[test]
    fn the_system_probe_reads_something_here() {
        let d = tempfile::tempdir().unwrap();
        assert!(free_bytes(d.path()).is_some());
    }
}
