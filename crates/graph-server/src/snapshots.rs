//! Server-side snapshot handles (ADR 0004 D1): `Store.OpenSnapshot` freezes
//! a view a client then names by `View.snapshot_id`.
//!
//! Limits: at most [`SNAPSHOT_HANDLES_PER_CONNECTION`] per connection
//! (`INVALID_ARGUMENT`, close one first) and [`SNAPSHOT_HANDLES_GLOBAL`] on
//! the whole server (`RESOURCE_EXHAUSTED`). A handle expires at the store's
//! snapshot max age (the store itself refuses reads through an older handle
//! with `SnapshotExpired` too, and `get` checks the age on every call, so
//! expiry does not depend on the reaper); an idle reaper drops expired
//! handles every [`SNAPSHOT_REAP_INTERVAL`]; and every handle a connection
//! opened is dropped as soon as that connection closes
//! ([`SnapshotTable::drop_conn`], called when the connection's IO wrapper
//! is dropped), so a vanished client does not pin a snapshot until its TTL.
//!
//! Handle ids are random 64-bit values, not a counter, and a handle is
//! **not** bound to the connection that opened it for reads: any connection
//! may read through an id it knows (ids are unguessable, so knowing one is
//! the capability). The opening connection owns the handle's lifetime: its
//! cap counts it and its close drops it. A client that reconnects therefore
//! gets `SnapshotExpired` for handles of its old connection and opens new
//! ones, which is the documented reaction to that error anyway.
use crate::{SNAPSHOT_HANDLES_GLOBAL, SNAPSHOT_HANDLES_PER_CONNECTION, SNAPSHOT_REAP_INTERVAL};
use graph_proto::store_error_to_status;
use graph_store::{StoreError, V2Snapshot, V2Store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tonic::Status;

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
    global_cap: usize,
}

#[derive(Default)]
struct Table {
    open: HashMap<u64, Handle>,
    /// Handles held per connection (no linear scan on open).
    per_conn: HashMap<u64, usize>,
}

impl Table {
    fn remove(&mut self, id: u64) -> Option<Handle> {
        let h = self.open.remove(&id)?;
        if let Some(n) = self.per_conn.get_mut(&h.conn) {
            *n -= 1;
            if *n == 0 {
                self.per_conn.remove(&h.conn);
            }
        }
        Some(h)
    }

    fn held(&self, conn: u64) -> usize {
        self.per_conn.get(&conn).copied().unwrap_or(0)
    }
}

impl SnapshotTable {
    pub fn new(max_age: Duration) -> Self {
        Self::with_caps(
            max_age,
            SNAPSHOT_HANDLES_PER_CONNECTION,
            SNAPSHOT_HANDLES_GLOBAL,
        )
    }

    /// A table with explicit caps (tests use small ones).
    pub fn with_caps(max_age: Duration, per_conn_cap: usize, global_cap: usize) -> Self {
        Self {
            inner: Mutex::new(Table::default()),
            max_age,
            per_conn_cap,
            global_cap,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn check_caps(&self, t: &Table, conn: u64) -> Result<(), Status> {
        let held = t.held(conn);
        if held >= self.per_conn_cap {
            return Err(store_error_to_status(&StoreError::Rejected(format!(
                "this connection already holds {held} snapshot handles (the limit is {}); close one with CloseSnapshot",
                self.per_conn_cap
            ))));
        }
        if t.open.len() >= self.global_cap {
            return Err(Status::resource_exhausted(format!(
                "the server already holds {} snapshot handles (the server-wide limit is {}); retry later",
                t.open.len(),
                self.global_cap
            )));
        }
        Ok(())
    }

    /// Open a handle for connection `conn` over `store`'s current state.
    /// `INVALID_ARGUMENT` once the connection holds its cap,
    /// `RESOURCE_EXHAUSTED` once the server holds the global cap. The
    /// snapshot is taken outside the table lock (it opens a read
    /// transaction) and the caps are checked again after.
    pub fn open(&self, conn: u64, store: &V2Store) -> Result<u64, Status> {
        self.check_caps(&self.lock(), conn)?;
        let snap = store
            .snapshot_owned()
            .map_err(|e| store_error_to_status(&e))?;
        let mut t = self.lock();
        self.check_caps(&t, conn)?;
        let id = loop {
            let id = rand::random::<u64>();
            if id != 0 && !t.open.contains_key(&id) {
                break id;
            }
        };
        t.open.insert(
            id,
            Handle {
                conn,
                created: Instant::now(),
                snap: Arc::new(Mutex::new(snap)),
            },
        );
        *t.per_conn.entry(conn).or_insert(0) += 1;
        Ok(id)
    }

    /// The handle's view, or `SnapshotExpired` for one that expired, was
    /// closed, was dropped by a compact / snapshot install or its
    /// connection's close, or never existed (a client cannot tell those
    /// apart and reacts the same way: open a new one). The age is checked
    /// here on every call, whether or not the reaper has run.
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
            let h = t.remove(id).expect("present");
            return Err(StoreError::SnapshotExpired {
                age_secs: h.created.elapsed().as_secs(),
                max_age_secs: self.max_age.as_secs(),
            });
        }
        Ok(Arc::clone(&t.open[&id].snap))
    }

