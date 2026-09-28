//! [`PowerCutDisk`]: an in-memory redb [`StorageBackend`] that can lose
//! power (the durability tests of ADR 0004 D7).
//!
//! Every file is two images: `current` (what reads see, every write lands
//! here) and `durable` (what survives a power cut: `current` as of the last
//! `sync_data`). [`PowerCutDisk::power_cut`] throws away everything written
//! since the last sync of every file under a directory, and cuts the old
//! handles off (a node still running on them afterwards writes nowhere), so
//! the next open sees exactly what a machine that lost power would find.
//! The disk also records the order of writes and syncs per file
//! ([`DiskEvent`]) and can die after the n-th sync of a file
//! ([`PowerCutDisk::fail_after_syncs`]: every later write, resize and sync
//! fails), a crash at a deterministic point inside a commit sequence.
//!
//! Model: a sync is a full barrier and a power cut keeps nothing unsynced.
//! (A real disk may also keep some unsynced writes; redb's commit protocol
//! checksums its header and pages for that, which its own crash tests
//! cover. What this disk checks is the server's side: that nothing is
//! acknowledged before its bytes were synced.)
//!
//! Test-only: `serve` never builds one.
use redb::StorageBackend;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Opens the storage of the store or log file at a path
/// ([`crate::ServeConfig::storage_backend`]).
pub type BackendFactory = Arc<dyn Fn(&Path) -> Box<dyn StorageBackend> + Send + Sync>;

/// One step of a file's I/O, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskEvent {
    Write { offset: u64, len: u64 },
    SetLen(u64),
    Sync,
}

#[derive(Debug, Default)]
struct FileState {
    durable: Vec<u8>,
    current: Vec<u8>,
    events: Vec<DiskEvent>,
    syncs: u64,
    /// The disk dies once `syncs` reaches this.
    fail_at_sync: Option<u64>,
    /// Writes, resizes and syncs fail (the disk died, or a power cut
    /// detached this state from the file).
    dead: bool,
}

type Shared = Arc<Mutex<FileState>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, FileState> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A set of in-memory files that can lose power together.
#[derive(Clone, Default)]
pub struct PowerCutDisk {
    files: Arc<Mutex<BTreeMap<PathBuf, Shared>>>,
}

impl std::fmt::Debug for PowerCutDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PowerCutDisk")
    }
}

impl PowerCutDisk {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self, path: &Path) -> Shared {
        let mut g = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(g.entry(path.to_path_buf()).or_default())
    }

    /// The backend of the file at `path` (created empty on first use).
    pub fn open(&self, path: &Path) -> PowerCutFile {
        PowerCutFile {
            state: self.state(path),
        }
    }

    /// A factory for [`crate::ServeConfig::storage_backend`].
    pub fn factory(&self) -> BackendFactory {
        let disk = self.clone();
        Arc::new(move |p: &Path| Box::new(disk.open(p)) as Box<dyn StorageBackend>)
    }

    /// Lose power under `dir`: every file there goes back to what was last
    /// synced, and whoever still holds the old handles writes nowhere.
    pub fn power_cut(&self, dir: &Path) {
        let mut g = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, st) in g.iter_mut() {
            if !path.starts_with(dir) {
                continue;
            }
            let fresh = {
                let mut old = lock(st);
                old.dead = true;
                FileState {
                    durable: old.durable.clone(),
                    current: old.durable.clone(),
                    ..FileState::default()
                }
            };
            *st = Arc::new(Mutex::new(fresh));
        }
    }

    /// The disk under `path` dies right after its next `n`-th sync
    /// completes (`n == 0`: before any further write).
    pub fn fail_after_syncs(&self, path: &Path, n: u64) {
        let st = self.state(path);
        let mut g = lock(&st);
        g.fail_at_sync = Some(g.syncs + n);
        if n == 0 {
            g.dead = true;
        }
    }

    /// Whether the disk under `path` died.
    pub fn is_dead(&self, path: &Path) -> bool {
        lock(&self.state(path)).dead
    }

    /// Syncs of `path` since it was created (or last power cut).
    pub fn syncs(&self, path: &Path) -> u64 {
        lock(&self.state(path)).syncs
    }

    /// The I/O of `path` since it was created (or last power cut).
    pub fn events(&self, path: &Path) -> Vec<DiskEvent> {
        lock(&self.state(path)).events.clone()
    }

    /// Bytes written to `path` but not synced yet.
    pub fn unsynced(&self, path: &Path) -> bool {
        let st = self.state(path);
        let g = lock(&st);
        g.current != g.durable
    }
}

