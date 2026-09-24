//! Live progress for `memory-graph index`: one overall bar plus one line per
//! parsing worker, drawn on stderr (stdout keeps only the final summary).
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Worker lines shown at most; the rest are summarized on one line.
const MAX_WORKER_LINES: usize = 16;

/// Thread-safe progress reporter. Hidden reporters keep the counters but draw
/// nothing, so tests can check them.
pub struct Progress {
    multi: MultiProgress,
    overall: ProgressBar,
    label: String,
    workers: Mutex<Vec<ProgressBar>>,
    more: Mutex<Option<ProgressBar>>,
    total_bytes: AtomicU64,
    done_bytes: AtomicU64,
    skipped: AtomicUsize,
    failed: AtomicUsize,
}

impl Progress {
    /// Draw on stderr (about 10 redraws a second).
    pub fn stderr(label: &str) -> Self {
        Self::new(label, ProgressDrawTarget::stderr())
    }

    /// Draw nothing.
    pub fn hidden() -> Self {
        Self::new("", ProgressDrawTarget::hidden())
    }

    fn new(label: &str, target: ProgressDrawTarget) -> Self {
        let multi = MultiProgress::with_draw_target(target);
        let overall = multi.add(ProgressBar::new_spinner());
        overall.set_style(
            ProgressStyle::with_template("{spinner} walking {prefix}: {pos} files found")
                .expect("valid template"),
        );
        overall.set_prefix(label.to_string());
        Self {
            multi,
            overall,
            label: label.to_string(),
            workers: Mutex::new(Vec::new()),
            more: Mutex::new(None),
            total_bytes: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            skipped: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        }
    }

    /// The walk has found `n` entries so far.
    pub fn walked(&self, n: usize) {
        self.overall.set_position(n as u64);
    }

    /// The walk is over: `files` entries (files and walk-time skips) and
    /// about `bytes` bytes to process.
    pub fn start(&self, files: usize, bytes: u64) {
        self.total_bytes.store(bytes, Ordering::Relaxed);
        self.overall.set_style(
            ProgressStyle::with_template(
                "indexing {prefix} [{bar:30}] {pos}/{len} files  {msg}  {per_sec}  ETA {eta}",
            )
            .expect("valid template")
            .progress_chars("=> "),
        );
        self.overall.set_prefix(self.label.clone());
        self.overall.set_length(files as u64);
        self.overall.set_position(0);
        self.overall.reset_eta();
        self.redraw_msg();
    }

    /// Show one line per worker (at most `MAX_WORKER_LINES`, then one
    /// `… +N more workers` line).
    pub fn set_workers(&self, n: usize) {
        let mut ws = self.workers.lock().expect("progress lock");
        for i in ws.len()..n.min(MAX_WORKER_LINES) {
            let bar = self.multi.add(ProgressBar::new_spinner());
            bar.set_style(ProgressStyle::with_template("  {prefix} {msg}").expect("valid"));
            bar.set_prefix(format!("worker {i:>2}"));
            bar.set_message("(idle)");
            ws.push(bar);
        }
        if n > MAX_WORKER_LINES {
            let more = self.multi.add(ProgressBar::new_spinner());
            more.set_style(ProgressStyle::with_template("  {msg}").expect("valid"));
            more.set_message(format!("… +{} more workers", n - MAX_WORKER_LINES));
            *self.more.lock().expect("progress lock") = Some(more);
        }
    }

    /// Worker `k` started on `path`, or went idle (`None`).
    pub fn worker(&self, k: usize, path: Option<&str>) {
        if let Some(bar) = self.workers.lock().expect("progress lock").get(k) {
            bar.set_message(path.map_or_else(|| "(idle)".to_string(), str::to_string));
        }
    }

    /// `n` entries were skipped before reaching the store.
    pub fn skipped(&self, n: usize) {
        self.skipped.fetch_add(n, Ordering::Relaxed);
        self.overall.inc(n as u64);
        self.redraw_msg();
    }

    /// A batch of `n` files was committed, of which `skipped` were skipped and
    /// `failed` failed by the store.
    pub fn committed(&self, n: usize, skipped: usize, failed: usize) {
        self.skipped.fetch_add(skipped, Ordering::Relaxed);
        self.failed.fetch_add(failed, Ordering::Relaxed);
        self.overall.inc(n as u64);
        self.redraw_msg();
    }

    /// `bytes` more source bytes were read.
    pub fn read_bytes(&self, bytes: u64) {
        self.done_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn redraw_msg(&self) {
        const MB: f64 = 1024.0 * 1024.0;
        self.overall.set_message(format!(
            "{:.1}/{:.1} MB  skipped {} failed {}",
            self.done_bytes.load(Ordering::Relaxed) as f64 / MB,
            self.total_bytes.load(Ordering::Relaxed) as f64 / MB,
            self.skipped.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        ));
    }

    /// Entries processed so far (committed or skipped).
    pub fn position(&self) -> u64 {
        self.overall.position()
    }

    /// Entries to process, once the walk is over.
    pub fn length(&self) -> Option<u64> {
        self.overall.length()
    }

    /// (skipped, failed) so far.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.skipped.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// Worker lines currently shown (excluding the `… +N more` line).
    pub fn worker_lines(&self) -> usize {
        self.workers.lock().expect("progress lock").len()
    }

    /// Remove every line from the terminal.
    pub fn finish(&self) {
        for w in self.workers.lock().expect("progress lock").iter() {
            w.finish_and_clear();
        }
        if let Some(m) = self.more.lock().expect("progress lock").as_ref() {
            m.finish_and_clear();
        }
        self.overall.finish_and_clear();
        let _ = self.multi.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_worker_cap() {
        let p = Progress::hidden();
        p.walked(3);
        p.start(10, 2048);
        assert_eq!((p.position(), p.length()), (0, Some(10)));
        p.skipped(2);
        p.committed(5, 1, 1);
        assert_eq!(p.position(), 7);
        assert_eq!(p.counts(), (3, 1));
        p.set_workers(4);
        p.worker(0, Some("a.rs"));
        p.worker(3, None);
        p.worker(9, Some("ignored.rs"));
        assert_eq!(p.worker_lines(), 4);
        let q = Progress::hidden();
        q.set_workers(40);
        q.worker(39, Some("z.rs"));
        assert_eq!(q.worker_lines(), MAX_WORKER_LINES);
        assert!(q.more.lock().unwrap().is_some());
        p.finish();
    }
}
