//! [`FullDisk`]: test-only redb storage backends over the real files whose
//! writes and growth fail with a full-disk I/O error while a flag is on
//! (story 67). Unlike the `DiskStall` failpoints, the error reaches redb
//! itself, which then poisons the open database (`PreviousIo` for every
//! later operation until it is reopened): the path a real ENOSPC takes.
use crate::powercut::BackendFactory;
use redb::StorageBackend;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The flags of one node: the store (`graph.redb`) and the Raft log
/// (`raft.redb`) fail separately.
#[derive(Clone, Default)]
pub struct FullDisk {
    store: Arc<AtomicBool>,
    log: Arc<AtomicBool>,
}

impl FullDisk {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes to the store file fail (on) or succeed (off).
    pub fn set_store_full(&self, on: bool) {
        self.store.store(on, Ordering::SeqCst);
    }

    /// Writes to the Raft log file fail (on) or succeed (off).
    pub fn set_log_full(&self, on: bool) {
        self.log.store(on, Ordering::SeqCst);
    }

    /// The factory for `ServeConfig::storage_backend`: each path opens its
    /// real file (created if missing, locked like redb's own backend).
    pub fn factory(&self) -> BackendFactory {
        let me = self.clone();
        Arc::new(move |path: &Path| {
            let is_log = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("raft.redb"));
            let full = if is_log {
                Arc::clone(&me.log)
            } else {
                Arc::clone(&me.store)
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
            let inner = redb::backends::FileBackend::new(file)
                .unwrap_or_else(|e| panic!("lock {}: {e}", path.display()));
            Box::new(FullDiskFile { inner, full }) as Box<dyn StorageBackend>
        })
    }
}

#[derive(Debug)]
struct FullDiskFile {
    inner: redb::backends::FileBackend,
    full: Arc<AtomicBool>,
}

impl FullDiskFile {
    fn check(&self) -> Result<(), io::Error> {
        if self.full.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "No space left on device (os error 28)",
            ));
        }
        Ok(())
    }
}

impl StorageBackend for FullDiskFile {
    fn len(&self) -> Result<u64, io::Error> {
        self.inner.len()
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        self.inner.read(offset, len)
    }
    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        self.check()?;
        self.inner.set_len(len)
    }
    fn sync_data(&self, eventual: bool) -> Result<(), io::Error> {
        self.inner.sync_data(eventual)
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        self.check()?;
        self.inner.write(offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, TableDefinition};

    const T: TableDefinition<u64, u64> = TableDefinition::new("t");

    /// The premise of story 67's backstop: a failed write poisons the open
    /// database (`PreviousIo`) even after space is back, and a reopen
    /// clears it, keeping what committed before.
    #[test]
    fn a_full_disk_poisons_redb_until_it_is_reopened() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("g.redb");
        let disk = FullDisk::new();
        let f = disk.factory();
        let open = || {
            Database::builder()
                .create_with_backend(crate::powercut::DynBackend(f(&path)))
                .unwrap()
        };
        let db = open();
        let put = |db: &Database, k: u64| -> Result<(), Box<redb::Error>> {
            let wt = db.begin_write().map_err(|e| Box::new(e.into()))?;
            wt.open_table(T)
                .map_err(|e| Box::new(e.into()))?
                .insert(k, k)
                .map_err(|e| Box::new(e.into()))?;
            wt.commit().map_err(|e| Box::new(e.into()))?;
            Ok(())
        };
        put(&db, 1).unwrap();
        disk.set_store_full(true);
        let e = put(&db, 2).unwrap_err();
        assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e}");
        disk.set_store_full(false);
        let e = put(&db, 3).unwrap_err();
        assert!(graph_proto::error::is_previous_io(&e.to_string()), "{e}");
        drop(db);
        let db = open();
        put(&db, 4).unwrap();
        let rt = db.begin_read().unwrap();
        let t = rt.open_table(T).unwrap();
        assert_eq!(t.get(1).unwrap().unwrap().value(), 1);
        assert!(t.get(2).unwrap().is_none());
        assert_eq!(t.get(4).unwrap().unwrap().value(), 4);
    }
}
