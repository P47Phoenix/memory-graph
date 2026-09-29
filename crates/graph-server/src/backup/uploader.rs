//! [`Backup`]: the uploader (ADR 0006 E2, E7, E9 and the failure modes).
//!
//! A snapshot build hands it a job ([`Backup::snapshot_built`]) and returns
//! at once. One worker thread uploads one snapshot at a time; a job that
//! arrives while one is queued replaces it (the newer snapshot supersedes
//! the older), so a slow or failing sink costs at most one upload's worth of
//! work and never holds up a build or a purge. Each upload streams the data
//! object from an open handle on the snapshot (see the module docs of
//! [`super`] for why not a copy), checks the bytes read against the
//! sidecar, then writes the `.meta` (the commit), then applies retention.
use super::{parse_stem, BackupConfig, BackupOn, BackupSink, ObjectInfo, UPLOAD_RETRIES};
use crate::paths::ClusterIdentity;
use crate::raft::snapshot_dir::{hex, SnapshotSidecar};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

/// Whether this node is the leader right now.
pub type IsLeader = Box<dyn Fn() -> bool + Send + Sync>;

/// What `cluster status` and `/metrics` report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupStats {
    /// Log index of the last snapshot committed to the sink (0: none yet).
    pub last_index: u64,
    /// Unix seconds of that commit (0: none yet).
    pub last_success_unix: u64,
    /// Uploads that failed after every retry.
    pub failures_total: u64,
    /// Bytes written to the sink (data and `.meta`), successful uploads.
    pub bytes_total: u64,
    /// Snapshots committed since start.
    pub uploads_total: u64,
    /// The last failure's message; cleared by the next success.
    pub last_error: String,
}

struct Job {
    side: SnapshotSidecar,
    path: PathBuf,
}

#[derive(Default)]
struct Queue {
    pending: Option<Job>,
    busy: bool,
    stop: bool,
}

struct Inner {
    cfg: BackupConfig,
    sink: Arc<dyn BackupSink>,
    identity: Arc<ClusterIdentity>,
    is_leader: OnceLock<IsLeader>,
    queue: Mutex<Queue>,
    cv: Condvar,
    stats: Mutex<BackupStats>,
}

/// The uploader of one server; cheap to clone.
#[derive(Clone)]
pub struct Backup {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Backup {
    /// Open the sink and start the worker thread.
    pub fn start(cfg: BackupConfig, identity: Arc<ClusterIdentity>) -> Result<Self, String> {
        let sink = cfg.open_sink()?;
        let inner = Arc::new(Inner {
            cfg,
            sink,
            identity,
            is_leader: OnceLock::new(),
            queue: Mutex::new(Queue::default()),
            cv: Condvar::new(),
            stats: Mutex::new(BackupStats::default()),
        });
        let worker = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("backup-upload".into())
            .spawn(move || worker.run())
            .map_err(|e| format!("starting the backup thread: {e}"))?;
        tracing::info!(
            sink = %inner.sink.describe(),
            on = %inner.cfg.on,
            keep = inner.cfg.keep,
            "snapshot backups enabled"
        );
        Ok(Self { inner })
    }

    /// Tell it how to know whether this node leads (set once the Raft node
    /// runs; builds before that are not uploaded under `--backup-on
    /// leader`).
    pub fn set_is_leader(&self, f: IsLeader) {
        let _ = self.inner.is_leader.set(f);
    }

    pub fn on(&self) -> BackupOn {
        self.inner.cfg.on
    }

    pub fn sink(&self) -> &Arc<dyn BackupSink> {
        &self.inner.sink
    }

    pub fn stats(&self) -> BackupStats {
        lock(&self.inner.stats).clone()
    }

    /// A snapshot build completed (called on the build's thread): queue it
    /// if this node uploads, replacing any queued one. Never blocks.
    pub fn snapshot_built(&self, side: &SnapshotSidecar, path: &Path) {
        let i = &self.inner;
        let upload = match i.cfg.on {
            BackupOn::None => false,
            BackupOn::All => true,
            BackupOn::Leader => i.is_leader.get().is_some_and(|f| f()),
        };
        if !upload {
            return;
        }
        let mut q = lock(&i.queue);
        if q.stop {
            return;
        }
        if let Some(old) = &q.pending {
            tracing::info!(
                superseded = old.side.index,
                by = side.index,
                "backup: a newer snapshot supersedes the queued upload"
            );
        }
        q.pending = Some(Job {
            side: side.clone(),
            path: path.to_path_buf(),
        });
        i.cv.notify_all();
    }

