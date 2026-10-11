//! A small dataflow toolkit in the spirit of .NET TPL Dataflow: pipeline
//! stages run on their own threads and stream into each other through
//! channels, bounded by a shared memory budget (in bytes, sized from the
//! machine) instead of fixed item counts. Every stage records what each of
//! its threads is doing, so the progress display and `--stats` can say what
//! is being worked on and what is waiting on what.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// What one stage thread is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// Working on an item (`verb` e.g. "parsing", `what` e.g. a path).
    Busy {
        verb: &'static str,
        what: String,
    },
    /// Waiting for input from the stage before it.
    Starved {
        on: String,
    },
    /// Waiting for the stage after it (backpressure).
    Blocked {
        on: String,
    },
    Done,
}

struct Slot {
    act: Activity,
    since: Instant,
}

/// Live state and counters of one pipeline stage.
pub struct Stage {
    pub name: &'static str,
    slots: Vec<Mutex<Slot>>,
    busy: AtomicU64,
    starved: AtomicU64,
    blocked: AtomicU64,
    items: AtomicU64,
    bytes: AtomicU64,
    trace: Option<(usize, &'static Trace)>,
}

/// One thread of a stage, as seen by the display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadView {
    pub act: Activity,
    pub elapsed: Duration,
}

/// A copy of a stage's state for rendering.
#[derive(Debug, Clone, Default)]
pub struct StageView {
    pub name: &'static str,
    pub threads: Vec<ThreadView>,
    pub busy: Duration,
    pub starved: Duration,
    pub blocked: Duration,
    pub items: u64,
    pub bytes: u64,
}

impl StageView {
    pub fn count(&self, f: impl Fn(&Activity) -> bool) -> usize {
        self.threads.iter().filter(|t| f(&t.act)).count()
    }
}

impl Stage {
    pub fn new(name: &'static str, threads: usize) -> Self {
        let now = Instant::now();
        Self {
            name,
            slots: (0..threads.max(1))
                .map(|_| {
                    Mutex::new(Slot {
                        act: Activity::Starved { on: "start".into() },
                        since: now,
                    })
                })
                .collect(),
            busy: AtomicU64::new(0),
            starved: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
            items: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            trace: None,
        }
    }

    /// Record spans into `trace` (thread ids offset by `tid_base`).
    pub fn traced(mut self, tid_base: usize, trace: Option<&'static Trace>) -> Self {
        self.trace = trace.map(|t| (tid_base, t));
        self
    }

    fn set(&self, k: usize, act: Activity) -> Instant {
        let now = Instant::now();
        let mut s = self.slots[k].lock().unwrap_or_else(|e| e.into_inner());
        let spent = now - s.since;
        let (acc, name) = match &s.act {
            Activity::Busy { verb, what } => (&self.busy, Some(format!("{verb} {what}"))),
            Activity::Starved { .. } => (&self.starved, None),
            Activity::Blocked { .. } => (&self.blocked, None),
            Activity::Done => (&self.busy, None),
        };
        if !matches!(s.act, Activity::Done) {
            acc.fetch_add(spent.as_nanos() as u64, Relaxed);
        }
        if let (Some((base, t)), Some(name)) = (self.trace, name) {
            t.span(base + k, self.name, &name, s.since, now);
        }
        s.act = act;
        s.since = now;
        now
    }

    /// Run `f` as thread `k` working on `what`.
    pub fn busy<T>(&self, k: usize, verb: &'static str, what: &str, f: impl FnOnce() -> T) -> T {
        self.set(
            k,
            Activity::Busy {
                verb,
                what: what.to_string(),
            },
        );
        let r = f();
        self.set(k, Activity::Starved { on: String::new() });
        r
    }

    /// Run `f` as thread `k` waiting for upstream (`on` says for what).
    pub fn starved<T>(&self, k: usize, on: impl Into<String>, f: impl FnOnce() -> T) -> T {
        self.set(k, Activity::Starved { on: on.into() });
        f()
    }

    /// Run `f` as thread `k` waiting for downstream (`on` says for what).
    pub fn blocked<T>(&self, k: usize, on: impl Into<String>, f: impl FnOnce() -> T) -> T {
        self.set(k, Activity::Blocked { on: on.into() });
        f()
    }

    /// Thread `k` has finished for good.
    pub fn done(&self, k: usize) {
        self.set(k, Activity::Done);
    }

    /// Count `items` items of `bytes` bytes through the stage.
    pub fn count(&self, items: u64, bytes: u64) {
        self.items.fetch_add(items, Relaxed);
        self.bytes.fetch_add(bytes, Relaxed);
    }