/// One file of a [`PowerCutDisk`].
#[derive(Debug)]
pub struct PowerCutFile {
    state: Shared,
}

fn dead() -> io::Error {
    io::Error::other("power-cut disk: the disk is gone (testing)")
}

impl StorageBackend for PowerCutFile {
    fn len(&self) -> Result<u64, io::Error> {
        Ok(lock(&self.state).current.len() as u64)
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        let g = lock(&self.state);
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start
            .checked_add(len)
            .filter(|e| *e <= g.current.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end"))?;
        Ok(g.current[start..end].to_vec())
    }

    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        g.current
            .resize(usize::try_from(len).map_err(io::Error::other)?, 0);
        g.events.push(DiskEvent::SetLen(len));
        Ok(())
    }

    fn sync_data(&self, _eventual: bool) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        g.durable = g.current.clone();
        g.syncs += 1;
        g.events.push(DiskEvent::Sync);
        if g.fail_at_sync.is_some_and(|n| g.syncs >= n) {
            g.dead = true;
        }
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start + data.len();
        if g.current.len() < end {
            g.current.resize(end, 0);
        }
        g.current[start..end].copy_from_slice(data);
        g.events.push(DiskEvent::Write {
            offset,
            len: data.len() as u64,
        });
        Ok(())
    }
}

/// A boxed backend as a concrete one (redb's builder takes `impl
/// StorageBackend`).
#[derive(Debug)]
pub struct DynBackend(pub Box<dyn StorageBackend>);

impl StorageBackend for DynBackend {
    fn len(&self) -> Result<u64, io::Error> {
        self.0.len()
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        self.0.read(offset, len)
    }
    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        self.0.set_len(len)
    }
    fn sync_data(&self, eventual: bool) -> Result<(), io::Error> {
        self.0.sync_data(eventual)
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        self.0.write(offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, TableDefinition};

    const T: TableDefinition<u64, u64> = TableDefinition::new("t");

    fn open(disk: &PowerCutDisk, p: &Path) -> Database {
        Database::builder()
            .create_with_backend(disk.open(p))
            .unwrap()
    }

    fn get(db: &Database, k: u64) -> Option<u64> {
        let rt = db.begin_read().unwrap();
        match rt.open_table(T) {
            Ok(t) => t.get(k).unwrap().map(|v| v.value()),
            Err(_) => None,
        }
    }

    /// A committed (synced) transaction survives a power cut; the old
    /// handle is cut off; a reopen sees the synced state.
    #[test]
    fn a_power_cut_keeps_what_was_synced() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/raft.redb");
        let db = open(&disk, p);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        wt.commit().unwrap();
        assert!(disk.syncs(p) > 0);
        assert!(!disk.unsynced(p));
        disk.power_cut(Path::new("/n"));
        // The old handle writes nowhere now.
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(2, 20).unwrap();
        assert!(wt.commit().is_err());
        drop(db);
        let db = open(&disk, p);
        assert_eq!(get(&db, 1), Some(10));
        assert_eq!(get(&db, 2), None);
    }

    /// A transaction committed with no durability is not synced and is
    /// lost by a power cut, and the file still opens.
    #[test]
    fn a_power_cut_drops_unsynced_commits() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/g.redb");
        let db = open(&disk, p);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        wt.commit().unwrap();
        let mut wt = db.begin_write().unwrap();
        wt.set_durability(redb::Durability::None);
        wt.open_table(T).unwrap().insert(2, 20).unwrap();
        wt.commit().unwrap();
        assert_eq!(get(&db, 2), Some(20));
        assert!(disk.unsynced(p));
        disk.power_cut(Path::new("/n"));
        drop(db);
        let db = open(&disk, p);
        assert_eq!(get(&db, 1), Some(10));
        assert_eq!(get(&db, 2), None);
    }

    #[test]
    fn a_dead_disk_fails_writes_after_the_armed_sync() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/x.redb");
        let db = open(&disk, p);
        disk.fail_after_syncs(p, 1);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        // redb syncs once or more per commit; the disk dies at the first.
        let _ = wt.commit();
        assert!(disk.is_dead(p));
        let wt = db.begin_write();
        let failed = match wt {
            Ok(wt) => {
                wt.open_table(T).unwrap().insert(2, 20).unwrap();
                wt.commit().is_err()
            }
            Err(_) => true,
        };
        assert!(failed, "writes after the disk died fail");
        assert!(disk.events(p).iter().any(|e| matches!(e, DiskEvent::Sync)));
    }
}
