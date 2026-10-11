//! [`DiskStall`]: a full disk while appending to the Raft log or applying
//! an entry to the store does not stop the node (ADR 0004 D7, note of
//! 2026-10-11, story 67 / #115).
//!
//! openraft 0.9 treats every error of `append` and `apply` as fatal (the
//! node stops), and `apply` must answer every entry it is given, so an
//! entry cannot be skipped and answered later. Instead, an I/O error that
//! says the disk is full (`graph_proto::error::is_disk_full`) is retried in
//! place, with a capped backoff, on the blocking thread that met it:
//!
//! * the failed redb transaction rolled back, so a retry starts clean: the
//!   log holds no part of the entries and the store's `RAFT_SM` marker has
//!   not moved;
//! * while it retries, the stall is visible here: a write waiting on this
//!   node ([`super::RaftNode::propose`]) and every new one answer
//!   `StoreError::Storage("disk full: ...")`, which the wire maps to
//!   `RESOURCE_EXHAUSTED`. The write is not acknowledged; its entry may
//!   still land once space is freed, and a retry is idempotent (D2/D7);
//! * the node keeps serving `LOCAL` reads from its store. A stalled apply
//!   runs on openraft's state-machine task, so the Raft core goes on
//!   (heartbeats, replication, a follower's log keeps growing and it
//!   catches up once space is freed). A stalled log append runs inside the
//!   Raft core, so that node sends no heartbeats meanwhile: a leader loses
//!   leadership to a peer with a disk, and rejoins as a follower;
//! * once the retry succeeds the stall clears by itself;
//! * shutting the node down ([`DiskStall::stop`]) ends the retries with the
//!   original error.
//!
//! Any other I/O-class error is still fatal, as before.
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
}

impl StallSource {
    fn what(self) -> &'static str {
        match self {
            StallSource::LogAppend => "append the write to its Raft log",
            StallSource::Apply => "apply the write to its store",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    log: Option<String>,
    apply: Option<String>,
}

impl State {
    fn slot(&mut self, src: StallSource) -> &mut Option<String> {
        match src {
            StallSource::LogAppend => &mut self.log,
            StallSource::Apply => &mut self.apply,
        }
    }

    fn first(&self) -> Option<String> {
        self.log.clone().or_else(|| self.apply.clone())
    }
}

struct Inner {
    state: watch::Sender<State>,
    stopping: AtomicBool,
    fail_log_append: AtomicBool,
    fail_apply: AtomicBool,
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

impl DiskStall {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: watch::Sender::new(State::default()),
                stopping: AtomicBool::new(false),
                fail_log_append: AtomicBool::new(false),
                fail_apply: AtomicBool::new(false),
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
        }
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
    /// disk-full error (see the module docs). Any other error, or any error
    /// once [`stop`](Self::stop) was called, is returned as is. Blocks the
    /// calling thread between retries: call it on the blocking pool.
    pub fn retry<T, E: std::fmt::Display>(
        &self,
        src: StallSource,
        mut f: impl FnMut() -> Result<T, E>,
    ) -> Result<T, E> {
        let mut backoff = FIRST_BACKOFF;
        let mut retries = 0u64;
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
                    if !graph_proto::error::is_disk_full(&cause) || self.stopping() {
                        return Err(e);
                    }
                    if self.set(src, &cause) {
                        tracing::error!(
                            source = ?src,
                            error = %cause,
                            "disk full: retrying in place until space is freed; writes \
                             answer RESOURCE_EXHAUSTED meanwhile"
                        );
                    }
                    retries += 1;
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
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
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn a_disk_full_error_is_retried_until_it_succeeds_and_then_clears() {
        let s = DiskStall::new();
        let calls = AtomicU32::new(0);
        let seen = std::thread::scope(|sc| {
            let h = sc.spawn(|| {
                s.retry(StallSource::Apply, || {
                    if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                        Err("No space left on device (os error 28)".to_string())
                    } else {
                        Ok(7)
                    }
                })
            });
            h.join().unwrap()
        });
        assert_eq!(seen, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(s.current(), None, "cleared once it succeeded");
    }

    #[test]
    fn other_errors_are_returned_at_once() {
        let s = DiskStall::new();
        let r: Result<(), String> = s.retry(StallSource::LogAppend, || Err("corrupt".into()));
        assert_eq!(r, Err("corrupt".into()));
        assert_eq!(s.current(), None);
    }

    #[test]
    fn a_stall_is_visible_and_stop_ends_the_retries() {
        let s = DiskStall::new();
        let s2 = s.clone();
        let h = std::thread::spawn(move || {
            s2.retry(StallSource::LogAppend, || {
                Err::<(), _>("disk full".to_string())
            })
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let msg = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), s.stalled())
                .await
                .unwrap()
        });
        assert!(graph_proto::error::is_disk_full(&msg), "{msg}");
        assert!(msg.contains("Raft log"), "{msg}");
        assert!(s.is_stalled(StallSource::LogAppend));
        assert!(!s.is_stalled(StallSource::Apply));
        s.stop();
        assert_eq!(h.join().unwrap(), Err("disk full".into()));
    }

    #[test]
    fn the_failpoint_error_reads_as_disk_full() {
        let s = DiskStall::new();
        assert!(s.injected(StallSource::Apply).is_none());
        s.fail_apply(true);
        let e = s.injected(StallSource::Apply).unwrap();
        assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e}");
        assert!(s.injected(StallSource::LogAppend).is_none());
    }
}