    pub fn view(&self) -> StageView {
        let now = Instant::now();
        let mut v = StageView {
            name: self.name,
            items: self.items.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
            ..StageView::default()
        };
        let (mut busy, mut starved, mut blocked) = (
            self.busy.load(Relaxed),
            self.starved.load(Relaxed),
            self.blocked.load(Relaxed),
        );
        for s in &self.slots {
            let s = s.lock().unwrap_or_else(|e| e.into_inner());
            let el = now - s.since;
            match s.act {
                Activity::Busy { .. } => busy += el.as_nanos() as u64,
                Activity::Starved { .. } => starved += el.as_nanos() as u64,
                Activity::Blocked { .. } => blocked += el.as_nanos() as u64,
                Activity::Done => {}
            }
            v.threads.push(ThreadView {
                act: s.act.clone(),
                elapsed: el,
            });
        }
        v.busy = Duration::from_nanos(busy);
        v.starved = Duration::from_nanos(starved);
        v.blocked = Duration::from_nanos(blocked);
        v
    }
}

/// A byte budget shared by the stages: producers acquire before taking in
/// data and the last stage releases after it is done with it, so the data in
/// flight never exceeds the cap (a single item larger than it still goes
/// through, alone). The cap can move while the run goes on (`set_cap`):
/// lowering it below what is held just makes new acquires wait.
pub struct Budget {
    cap: AtomicU64,
    used: Mutex<u64>,
    cv: Condvar,
    peak: AtomicU64,
    cap_min: AtomicU64,
    cap_max: AtomicU64,
    reason: Mutex<String>,
    pressure: AtomicBool,
    pressure_episodes: AtomicU64,
    pressure_ns: AtomicU64,
    pressure_since: Mutex<Option<Instant>>,
    /// Extractor scratch (parse stacks) held now, and at most, included in
    /// `used`.
    scratch: AtomicU64,
    scratch_peak: AtomicU64,
}

/// How the cap moved over a run (`--stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetView {
    pub cap: u64,
    pub used: u64,
    pub peak: u64,
    pub cap_min: u64,
    pub cap_max: u64,
    pub reason: String,
    pub pressure: bool,
    pub pressure_episodes: u64,
    pub pressure_time: Duration,
    /// Most extractor scratch (parse stacks) reserved at once.
    pub scratch_peak: u64,
}

impl Budget {
    pub fn new(cap: u64) -> Self {
        let cap = cap.max(1);
        Self {
            cap: AtomicU64::new(cap),
            used: Mutex::new(0),
            cv: Condvar::new(),
            peak: AtomicU64::new(0),
            cap_min: AtomicU64::new(cap),
            cap_max: AtomicU64::new(cap),
            reason: Mutex::new(String::new()),
            pressure: AtomicBool::new(false),
            pressure_episodes: AtomicU64::new(0),
            pressure_ns: AtomicU64::new(0),
            pressure_since: Mutex::new(None),
            scratch: AtomicU64::new(0),
            scratch_peak: AtomicU64::new(0),
        }
    }

    /// Move the cap (the policy decided from a fresh memory sample).
    pub fn set_cap(&self, cap: u64, reason: &str, pressure: bool) {
        let cap = cap.max(1);
        self.cap.store(cap, Relaxed);
        self.cap_min.fetch_min(cap, Relaxed);
        self.cap_max.fetch_max(cap, Relaxed);
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = reason.to_string();
        let was = self.pressure.swap(pressure, Relaxed);
        if pressure != was {
            let mut since = self
                .pressure_since
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if pressure {
                self.pressure_episodes.fetch_add(1, Relaxed);
                *since = Some(Instant::now());
            } else if let Some(t) = since.take() {
                self.pressure_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            }
        }
        // A raised cap may let waiters through.
        self.cv.notify_all();
    }

