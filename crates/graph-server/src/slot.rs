//! [`StoreSlot`]: the one `V2Store` a server owns, behind a read/write lock
//! (ADR 0004 D7). Reads, writes and Raft apply take the read lock (the
//! store serializes writers itself); `compact` and a snapshot install take
//! the write lock, close the store, swap the file and reopen it with the
//! same extractors. The slot also owns the snapshot handle table, because a
//! swap must drop every handle first (they read the old file).
//!
//! Client reads during a snapshot install (D8): the install marks the slot
//! as installing before it waits for the write lock, and
//! [`StoreSlot::with_store_read`] answers `Locked` (`UNAVAILABLE` on the
//! wire) at once while it is, so a client moves to its next endpoint rather
//! than wait for the swap. Raft apply and writes use
//! [`StoreSlot::with_store`], which waits (an install is short, and failing
//! an apply would stop the node).
use crate::extractors::register_all;
use crate::powercut::{BackendFactory, DynBackend};
use crate::snapshots::SnapshotTable;
use graph_core::Extractor;
use graph_store::{CompactStats, MarkedCommitHook, StoreError, V2Store};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Test-only: called by a snapshot install once it holds the write lock
/// and every snapshot handle is gone, just before the swap; the test blocks
/// inside it to observe the install in progress.
pub type InstallGate = Arc<dyn Fn() + Send + Sync>;

/// How long a swap waits for requests still reading a dropped snapshot
/// handle before it refuses (`Locked`) and leaves the store as it was.
pub const HANDLE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct StoreSlot {
    store: RwLock<Option<V2Store>>,
    path: PathBuf,
    extractors: Vec<Arc<dyn Extractor>>,
    cache_bytes: Option<usize>,
    max_snapshot_age: Duration,
    snapshots: SnapshotTable,
    /// Test-only failpoint kept across reopenings (compact, install).
    marked_commit_hook: Mutex<Option<MarkedCommitHook>>,
    /// Test-only: the store lives on these backends, not in a file.
    backend: Option<BackendFactory>,
    /// A snapshot install is under way (reads fail fast).
    installing: AtomicBool,
    install_gate: Mutex<Option<InstallGate>>,
    handle_drain_timeout: Mutex<Duration>,
}

/// Clears the installing flag however the install ends.
struct Installing<'a>(&'a AtomicBool);

