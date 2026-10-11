//! [`DiskStall`]: a full disk while appending to the Raft log or applying
//! an entry to the store does not stop the node (ADR 0004 D7, note of
//! 2026-10-11, story 67 / #115).
//!
//! openraft 0.9 treats every error of `append`, `apply` and the log's
//! other writes as fatal (the node stops), and `apply` must answer every
//! entry it is given, so an entry cannot be skipped and answered later.
//! Instead:
//!
//! * before appending or applying a client write, [`DiskStall::precheck`]
//!   compares the volume with `--min-free-disk` and stalls below it, so
//!   redb does not meet ENOSPC in the common case;
//! * an I/O error that is a full disk (`graph_proto::error::error_is_disk_full`)
//!   is retried in place, with a capped backoff, on the blocking thread
//!   that met it. Before each retry the redb database is closed and
//!   reopened (redb answers `PreviousIo` to everything after an I/O error
//!   until then; the reopen repairs the file if needed). The failed
//!   transaction rolled back: the log holds no part of the entries and the
//!   store's `RAFT_SM` marker has not moved;
//! * while it retries, the stall is visible here: a write waiting on this
//!   node ([`super::RaftNode::propose`]), every new one and a linearizable
//!   read barrier answer `StoreError::Storage("disk full: ...")`
//!   (`RESOURCE_EXHAUSTED`), or `NotLeader` when a stalled log write holds
//!   the Raft core and a peer already leads in a higher term. The write is
//!   not acknowledged; it may still land once space is freed, and a retry
//!   is idempotent (D2/D7);
//! * the node keeps serving `LOCAL` reads. A stalled apply runs on
//!   openraft's state-machine task, so the Raft core goes on. A stalled log
//!   write runs inside the Raft core, so that node sends no heartbeats
//!   meanwhile: a leader loses leadership to a peer with a disk and
//!   rejoins as a follower;
//! * once a retry succeeds the stall clears by itself; shutting the node
//!   down ([`DiskStall::stop`]) ends the retries with the original error.
//!
//! Any other I/O-class error is still fatal.
use crate::disk::DiskGuard;
use graph_store::StoreError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Where a disk-full stall happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallSource {
    /// Appending entries to the Raft log (`raft.redb`).
    LogAppend,
    /// Applying a committed entry to the store.
    Apply,
    /// The Raft log's small writes: vote, committed index, truncate, purge.
    LogMeta,
}

impl StallSource {
    fn what(self) -> &'static str {
        match self {
            StallSource::LogAppend => "append the write to its Raft log",
            StallSource::Apply => "apply the write to its store",
            StallSource::LogMeta => "write its Raft state (vote, commit index, truncate, purge)",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    log: Option<String>,
    apply: Option<String>,
    meta: Option<String>,
}

impl State {
    fn slot(&mut self, src: StallSource) -> &mut Option<String> {
        match src {
            StallSource::LogAppend => &mut self.log,
            StallSource::Apply => &mut self.apply,
            StallSource::LogMeta => &mut self.meta,
        }
    }

    fn first(&self) -> Option<String> {
        self.log
            .clone()
            .or_else(|| self.apply.clone())
            .or_else(|| self.meta.clone())
    }
}

struct Inner {
    state: watch::Sender<State>,
    stopping: AtomicBool,
    fail_log_append: AtomicBool,
    fail_apply: AtomicBool,
    guard: std::sync::Mutex<Option<DiskGuard>>,
}

/// The node's disk-full stall state, shared by the log store, the state
/// machine and the write path. Cheap to clone.
#[derive(Clone)]
pub struct DiskStall {
    inner: Arc<Inner>,
}

impl Default for DiskStall {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DiskStall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStall")
            .field("state", &*self.inner.state.borrow())
            .finish()
    }
}

/// The first retry waits this long; each next one twice as long, up to
/// [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(20);
/// The longest wait between two retries.
const MAX_BACKOFF: Duration = Duration::from_millis(500);
/// How many reopenings in a row may still meet `PreviousIo` (with no
/// disk-full error in between) before the error is returned.
pub const MAX_PREVIOUS_IO: u32 = 5;