    pub fn view(&self) -> BudgetView {
        let mut ns = self.pressure_ns.load(Relaxed);
        if let Some(t) = *self
            .pressure_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            ns += t.elapsed().as_nanos() as u64;
        }
        BudgetView {
            cap: self.cap(),
            used: self.used(),
            peak: self.peak(),
            cap_min: self.cap_min.load(Relaxed),
            cap_max: self.cap_max.load(Relaxed),
            reason: self
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            pressure: self.pressure.load(Relaxed),
            pressure_episodes: self.pressure_episodes.load(Relaxed),
            pressure_time: Duration::from_nanos(ns),
            scratch_peak: self.scratch_peak(),
        }
    }

    /// Wait until `n` bytes fit, or at most `slack` bytes are in flight (0:
    /// nothing else in flight), so an item of any size goes through alone
    /// or on top of at most `slack`. Returns false if `cancel` was set
    /// meanwhile.
    pub fn acquire(&self, n: u64, slack: u64, cancel: &AtomicBool) -> bool {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used > slack && *used + n > self.cap.load(Relaxed) {
            if cancel.load(Relaxed) {
                return false;
            }
            used = self
                .cv
                .wait_timeout(used, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *used += n;
        self.peak.fetch_max(*used, Relaxed);
        true
    }

    pub fn release(&self, n: u64) {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(n);
        self.cv.notify_all();
    }

    /// Reserve `n` bytes of an extractor's scratch memory (the Rust parse
    /// stack, #254) on top of the bytes in flight, so admission sees it too.
    /// Waits until it fits under the cap. It goes through over the cap only
    /// when no other scratch is held and `oldest()` says the caller's file
    /// is the oldest still being parsed: everything held beyond it then
    /// waits on it (the writer commits in walk order), so waiting longer
    /// could never end. Or when `cancel` is set.
    pub fn acquire_scratch(&self, n: u64, oldest: &dyn Fn() -> bool, cancel: &AtomicBool) {
        // Lock order: `oldest()` takes the pipeline's `inflight` lock while
        // `used` is held (`used` -> `inflight`); nothing takes `used` while
        // holding `inflight`. A benign race: a younger job can briefly see
        // itself as oldest (an older job just left `inflight` but has not
        // reached the writer yet). It then goes over the cap only while no
        // other scratch is held, so at most one stack is ever over the cap.
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used + n > self.cap.load(Relaxed)
            && !(self.scratch.load(Relaxed) == 0 && oldest())
            && !cancel.load(Relaxed)
        {
            used = self
                .cv
                .wait_timeout(used, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *used += n;
        let scratch = self.scratch.fetch_add(n, Relaxed) + n;
        self.scratch_peak.fetch_max(scratch, Relaxed);
        self.peak.fetch_max(*used, Relaxed);
    }

    /// Give back scratch reserved with [`Budget::acquire_scratch`].
    pub fn release_scratch(&self, n: u64) {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(n);
        // Only changed under the `used` lock.
        let s = self.scratch.load(Relaxed);
        self.scratch.store(s.saturating_sub(n), Relaxed);
        self.cv.notify_all();
    }

    /// Most scratch held at once.
    pub fn scratch_peak(&self) -> u64 {
        self.scratch_peak.load(Relaxed)
    }

    pub fn wake_all(&self) {
        self.cv.notify_all();
    }

    pub fn cap(&self) -> u64 {
        self.cap.load(Relaxed)
    }
    pub fn used(&self) -> u64 {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn peak(&self) -> u64 {
        self.peak.load(Relaxed)
    }
}

/// Chrome / Perfetto trace-event recorder (`--trace`): one complete event
/// ("X") per busy span.
pub struct Trace {
    start: Instant,
    events: Mutex<Vec<serde_json::Value>>,
}

impl Trace {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn span(&self, tid: usize, cat: &str, name: &str, from: Instant, to: Instant) {
        let us = |t: Instant| t.saturating_duration_since(self.start).as_micros() as u64;
        let ev = serde_json::json!({
            "name": name, "cat": cat, "ph": "X", "pid": 1, "tid": tid,
            "ts": us(from), "dur": us(to).saturating_sub(us(from)),
        });
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ev);
    }

    /// Name a thread in the trace viewer.
    pub fn thread_name(&self, tid: usize, name: &str) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(serde_json::json!({
                "name": "thread_name", "ph": "M", "pid": 1, "tid": tid,
                "args": { "name": name },
            }));
    }

    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let doc = serde_json::json!({ "traceEvents": *events });
        std::fs::write(path, serde_json::to_vec(&doc)?)
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_accounts_busy_starved_blocked() {
        let s = Stage::new("parse", 2);
        s.busy(0, "parsing", "a.rs", || {
            std::thread::sleep(Duration::from_millis(20))
        });
        s.blocked(1, "memory", || {
            std::thread::sleep(Duration::from_millis(20))
        });
        s.count(1, 10);
        let v = s.view();
        assert!(v.busy >= Duration::from_millis(20), "{v:?}");
        assert!(v.blocked >= Duration::from_millis(20), "{v:?}");
        assert_eq!((v.items, v.bytes), (1, 10));
        assert_eq!(v.count(|a| matches!(a, Activity::Blocked { .. })), 1);
        s.done(0);
        assert_eq!(s.view().threads[0].act, Activity::Done);
    }

    #[test]
    fn budget_bounds_bytes_in_flight() {
        let b = std::sync::Arc::new(Budget::new(100));
        let cancel = AtomicBool::new(false);
        assert!(b.acquire(60, 0, &cancel));
        assert!(b.acquire(40, 0, &cancel));
        let b2 = b.clone();
        let t = std::thread::spawn(move || {
            let c = AtomicBool::new(false);
            b2.acquire(30, 0, &c)
        });
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(b.used(), 100, "third acquire waits");
        b.release(60);
        assert!(t.join().unwrap());
        assert_eq!(b.used(), 70);
        assert!(b.peak() <= 100);
        // An item bigger than the budget goes through alone.
        b.release(70);
        assert!(b.acquire(500, 0, &cancel));
        // Cancel wakes a waiter.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (b2, c2) = (b.clone(), cancel.clone());
        let t = std::thread::spawn(move || b2.acquire(1, 0, &c2));
        cancel.store(true, Relaxed);
        b.wake_all();
        assert!(!t.join().unwrap());
    }

    #[test]
    fn slack_admits_any_size_on_top_of_a_small_load() {
        let b = std::sync::Arc::new(Budget::new(100));
        let cancel = AtomicBool::new(false);
        // Up to `slack` in flight: an item of any size goes through.
        assert!(b.acquire(30, 32, &cancel));
        assert!(b.acquire(500, 32, &cancel));
        assert_eq!(b.used(), 530);
        // Over `slack` and over the cap: waits until enough is released.
        let b2 = b.clone();
        let t = std::thread::spawn(move || {
            let c = AtomicBool::new(false);
            b2.acquire(1, 32, &c)
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished(), "waits while over slack and cap");
        b.release(500);
        assert!(t.join().unwrap());
        assert_eq!(b.used(), 31);
    }

    #[test]
    fn scratch_waits_under_the_cap_unless_its_file_is_oldest_and_alone() {
        let b = std::sync::Arc::new(Budget::new(100));
        let cancel = AtomicBool::new(false);
        assert!(b.acquire(10, 0, &cancel));
        b.acquire_scratch(60, &|| false, &cancel);
        assert_eq!((b.used(), b.scratch_peak()), (70, 60));
        // Does not fit: waits, even for the oldest file, while scratch is held.
        let b2 = b.clone();
        let t = std::thread::spawn(move || {
            b2.acquire_scratch(60, &|| true, &AtomicBool::new(false));
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished(), "waits while another scratch is held");
        b.release_scratch(60);
        t.join().unwrap();
        assert!(b.peak() <= 100, "{}", b.peak());
        b.release_scratch(60);
        // The oldest file alone goes through over the cap (no deadlock).
        b.acquire_scratch(500, &|| true, &cancel);
        assert_eq!(b.used(), 510);
        b.release_scratch(500);
        assert_eq!(b.used(), 10);
        // A file that is not the oldest waits until it fits or a cancel.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (b2, c2) = (b.clone(), cancel.clone());
        let t = std::thread::spawn(move || b2.acquire_scratch(500, &|| false, &c2));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished());
        cancel.store(true, Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn cap_moves_and_pressure_is_tracked() {
        let b = std::sync::Arc::new(Budget::new(100));
        let cancel = AtomicBool::new(false);
        assert!(b.acquire(80, 0, &cancel));
        // Lowered below what is held: a new acquire waits...
        b.set_cap(50, "pressure: test", true);
        let b2 = b.clone();
        let t = std::thread::spawn(move || {
            let c = AtomicBool::new(false);
            b2.acquire(10, 0, &c)
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished(), "waits while over the lowered cap");
        // ...until the cap is raised again (or bytes are released).
        b.set_cap(200, "plenty", false);
        assert!(t.join().unwrap());
        let v = b.view();
        assert_eq!((v.cap_min, v.cap_max, v.cap), (50, 200, 200));
        assert_eq!(v.pressure_episodes, 1);
        assert!(v.pressure_time >= Duration::from_millis(30));
        assert!(!v.pressure);
        assert_eq!(v.reason, "plenty");
    }

    #[test]
    fn trace_is_valid_json() {
        let t = Trace::new();
        t.thread_name(0, "parse 0");
        let now = Instant::now();
        t.span(
            0,
            "parse",
            "parsing a.rs",
            now,
            now + Duration::from_millis(1),
        );
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.json");
        t.write(&p).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        assert_eq!(v["traceEvents"].as_array().unwrap().len(), 2);
    }
}
