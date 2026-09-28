//! [`StoreSlot`]: the one `V2Store` a server owns, behind a read/write lock
//! (ADR 0004 D7). Reads, writes and Raft apply take the read lock (the
//! store serializes writers itself); `compact` and a snapshot install take
//! the write lock, close the store, swap the file and reopen it with the
//! same extractors. The slot also owns the snapshot handle table, because a
//! swap must drop every handle first (they read the old file).
use crate::extractors::register_all;
use crate::snapshots::SnapshotTable;
use graph_core::Extractor;
use graph_store::{CompactStats, MarkedCommitHook, StoreError, V2Store};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub struct StoreSlot {
    store: RwLock<Option<V2Store>>,
    path: PathBuf,
    extractors: Vec<Arc<dyn Extractor>>,
    cache_bytes: Option<usize>,
    max_snapshot_age: Duration,
    snapshots: SnapshotTable,
    /// Test-only failpoint kept across reopenings (compact, install).
    marked_commit_hook: Mutex<Option<MarkedCommitHook>>,
}

impl StoreSlot {
    /// Open the store at `path` (redb's exclusive lock: `Locked` while
    /// another process holds it) with `extractors` registered.
    pub fn open(
        path: &Path,
        extractors: Vec<Arc<dyn Extractor>>,
        cache_bytes: Option<usize>,
        max_snapshot_age: Duration,
    ) -> Result<Arc<Self>, StoreError> {
        let store = Self::open_store(path, &extractors, cache_bytes, max_snapshot_age, None)?;
        Ok(Arc::new(Self {
            store: RwLock::new(Some(store)),
            path: path.to_path_buf(),
            extractors,
            cache_bytes,
            max_snapshot_age,
            snapshots: SnapshotTable::new(max_snapshot_age),
            marked_commit_hook: Mutex::new(None),
        }))
    }

    fn open_store(
        path: &Path,
        extractors: &[Arc<dyn Extractor>],
        cache_bytes: Option<usize>,
        max_snapshot_age: Duration,
        hook: Option<MarkedCommitHook>,
    ) -> Result<V2Store, StoreError> {
        let mut store = V2Store::open_with_cache_bytes(path, cache_bytes)?;
        store.set_max_snapshot_age(max_snapshot_age);
        store.set_marked_commit_hook(hook);
        register_all(&mut store, extractors);
        Ok(store)
    }

    fn reopen(&self) -> Result<V2Store, StoreError> {
        Self::open_store(
            &self.path,
            &self.extractors,
            self.cache_bytes,
            self.max_snapshot_age,
            self.hook(),
        )
    }

    fn hook(&self) -> Option<MarkedCommitHook> {
        self.marked_commit_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install (or clear) the store's test-only [`MarkedCommitHook`], now
    /// and on every reopening.
    pub fn set_marked_commit_hook(&self, hook: Option<MarkedCommitHook>) {
        *self
            .marked_commit_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.clone();
        let mut g = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = g.as_mut() {
            s.set_marked_commit_hook(hook);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn extractors(&self) -> &[Arc<dyn Extractor>] {
        &self.extractors
    }

    pub fn snapshots(&self) -> &SnapshotTable {
        &self.snapshots
    }

    pub fn max_snapshot_age(&self) -> Duration {
        self.max_snapshot_age
    }

    /// Run `f` on the store under the read lock. `Locked` (UNAVAILABLE on
    /// the wire, ADR 0004 D8) in the moment between a swap closing the old
    /// file and reopening the new one, which the write lock makes
    /// unobservable except when the reopen itself failed.
    pub fn with_store<T>(
        &self,
        f: impl FnOnce(&V2Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let g = self
            .store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match g.as_ref() {
            Some(s) => f(s),
            None => Err(StoreError::Locked(format!(
                "store `{}` is closed (a snapshot install or compact failed to reopen it)",
                self.path.display()
            ))),
        }
    }

    /// Take the store out under the write lock, run `f` on it (which may
    /// close and replace the file), and put the store `f` returns back.
    /// Every snapshot handle is dropped first. If `f` fails, the slot
    /// reopens the file (`compact` and `install_snapshot` leave it intact
    /// on failure) so the server keeps serving, or stays closed if even
    /// that fails, which every later call reports.
    pub fn replace<T>(
        &self,
        f: impl FnOnce(V2Store) -> Result<(V2Store, T), StoreError>,
    ) -> Result<T, StoreError> {
        self.snapshots.clear();
        let mut g = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = g.take().ok_or_else(|| {
            StoreError::Locked(format!("store `{}` is closed", self.path.display()))
        })?;
        match f(store) {
            Ok((store, out)) => {
                *g = Some(store);
                Ok(out)
            }
            Err(e) => {
                match self.reopen() {
                    Ok(store) => *g = Some(store),
                    Err(re) => tracing::error!(error = %re, "store could not be reopened"),
                }
                Err(e)
            }
        }
    }

    /// `V2Store::compact` under the write lock (ADR 0004 D5: node-local,
    /// never replicated).
    pub fn compact(&self) -> Result<CompactStats, StoreError> {
        self.replace(|store| store.compact())
    }

    /// Close the store, replace its file by the snapshot at `src`
    /// (`V2Store::install_snapshot`) and reopen it, under the write lock.
    pub fn install_snapshot(&self, src: &Path) -> Result<(), StoreError> {
        self.replace(|store| {
            drop(store);
            V2Store::install_snapshot(&self.path, src)?;
            let store = self.reopen()?;
            Ok((store, ()))
        })
    }

    /// Close the store for good (shutdown): later calls answer `Locked`.
    pub fn close(&self) {
        self.snapshots.clear();
        let mut g = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = None;
    }
}