impl Drop for Installing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// #74: at startup, warn for every language whose stored symbols came from
/// an extractor this server was built without (clients' `index --server`
/// is parsed here, so this is where the gap is known). Never fatal.
fn warn_extractor_gaps(store: &V2Store) {
    match graph_store::Store::extractor_gaps(store, None, None) {
        Ok(gaps) => {
            for g in gaps {
                tracing::warn!(
                    org = %g.org,
                    repo = %g.repo,
                    language = %g.language,
                    stored_extractor = %g.stored_version,
                    "{g}"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "checking stored extractors failed"),
    }
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
        Self::open_with(path, extractors, cache_bytes, max_snapshot_age, None)
    }

    /// [`open`](Self::open), over test-only storage backends when
    /// `backend` is `Some` ([`crate::powercut`]).
    pub fn open_with(
        path: &Path,
        extractors: Vec<Arc<dyn Extractor>>,
        cache_bytes: Option<usize>,
        max_snapshot_age: Duration,
        backend: Option<BackendFactory>,
    ) -> Result<Arc<Self>, StoreError> {
        let store = Self::open_store(
            path,
            &extractors,
            cache_bytes,
            max_snapshot_age,
            None,
            backend.as_ref(),
        )?;
        warn_extractor_gaps(&store);
        Ok(Arc::new(Self {
            store: RwLock::new(Some(store)),
            path: path.to_path_buf(),
            extractors,
            cache_bytes,
            max_snapshot_age,
            snapshots: SnapshotTable::new(max_snapshot_age),
            marked_commit_hook: Mutex::new(None),
            backend,
            installing: AtomicBool::new(false),
            install_gate: Mutex::new(None),
            handle_drain_timeout: Mutex::new(HANDLE_DRAIN_TIMEOUT),
        }))
    }

    fn open_store(
        path: &Path,
        extractors: &[Arc<dyn Extractor>],
        cache_bytes: Option<usize>,
        max_snapshot_age: Duration,
        hook: Option<MarkedCommitHook>,
        backend: Option<&BackendFactory>,
    ) -> Result<V2Store, StoreError> {
        let mut store = match backend {
            Some(f) => V2Store::open_with_backend(path, DynBackend(f(path)), cache_bytes)?,
            None => V2Store::open_with_cache_bytes(path, cache_bytes)?,
        };
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
            self.backend.as_ref(),
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

    /// Install (or clear) the test-only [`InstallGate`].
    pub fn set_install_gate(&self, gate: Option<InstallGate>) {
        *self
            .install_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = gate;
    }

    /// How long a swap waits for readers of dropped snapshot handles
    /// (tests shorten it).
    pub fn set_handle_drain_timeout(&self, t: Duration) {
        *self
            .handle_drain_timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = t;
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

    /// Whether a snapshot install is under way.
    pub fn is_installing(&self) -> bool {
        self.installing.load(Ordering::SeqCst)
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

    /// [`with_store`](Self::with_store) for a client read: `Locked` at
    /// once while a snapshot install is under way (ADR 0004 D8).
    pub fn with_store_read<T>(
        &self,
        f: impl FnOnce(&V2Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if self.is_installing() {
            return Err(self.installing_error());
        }
        self.with_store(|s| {
            // Raced an install that began after the check above and is now
            // waiting for this read lock: still answer, the store is intact.
            f(s)
        })
    }

    fn installing_error(&self) -> StoreError {
        StoreError::Locked(format!(
            "store `{}` is being replaced by a snapshot from the leader; \
             try another node or retry in a moment",
            self.path.display()
        ))
    }

    /// Wait (bounded) until nobody reads a dropped snapshot view any more:
    /// such a view keeps the old file open, and Windows refuses to rename
    /// over an open file (Linux would reopen a file still locked).
    fn drain(
        &self,
        dropped: Vec<std::sync::Weak<Mutex<graph_store::V2Snapshot>>>,
    ) -> Result<(), StoreError> {
        let timeout = *self
            .handle_drain_timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + timeout;
        loop {
            let busy = dropped.iter().filter(|w| w.strong_count() > 0).count();
            if busy == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(StoreError::Locked(format!(
                    "store `{}`: {busy} request(s) still read a snapshot handle after {timeout:?}; \
                     not replacing the file now, retry later",
                    self.path.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Take the store out under the write lock, run `f` on it (which may
    /// close and replace the file), and put the store `f` returns back.
    /// Every snapshot handle is dropped first, under the write lock (so
    /// none can be opened in between), and the swap waits for requests
    /// still reading one ([`HANDLE_DRAIN_TIMEOUT`], then `Locked` with the
    /// store left as it was). If `f` fails, the slot reopens the file
    /// (`compact` and `install_snapshot` leave it intact on failure) so the
    /// server keeps serving, or stays closed if even that fails, which
    /// every later call reports.
    pub fn replace<T>(
        &self,
        f: impl FnOnce(V2Store) -> Result<(V2Store, T), StoreError>,
    ) -> Result<T, StoreError> {
        let mut g = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dropped = self.snapshots.clear();
        self.drain(dropped)?;
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

    fn file_based(&self, what: &str) -> Result<(), StoreError> {
        if self.backend.is_some() {
            return Err(StoreError::Rejected(format!(
                "{what} needs a store file (this test store lives on a storage backend)"
            )));
        }
        Ok(())
    }

    /// `V2Store::compact` under the write lock (ADR 0004 D5: node-local,
    /// never replicated).
    pub fn compact(&self) -> Result<CompactStats, StoreError> {
        self.file_based("compact")?;
        self.replace(|store| store.compact())
    }

    /// Close the store, replace its file by the snapshot at `src`
    /// (`V2Store::install_snapshot`) and reopen it, under the write lock.
    /// Client reads answer `Locked` from the start of the call to its end.
    pub fn install_snapshot(&self, src: &Path) -> Result<(), StoreError> {
        self.file_based("a snapshot install")?;
        self.installing.store(true, Ordering::SeqCst);
        let _installing = Installing(&self.installing);
        let gate = self
            .install_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.replace(|store| {
            if let Some(gate) = &gate {
                gate();
            }
            drop(store);
            V2Store::install_snapshot(&self.path, src)?;
            let store = self.reopen()?;
            Ok((store, ()))
        })
    }

    /// Close the store for good (shutdown): later calls answer `Locked`.
    pub fn close(&self) {
        let mut g = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.snapshots.clear();
        *g = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request still reading a dropped snapshot view keeps the old file
    /// open: the swap waits for it and, past the drain timeout, refuses
    /// with the store left open and serving (it used to drop the handles
    /// outside the write lock and fail the rename on Windows, leaving the
    /// slot closed). Once the reader lets go, the swap goes through.
    #[test]
    fn a_swap_waits_for_readers_of_dropped_snapshot_handles() {
        let d = tempfile::tempdir().unwrap();
        let slot = StoreSlot::open(
            &d.path().join("g.redb"),
            vec![],
            None,
            Duration::from_secs(900),
        )
        .unwrap();
        slot.with_store(|s| {
            graph_store::Store::index_bytes(s, "o", "r", "a.rs", b"fn a() {}", None)
        })
        .unwrap();
        let id = slot
            .with_store(|s| Ok(slot.snapshots().open(7, s).unwrap()))
            .unwrap();
        // A request got the view and is mid-read.
        let held = slot.snapshots().get(id).unwrap();
        slot.set_handle_drain_timeout(Duration::from_millis(50));
        let e = slot.compact().unwrap_err();
        assert!(
            matches!(e, StoreError::Locked(ref m) if m.contains("snapshot handle")),
            "{e}"
        );
        // The store is still open and serving, the handle gone.
        assert_eq!(
            slot.with_store_read(|s| graph_store::StoreRead::count_nodes(
                s,
                graph_core::NodeKind::File
            ))
            .unwrap(),
            1
        );
        assert!(slot.snapshots().get(id).is_err());
        drop(held);
        slot.compact().unwrap();
        assert_eq!(
            slot.with_store_read(|s| graph_store::StoreRead::count_nodes(
                s,
                graph_core::NodeKind::File
            ))
            .unwrap(),
            1
        );
    }

    /// During an install client reads fail fast with `Locked`; apply-side
    /// access waits; afterwards reads work again.
    #[test]
    fn reads_fail_fast_while_a_snapshot_installs() {
        let d = tempfile::tempdir().unwrap();
        let slot = StoreSlot::open(
            &d.path().join("g.redb"),
            vec![],
            None,
            Duration::from_secs(900),
        )
        .unwrap();
        let src = d.path().join("snap.redb");
        slot.with_store(|s| s.export_snapshot(&src)).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        slot.set_install_gate(Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        })));
        let s2 = Arc::clone(&slot);
        let installer = std::thread::spawn(move || s2.install_snapshot(&src));
        entered_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the install reached its gate");
        let e = slot
            .with_store_read(|s| graph_store::StoreRead::count_nodes(s, graph_core::NodeKind::File))
            .unwrap_err();
        assert!(
            matches!(e, StoreError::Locked(ref m) if m.contains("snapshot")),
            "{e}"
        );
        release_tx.send(()).unwrap();
        installer.join().unwrap().unwrap();
        assert!(!slot.is_installing());
        slot.with_store_read(|s| {
            graph_store::StoreRead::count_nodes(s, graph_core::NodeKind::File)
        })
        .unwrap();
    }
}