    /// Wait until nothing is queued or uploading (tests), up to `timeout`;
    /// whether it got there.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let i = &self.inner;
        let q = lock(&i.queue);
        let (q, _) =
            i.cv.wait_timeout_while(q, timeout, |q| q.pending.is_some() || q.busy)
                .unwrap_or_else(PoisonError::into_inner);
        q.pending.is_none() && !q.busy
    }

    /// Stop the worker after its current upload (never waits for it: a
    /// hung sink must not hold up a shutdown).
    pub fn stop(&self) {
        let mut q = lock(&self.inner.queue);
        q.stop = true;
        q.pending = None;
        self.inner.cv.notify_all();
    }
}

/// Reads through to `inner`, hashing and counting.
struct Hashing<R> {
    inner: R,
    hash: Sha256,
    n: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.hash.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
}

/// Why one upload attempt ended without a commit.
enum Outcome {
    /// Retry (and count if the retries run out).
    Failed(String),
    /// Not a failure: a newer snapshot replaced the file before it was
    /// opened, or there is no cluster id to key it by yet.
    Skipped(String),
}

impl Inner {
    fn run(self: Arc<Self>) {
        loop {
            let job = {
                let q = lock(&self.queue);
                let mut q = self
                    .cv
                    .wait_while(q, |q| q.pending.is_none() && !q.stop)
                    .unwrap_or_else(PoisonError::into_inner);
                if q.stop {
                    return;
                }
                q.busy = true;
                q.pending.take().expect("woken with a job")
            };
            self.upload_with_retries(&job);
            let mut q = lock(&self.queue);
            q.busy = false;
            self.cv.notify_all();
        }
    }

    fn superseded_or_stopped(&self) -> bool {
        let q = lock(&self.queue);
        q.stop || q.pending.is_some()
    }

    fn upload_with_retries(&self, job: &Job) {
        let mut attempt = 0;
        loop {
            match self.upload_once(job) {
                Ok(bytes) => {
                    let mut s = lock(&self.stats);
                    s.last_index = job.side.index;
                    s.last_success_unix = crate::paths::now_secs();
                    s.bytes_total += bytes;
                    s.uploads_total += 1;
                    s.last_error.clear();
                    drop(s);
                    tracing::info!(
                        index = job.side.index,
                        term = job.side.term,
                        bytes,
                        sink = %self.sink.describe(),
                        "backup: snapshot committed"
                    );
                    if let Err(e) = self.retain() {
                        // Retention is housekeeping: logged, not a failure
                        // of the backup that just committed.
                        tracing::warn!(error = %e, "backup: retention failed");
                    }
                    return;
                }
                Err(Outcome::Skipped(why)) => {
                    tracing::info!(index = job.side.index, "backup: skipped: {why}");
                    return;
                }
                Err(Outcome::Failed(e)) => {
                    if attempt >= UPLOAD_RETRIES || self.superseded_or_stopped() {
                        let msg = format!(
                            "backing up snapshot {} (term {}) to {}: {e}",
                            job.side.index,
                            job.side.term,
                            self.sink.describe()
                        );
                        tracing::error!("backup failed after {} attempts: {msg}", attempt + 1);
                        let mut s = lock(&self.stats);
                        s.failures_total += 1;
                        s.last_error = msg;
                        return;
                    }
                    let delay = self.cfg.retry_backoff * 2u32.pow(attempt);
                    tracing::warn!(
                        attempt = attempt + 1,
                        ?delay,
                        error = %e,
                        "backup: upload failed; retrying"
                    );
                    attempt += 1;
                    // Wait for the backoff, waking early for a stop or a
                    // newer job (which then wins, see above).
                    let q = lock(&self.queue);
                    let _ = self
                        .cv
                        .wait_timeout_while(q, delay, |q| !q.stop && q.pending.is_none());
                }
            }
        }
    }

    fn prefix(&self) -> Result<String, Outcome> {
        match self.identity.get() {
            Some(id) if !id.is_empty() => Ok(format!("{id}/")),
            _ => Err(Outcome::Skipped("this node has no cluster id yet".into())),
        }
    }

