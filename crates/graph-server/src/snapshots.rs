//! Server-side snapshot handles (ADR 0004 D1): `Store.OpenSnapshot` freezes
//! a view a client then names by `View.snapshot_id`. At most
//! [`SNAPSHOT_HANDLES_PER_CONNECTION`] per connection; a handle expires at
//! the store's snapshot max age (the store itself refuses reads through an
//! older handle with `SnapshotExpired` too); an idle reaper drops expired
//! handles every [`SNAPSHOT_REAP_INTERVAL`].
use crate::{SNAPSHOT_HANDLES_PER_CONNECTION, SNAPSHOT_REAP_INTERVAL};
use graph_store::{StoreError, V2Snapshot, V2Store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A handle's frozen view, shared between the table and an in-flight read.
/// `V2Snapshot` is `Send` but not `Sync` (it caches a warning flag), hence
/// the mutex; reads through one handle are serialized, which is what one
/// client paging through it does anyway.
pub type SharedSnapshot = Arc<Mutex<V2Snapshot>>;

struct Handle {
    conn: u64,
    created: Instant,
    snap: SharedSnapshot,
}

/// The table of open handles.
pub struct SnapshotTable {
    inner: Mutex<Table>,
    max_age: Duration,
    per_conn_cap: usize,
}

#[derive(Default)]
struct Table {
    next_id: u64,
    open: HashMap<u64, Handle>,
}

impl SnapshotTable {
    pub fn new(max_age: Duration) -> Self {
        Self {
            inner: Mutex::new(Table {
                next_id: 1,
                open: HashMap::new(),
            }),
            max_age,
            per_conn_cap: SNAPSHOT_HANDLES_PER_CONNECTION,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Open a handle for connection `conn` over `store`'s current state.
    /// `Rejected` once the connection holds the cap.
    pub fn open(&self, conn: u64, store: &V2Store) -> Result<u64, StoreError> {
        let mut t = self.lock();
        let held = t.open.values().filter(|h| h.conn == conn).count();
        if held >= self.per_conn_cap {
            return Err(StoreError::Rejected(format!(
                "this connection already holds {held} snapshot handles (the limit is {}); close one with CloseSnapshot",
                self.per_conn_cap
            )));
        }
        let snap = store.snapshot_owned()?;
        let id = t.next_id;
        t.next_id += 1;
        t.open.insert(
            id,
            Handle {
                conn,
                created: Instant::now(),
                snap: Arc::new(Mutex::new(snap)),
            },
        );
        Ok(id)
    }

    /// The handle's view, or `SnapshotExpired` for one that expired, was
    /// closed, was dropped by a compact / snapshot install, or never
    /// existed (a client cannot tell those apart and reacts the same way:
    /// open a new one).
    pub fn get(&self, id: u64) -> Result<SharedSnapshot, StoreError> {
        let mut t = self.lock();
        let expired = match t.open.get(&id) {
            None => {
                return Err(StoreError::SnapshotExpired {
                    age_secs: 0,
                    max_age_secs: self.max_age.as_secs(),
                })
            }
            Some(h) => h.created.elapsed() >= self.max_age,
        };
        if expired {
            let h = t.open.remove(&id).expect("present");
            return Err(StoreError::SnapshotExpired {
                age_secs: h.created.elapsed().as_secs(),
                max_age_secs: self.max_age.as_secs(),
            });
        }
        Ok(Arc::clone(&t.open[&id].snap))
    }

    /// Close one handle (a no-op for an unknown id).
    pub fn close(&self, id: u64) {
        self.lock().open.remove(&id);
    }

    /// Drop every expired handle; returns how many were dropped.
    pub fn reap(&self) -> usize {
        let mut t = self.lock();
        let before = t.open.len();
        let max_age = self.max_age;
        t.open.retain(|_, h| h.created.elapsed() < max_age);
        before - t.open.len()
    }

    /// Drop every handle (a compact or snapshot install replaces the file
    /// they read; their next use answers `SnapshotExpired`).
    pub fn clear(&self) {
        self.lock().open.clear();
    }

    /// Open handles right now.
    pub fn len(&self) -> usize {
        self.lock().open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// The reaper: runs until the table is dropped.
    pub async fn reaper(table: std::sync::Weak<crate::StoreSlot>) {
        let mut tick = tokio::time::interval(SNAPSHOT_REAP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(slot) = table.upgrade() else { return };
            let n = slot.snapshots().reap();
            if n > 0 {
                tracing::debug!(dropped = n, "reaped expired snapshot handles");
            }
        }
    }
}