    /// Close one handle (a no-op for an unknown id).
    pub fn close(&self, id: u64) {
        self.lock().remove(id);
    }

    /// Drop every handle connection `conn` opened (its connection closed);
    /// returns how many were dropped.
    pub fn drop_conn(&self, conn: u64) -> usize {
        let mut t = self.lock();
        if t.per_conn.remove(&conn).is_none() {
            return 0;
        }
        let before = t.open.len();
        t.open.retain(|_, h| h.conn != conn);
        before - t.open.len()
    }

    /// Drop every expired handle; returns how many were dropped.
    pub fn reap(&self) -> usize {
        let mut t = self.lock();
        let max_age = self.max_age;
        let expired: Vec<u64> = t
            .open
            .iter()
            .filter(|(_, h)| h.created.elapsed() >= max_age)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            t.remove(*id);
        }
        expired.len()
    }

    /// Drop every handle (a compact or snapshot install replaces the file
    /// they read; their next use answers `SnapshotExpired`).
    pub fn clear(&self) {
        let mut t = self.lock();
        t.open.clear();
        t.per_conn.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    fn store() -> (tempfile::TempDir, V2Store) {
        let d = tempfile::tempdir().unwrap();
        let s = V2Store::open(d.path().join("g.redb")).unwrap();
        (d, s)
    }

    #[test]
    fn global_cap_is_resource_exhausted_and_per_conn_cap_is_invalid_argument() {
        let (_d, s) = store();
        let t = SnapshotTable::with_caps(Duration::from_secs(60), 2, 3);
        t.open(1, &s).unwrap();
        t.open(1, &s).unwrap();
        let e = t.open(1, &s).unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "{e:?}");
        t.open(2, &s).unwrap();
        let e = t.open(3, &s).unwrap_err();
        assert_eq!(e.code(), Code::ResourceExhausted, "{e:?}");
        assert!(e.message().contains("server-wide limit is 3"), "{e:?}");
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn ids_are_random_and_counts_follow_close_and_drop_conn() {
        let (_d, s) = store();
        let t = SnapshotTable::with_caps(Duration::from_secs(60), 2, 100);
        let a = t.open(1, &s).unwrap();
        let b = t.open(1, &s).unwrap();
        assert_ne!(a, b);
        assert!(a > 2 || b > 2, "not a small counter: {a} {b}");
        t.close(a);
        t.open(1, &s).unwrap(); // the per-connection count went down
        t.open(2, &s).unwrap();
        assert_eq!(t.drop_conn(1), 2);
        assert_eq!(t.len(), 1);
        assert_eq!(t.drop_conn(1), 0);
        t.open(1, &s).unwrap();
        t.open(1, &s).unwrap();
        assert!(t.get(b).is_err(), "dropped with its connection");
    }

    /// Mutation M6: expiry is enforced by `get` itself, with no reaper
    /// running at all.
    #[test]
    fn get_expires_a_handle_without_the_reaper() {
        let (_d, s) = store();
        let t = SnapshotTable::with_caps(Duration::from_millis(50), 4, 4);
        let id = t.open(1, &s).unwrap();
        assert!(t.get(id).is_ok());
        std::thread::sleep(Duration::from_millis(80));
        assert!(matches!(t.get(id), Err(StoreError::SnapshotExpired { .. })));
        assert_eq!(t.len(), 0, "the expired handle was removed");
        // Its connection's count went down with it.
        for _ in 0..4 {
            t.open(1, &s).unwrap();
        }
    }
}