impl DiskStall {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: watch::Sender::new(State::default()),
                stopping: AtomicBool::new(false),
                fail_log_append: AtomicBool::new(false),
                fail_apply: AtomicBool::new(false),
                guard: std::sync::Mutex::new(None),
            }),
        }
    }

    /// The client-facing refusal while stalled (`None`: not stalled). It
    /// says "disk full", so the wire maps it to `RESOURCE_EXHAUSTED`.
    pub fn current(&self) -> Option<String> {
        self.inner.state.borrow().first()
    }

    /// Whether this source is stalled right now.
    pub fn is_stalled(&self, src: StallSource) -> bool {
        let s = self.inner.state.borrow();
        match src {
            StallSource::LogAppend => s.log.is_some(),
            StallSource::Apply => s.apply.is_some(),
            StallSource::LogMeta => s.meta.is_some(),
        }
    }

    /// The stalled sources, as `Admin.Status` and `mg_disk_stalled` name
    /// them: `log` (the Raft log's writes) and `apply` (the store).
    pub fn sources(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.is_stalled(StallSource::LogAppend) || self.is_stalled(StallSource::LogMeta) {
            out.push("log".to_string());
        }
        if self.is_stalled(StallSource::Apply) {
            out.push("apply".to_string());
        }
        out
    }

    /// Resolves with the refusal once the node is stalled (at once if it
    /// is already).
    pub async fn stalled(&self) -> String {
        let mut rx = self.inner.state.subscribe();
        loop {
            if let Some(m) = rx.borrow_and_update().first() {
                return m;
            }
            if rx.changed().await.is_err() {
                // The sender lives as long as `self`: never reached.
                std::future::pending::<()>().await;
            }
        }
    }

    /// The node is shutting down: a retry in progress gives up with its
    /// error (within one backoff), and no new one starts.
    pub fn stop(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
    }

    fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    fn set(&self, src: StallSource, cause: &str) -> bool {
        let msg = format!(
            "disk full: this node could not {} ({cause}); the write was not acknowledged and \
             the node keeps retrying; free space on its volume and retry the write (a retry \
             is safe)",
            src.what()
        );
        self.inner.state.send_if_modified(|s| {
            let slot = s.slot(src);
            let newly = slot.is_none();
            *slot = Some(msg);
            newly
        })
    }

    fn clear(&self, src: StallSource) {
        self.inner
            .state
            .send_if_modified(|s| s.slot(src).take().is_some());
    }

    /// Run `f` until it succeeds, retrying in place while it fails with a
    /// disk-full error, or with redb's `PreviousIo` (the database was
    /// poisoned by an earlier I/O error), see the module docs. After each
    /// such failure `recover` runs before the next try: it closes and
    /// reopens the redb database, which clears `PreviousIo` (and repairs
    /// the file if needed). Any other error, or any error once
    /// [`stop`](Self::stop) was called, is returned as is, as is a
    /// `PreviousIo` that persists over [`MAX_PREVIOUS_IO`] reopenings with
    /// no disk-full error seen (a fault a reopen does not cure). Blocks the
    /// calling thread between retries: call it on the blocking pool.
    pub fn retry<T, E: std::error::Error + 'static>(
        &self,
        src: StallSource,
        mut f: impl FnMut() -> Result<T, E>,
        mut recover: impl FnMut(),
    ) -> Result<T, E> {
        let mut backoff = FIRST_BACKOFF;
        let mut retries = 0u64;
        let mut previous_io = 0u32;
        loop {
            match f() {
                Ok(v) => {
                    if retries > 0 {
                        tracing::warn!(
                            source = ?src,
                            retries,
                            "disk space is back: the stalled write went through"
                        );
                    }
                    self.clear(src);
                    return Ok(v);
                }
                Err(e) => {
                    let cause = e.to_string();
                    let full = graph_proto::error::error_is_disk_full(&e);
                    let poisoned = !full && graph_proto::error::is_previous_io(&cause);
                    if poisoned {
                        previous_io += 1;
                    } else if full {
                        previous_io = 0;
                    }
                    if previous_io > MAX_PREVIOUS_IO {
                        tracing::error!(
                            source = ?src,
                            reopens = previous_io - 1,
                            error = %cause,
                            "redb still answers PreviousIo after {MAX_PREVIOUS_IO} reopenings \
                             with no disk-full error: giving up (the node stops)"
                        );
                    }
                    if !(full || poisoned) || previous_io > MAX_PREVIOUS_IO || self.stopping() {
                        return Err(e);
                    }
                    if full && self.set(src, &cause) {
                        tracing::error!(
                            source = ?src,
                            error = %cause,
                            "disk full: retrying in place until space is freed; writes                              answer RESOURCE_EXHAUSTED meanwhile"
                        );
                    }
                    if poisoned {
                        tracing::warn!(source = ?src, error = %cause, "reopening the database");
                    }
                    retries += 1;
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    if self.stopping() {
                        return Err(e);
                    }
                    recover();
                }
            }
        }
    }

    /// The disk guard's floor for a write that grows the files: `Ok` while
    /// the volume has at least `--min-free-disk` free, otherwise a
    /// disk-full error, so [`retry`](Self::retry) stalls before redb meets
    /// ENOSPC (which poisons an open database). No guard: always `Ok`.
    pub fn precheck(&self, what: &str) -> Result<(), StoreError> {
        let g = self
            .inner
            .guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match g {
            Some(g) => g.check_extra(what, 0),
            None => Ok(()),
        }
    }

    /// The node's disk guard, for [`precheck`](Self::precheck).
    pub fn set_guard(&self, guard: Option<DiskGuard>) {
        *self
            .inner
            .guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = guard;
    }

    /// Testing only (a ClusterTestbed failpoint): while on, every log
    /// append of this node fails with a disk-full I/O error just before
    /// its commit.
    #[doc(hidden)]
    pub fn fail_log_append(&self, on: bool) {
        self.inner.fail_log_append.store(on, Ordering::SeqCst);
    }

    /// Testing only (a ClusterTestbed failpoint): while on, every apply of
    /// this node fails with a disk-full I/O error before the entry's store
    /// transaction commits.
    #[doc(hidden)]
    pub fn fail_apply(&self, on: bool) {
        self.inner.fail_apply.store(on, Ordering::SeqCst);
    }

    /// The injected error, when the failpoint for `src` is on.
    pub(crate) fn injected(&self, src: StallSource) -> Option<std::io::Error> {
        let on = match src {
            StallSource::LogAppend => &self.inner.fail_log_append,
            StallSource::Apply => &self.inner.fail_apply,
            StallSource::LogMeta => return None,
        };
        on.load(Ordering::SeqCst).then(|| {
            std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "testing: no space left on device (failpoint)",
            )
        })
    }
}

#[cfg(test)]
#[path = "disk_stall_tests.rs"]
mod tests;