    /// Data first, verified against the sidecar, then the `.meta`.
    fn upload_once(&self, job: &Job) -> Result<u64, Outcome> {
        let prefix = self.prefix()?;
        let stem = crate::raft::snapshot_dir::stem(job.side.last_log_id.as_ref());
        let file = match std::fs::File::open(&job.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Outcome::Skipped(format!(
                    "`{}` was replaced by a newer snapshot before the upload began",
                    job.path.display()
                )))
            }
            Err(e) => return Err(Outcome::Failed(format!("opening the snapshot: {e}"))),
        };
        let mut src = Hashing {
            inner: std::io::BufReader::with_capacity(1 << 20, file),
            hash: Sha256::new(),
            n: 0,
        };
        let data_key = format!("{prefix}{stem}.redb");
        let meta_key = format!("{prefix}{stem}.meta");
        let put = self
            .sink
            .put(&data_key, &mut src)
            .map_err(|e| Outcome::Failed(format!("writing `{data_key}`: {e}")))?;
        let sha = hex(&src.hash.finalize());
        if src.n != job.side.size || put != job.side.size || sha != job.side.sha256 {
            // Never commit bytes the sidecar does not describe.
            let _ = self.sink.delete(&data_key);
            return Err(Outcome::Failed(format!(
                "the snapshot read back as {} bytes (sha256 {sha}), but its meta records {} \
                 bytes (sha256 {})",
                src.n, job.side.size, job.side.sha256
            )));
        }
        let meta = serde_json::to_vec_pretty(&job.side).expect("sidecar serializes");
        self.sink
            .put(&meta_key, &mut meta.as_slice())
            .map_err(|e| Outcome::Failed(format!("writing `{meta_key}`: {e}")))?;
        Ok(put + meta.len() as u64)
    }

    /// Keep the newest `keep` committed backups of this cluster (the
    /// `.meta` of an older one goes first, then its data), and sweep data
    /// objects without a `.meta` older than the orphan age. Only this
    /// cluster's prefix is touched.
    fn retain(&self) -> std::io::Result<()> {
        let Ok(prefix) = self.prefix() else {
            return Ok(());
        };
        let objects = self.sink.list(&prefix)?;
        let removed = plan_retention(
            &prefix,
            &objects,
            self.cfg.keep,
            self.cfg.orphan_age,
            SystemTime::now(),
        );
        for key in removed {
            tracing::info!(key = %key, "backup: retention removes");
            self.sink.delete(&key)?;
        }
        Ok(())
    }
}

/// The keys retention deletes, in order (each older backup's `.meta`
/// before its data; then orphans).
pub fn plan_retention(
    prefix: &str,
    objects: &[ObjectInfo],
    keep: usize,
    orphan_age: Duration,
    now: SystemTime,
) -> Vec<String> {
    // Only direct children of this cluster's prefix.
    let mine: Vec<&ObjectInfo> = objects
        .iter()
        .filter(|o| {
            o.key
                .strip_prefix(prefix)
                .is_some_and(|rest| !rest.contains('/'))
        })
        .collect();
    let mut metas: Vec<(u64, u64, &str)> = mine
        .iter()
        .filter(|o| o.key.ends_with(".meta"))
        .filter_map(|o| parse_stem(&o.key).map(|(t, i)| (i, t, o.key.as_str())))
        .collect();
    metas.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = Vec::new();
    let mut committed = std::collections::BTreeSet::new();
    for (n, (_, _, meta)) in metas.iter().enumerate() {
        let data = format!("{}.redb", meta.strip_suffix(".meta").expect("a meta"));
        if keep == 0 || n < keep {
            committed.insert(data);
        } else {
            out.push(meta.to_string());
            out.push(data);
        }
    }
    for o in mine {
        if o.key.ends_with(".meta") || committed.contains(&o.key) || out.contains(&o.key) {
            continue;
        }
        let old = now
            .duration_since(o.modified)
            .is_ok_and(|age| age >= orphan_age);
        if old {
            out.push(o.key.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(key: &str, age_h: u64) -> ObjectInfo {
        ObjectInfo {
            key: key.into(),
            size: 1,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_h * 3600),
        }
    }

    #[test]
    fn retention_keeps_the_newest_and_sweeps_old_orphans_only() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let objects = vec![
            obj("c/snap-1-10.meta", 1),
            obj("c/snap-1-10.redb", 1),
            obj("c/snap-1-20.meta", 1),
            obj("c/snap-1-20.redb", 1),
            obj("c/snap-2-30.meta", 1),
            obj("c/snap-2-30.redb", 1),
            // An interrupted upload (no meta): young, then old.
            obj("c/snap-2-40.redb", 1),
            obj("c/snap-2-5.redb", 30),
            obj("c/snap-2-6.redb.part-0000abcd", 30),
            // Another cluster's prefix, and a nested key: never touched.
            obj("d/snap-1-1.meta", 99),
            obj("c/x/snap-1-1.redb", 99),
        ];
        let del = plan_retention("c/", &objects, 2, ORPHAN, now);
        assert_eq!(
            del,
            [
                "c/snap-1-10.meta",
                "c/snap-1-10.redb",
                "c/snap-2-5.redb",
                "c/snap-2-6.redb.part-0000abcd"
            ]
        );
        // keep 0: keep every committed backup.
        let del = plan_retention("c/", &objects, 0, ORPHAN, now);
        assert!(!del.iter().any(|k| k.ends_with(".meta")), "{del:?}");
    }

    const ORPHAN: Duration = super::super::ORPHAN_AGE;
}
