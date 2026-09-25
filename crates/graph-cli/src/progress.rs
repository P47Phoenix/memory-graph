//! Live view of `memory-graph index`: one line per pipeline stage saying what
//! it is working on, or what it is waiting for and why, plus the overall bar,
//! the bottleneck and memory in flight. Drawn on stderr (stdout keeps only the
//! final summary); `--stats` prints the same counters as a table at the end.
use crate::dataflow::{Activity, Budget, BudgetView, Stage, StageView, Trace};
use crate::diskinfo::DiskSample;
use crate::sysinfo::{MemSample, Sizing};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Everything the pipeline reports, shared by its threads.
pub struct Board {
    pub label: String,
    pub sizing: Sizing,
    pub start: Instant,
    pub walk: Stage,
    pub parse: Stage,
    pub commit: Stage,
    pub budget: Budget,
    /// Entries found by the walk so far (files and walk-time skips).
    pub found: AtomicU64,
    pub found_bytes: AtomicU64,
    pub walk_done: AtomicBool,
    /// Entries fully handled (committed, skipped or failed).
    pub handled: AtomicU64,
    pub handled_bytes: AtomicU64,
    pub skipped: AtomicU64,
    pub failed: AtomicU64,
    pub txns: AtomicU64,
    pub last_txn: AtomicU64,
    /// The latest good memory sample (from the sampler thread), and why the
    /// latest probe failed if it did (a transient failure keeps the sample).
    pub memory: std::sync::Mutex<(Option<MemSample>, Option<String>)>,
    /// Measured heap growth per source byte in flight (`f64` bits).
    pub expansion: AtomicU64,
    /// Heap footprint of the prepared files in flight.
    pub footprint: AtomicU64,
    /// Source bytes of those prepared files (the rest of what is held is
    /// still being read or parsed).
    pub prepared_bytes: AtomicU64,
    /// The latest disk reading, the database's size, the projected final
    /// size, the ratio behind it (`f64` bits), the reserve, and why the
    /// disk guard stopped the run (if it did).
    pub disk: std::sync::Mutex<Option<DiskSample>>,
    pub db_len: AtomicU64,
    pub db_len_start: AtomicU64,
    /// Source bytes of handled files the store already had (unchanged).
    pub unchanged_bytes: AtomicU64,
    pub disk_projected: AtomicU64,
    pub disk_ratio: AtomicU64,
    pub disk_min_free: AtomicU64,
    pub disk_stop: std::sync::Mutex<Option<String>>,
    pub disk_check: AtomicBool,
    /// Source bytes one group commit may take so it fits on the volume
    /// (set by the sampler; `u64::MAX` until a reading arrives).
    pub disk_group_cap: AtomicU64,
}

impl Board {
    pub fn new(label: &str, sizing: Sizing, trace: Option<&'static Trace>) -> Self {
        let t = sizing.parse_threads;
        if let Some(tr) = trace {
            tr.thread_name(0, "walk");
            tr.thread_name(1, "commit");
            for k in 0..t {
                tr.thread_name(100 + k, &format!("parse {k}"));
            }
        }
        Self {
            label: label.to_string(),
            budget: {
                let b = Budget::new(sizing.memory_budget);
                b.set_cap(
                    sizing.memory_budget,
                    &sizing.policy.reason,
                    sizing.policy.under_pressure,
                );
                b
            },
            memory: std::sync::Mutex::new(match &sizing.memory {
                Ok(m) => (Some(*m), None),
                Err(e) => (None, Some(e.clone())),
            }),
            expansion: AtomicU64::new(sizing.policy.expansion.to_bits()),
            footprint: AtomicU64::new(0),
            prepared_bytes: AtomicU64::new(0),
            disk: std::sync::Mutex::new(None),
            db_len: AtomicU64::new(0),
            db_len_start: AtomicU64::new(0),
            unchanged_bytes: AtomicU64::new(0),
            disk_projected: AtomicU64::new(0),
            disk_ratio: AtomicU64::new(crate::diskinfo::DISK_RATIO.to_bits()),
            disk_min_free: AtomicU64::new(0),
            disk_stop: std::sync::Mutex::new(None),
            disk_check: AtomicBool::new(true),
            disk_group_cap: AtomicU64::new(u64::MAX),
            start: Instant::now(),
            walk: Stage::new("walk", 1).traced(0, trace),
            parse: Stage::new("parse", t).traced(100, trace),
            commit: Stage::new("commit", 1).traced(1, trace),
            sizing,
            found: AtomicU64::new(0),
            found_bytes: AtomicU64::new(0),
            walk_done: AtomicBool::new(false),
            handled: AtomicU64::new(0),
            handled_bytes: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            txns: AtomicU64::new(0),
            last_txn: AtomicU64::new(0),
        }
    }

    pub fn view(&self) -> BoardView {
        // One lock, released before the literal below: a guard created
        // inside a struct expression lives to its end, so two `lock()`s
        // there would deadlock.
        let (memory, memory_error) = {
            let m = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            (m.0, m.1.clone())
        };
        BoardView {
            label: self.label.clone(),
            elapsed: self.start.elapsed(),
            stages: [self.walk.view(), self.parse.view(), self.commit.view()],
            found: self.found.load(Relaxed),
            found_bytes: self.found_bytes.load(Relaxed),
            walk_done: self.walk_done.load(std::sync::atomic::Ordering::Acquire),
            handled: self.handled.load(Relaxed),
            handled_bytes: self.handled_bytes.load(Relaxed),
            skipped: self.skipped.load(Relaxed),
            failed: self.failed.load(Relaxed),
            txns: self.txns.load(Relaxed),
            last_txn: Duration::from_millis(self.last_txn.load(Relaxed)),
            mem_used: self.budget.used(),
            mem_cap: self.budget.cap(),
            mem_peak: self.budget.peak(),
            budget: self.budget.view(),
            memory,
            memory_error,
            expansion: f64::from_bits(self.expansion.load(Relaxed)),
            footprint: self.footprint.load(Relaxed),
            disk: *self.disk.lock().unwrap_or_else(|e| e.into_inner()),
            db_len: self.db_len.load(Relaxed),
            disk_projected: self.disk_projected.load(Relaxed),
            disk_ratio: f64::from_bits(self.disk_ratio.load(Relaxed)),
            disk_min_free: self.disk_min_free.load(Relaxed),
            disk_stop: self
                .disk_stop
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            disk_check: self.disk_check.load(Relaxed),
            sizing: self.sizing.clone(),
        }
    }
}

/// A snapshot of the board, rendered without touching shared state.
#[derive(Debug, Clone)]
pub struct BoardView {
    pub label: String,
    pub elapsed: Duration,
    /// walk, parse, commit.
    pub stages: [StageView; 3],
    pub found: u64,
    pub found_bytes: u64,
    pub walk_done: bool,
    pub handled: u64,
    pub handled_bytes: u64,
    pub skipped: u64,
    pub failed: u64,
    pub txns: u64,
    pub last_txn: Duration,
    pub mem_used: u64,
    pub mem_cap: u64,
    pub mem_peak: u64,
    pub budget: BudgetView,
    pub memory: Option<MemSample>,
    pub memory_error: Option<String>,
    pub expansion: f64,
    pub footprint: u64,
    pub disk: Option<DiskSample>,
    pub db_len: u64,
    pub disk_projected: u64,
    pub disk_ratio: f64,
    pub disk_min_free: u64,
    pub disk_stop: Option<String>,
    pub disk_check: bool,
    pub sizing: Sizing,
}

fn mb(b: u64) -> String {
    let m = b as f64 / (1024.0 * 1024.0);
    if m >= 1024.0 {
        format!("{:.1} GB", m / 1024.0)
    } else {
        format!("{m:.0} MB")
    }
}

fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
    }
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn is_busy(a: &Activity) -> bool {
    matches!(a, Activity::Busy { .. })
}
fn is_blocked(a: &Activity) -> bool {
    matches!(a, Activity::Blocked { .. })
}
fn is_starved(a: &Activity) -> bool {
    matches!(a, Activity::Starved { .. })
}
fn is_done(a: &Activity) -> bool {
    matches!(a, Activity::Done)
}

impl BoardView {
    /// Which stage limits throughput right now, in words.
    pub fn bottleneck(&self) -> Option<String> {
        let [walk, parse, commit] = &self.stages;
        if parse.count(is_blocked) > 0 && commit.count(is_busy) > 0 {
            return Some("commit (parsed files wait for the database writer)".into());
        }
        if commit.count(is_starved) > 0 && parse.count(is_busy) > 0 {
            return Some("parse (the writer waits for parsed files)".into());
        }
        if !self.walk_done && walk.count(is_busy) > 0 && parse.count(is_starved) > 0 {
            return Some("walk (parsers wait for the directory walk)".into());
        }
        None
    }

    /// The lines of the live display, each at most `width` characters.
    pub fn render(&self, width: usize) -> Vec<String> {
        let [walk, parse, commit] = &self.stages;
        let mut lines = Vec::new();
        // Overall bar.
        let total = self.found.max(1);
        let frac = (self.handled as f64 / total as f64).min(1.0);
        let bar_w = 24;
        let filled = (frac * bar_w as f64) as usize;
        let bar: String = "=".repeat(filled) + &" ".repeat(bar_w - filled);
        let rate = self.handled_bytes as f64 / self.elapsed.as_secs_f64().max(0.001);
        let eta = if self.walk_done && self.handled > 0 && self.handled < self.found {
            let left = self.found_bytes.saturating_sub(self.handled_bytes) as f64;
            format!(
                "  ETA {}",
                secs(Duration::from_secs_f64(left / rate.max(1.0)))
            )
        } else {
            String::new()
        };
        let of = if self.walk_done {
            group(self.found)
        } else {
            format!("{}+", group(self.found))
        };
        lines.push(format!(
            "indexing {} [{bar}] {}/{of} files  {}/s  {}{eta}  skipped {} failed {}",
            self.label,
            group(self.handled),
            mb(rate as u64),
            secs(self.elapsed),
            self.skipped,
            self.failed,
        ));
        // walk
        lines.push(if self.walk_done {
            format!("  walk    ✓ done         {} files found", group(self.found))
        } else {
            let at = walk
                .threads
                .first()
                .and_then(|t| match &t.act {
                    Activity::Busy { what, .. } => Some(what.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            format!("  walk    ▶ walking      {} found  {at}", group(self.found))
        });
        // parse
        let n = parse.threads.len();
        let (b, bl, st, dn) = (
            parse.count(is_busy),
            parse.count(is_blocked),
            parse.count(is_starved),
            parse.count(is_done),
        );
        let state = if dn == n {
            "✓ done".to_string()
        } else if b > 0 {
            format!("▶ {b}/{n} busy")
        } else if bl > 0 {
            format!("⛔ {bl}/{n} blocked")
        } else {
            format!("⏸ {st}/{n} waiting")
        };
        let mut busy: Vec<_> = parse
            .threads
            .iter()
            .filter_map(|t| match &t.act {
                Activity::Busy { what, verb } => Some((t.elapsed, *verb, what.as_str())),
                _ => None,
            })
            .collect();
        busy.sort_by_key(|b| std::cmp::Reverse(b.0));
        let mut detail = String::new();
        for (el, verb, what) in busy.iter().take(4) {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            let v = if *verb == "parsing" { "" } else { " (reading)" };
            detail.push_str(&format!("{what}{v} {}", secs(*el)));
        }
        if busy.len() > 4 {
            detail.push_str(&format!(" · … +{}", busy.len() - 4));
        }
        lines.push(format!("  parse   {state:<13}  {detail}"));
        if dn < n && (bl > 0 || st > 0) {
            let why = if bl > 0 {
                format!(
                    "{bl} blocked: memory budget full ({} waiting for the writer)",
                    mb(self.mem_used)
                )
            } else if !self.walk_done {
                format!("{st} waiting for the walk")
            } else {
                format!("{st} idle: no files left to parse")
            };
            lines.push(format!("                         {why}"));
        }
        // commit
        let cline = match commit.threads.first().map(|t| (&t.act, t.elapsed)) {
            Some((Activity::Busy { what, .. }, el)) => format!("▶ {what}, {}", secs(el)),
            Some((Activity::Starved { on }, el)) if !on.is_empty() => {
                format!("⏸ waiting {} for {on}", secs(el))
            }
            Some((Activity::Done, _)) => "✓ done".to_string(),
            _ => "⏸ waiting for parsed files".to_string(),
        };
        lines.push(format!(
            "  commit  {cline}   ({} txns, {} files stored, last txn {})",
            self.txns,
            group(commit.items),
            secs(self.last_txn)
        ));
        // memory + bottleneck
        let bn = self
            .bottleneck()
            .map_or(String::new(), |b| format!("   bottleneck: {b}"));
        let why = if self.budget.pressure {
            format!("⚠ {}", self.budget.reason)
        } else if self.budget.reason.is_empty() {
            String::new()
        } else {
            format!("({})", self.budget.reason)
        };
        lines.push(format!(
            "  memory  in flight {} (≈{} in memory) · budget {} (≈{}) {why}{bn}",
            mb(self.mem_used),
            mb(self.footprint),
            mb(self.mem_cap),
            mb((self.mem_cap as f64 * self.expansion) as u64)
        ));
        lines.push(self.disk_line());
        lines
            .into_iter()
            .map(|l| {
                if l.chars().count() > width {
                    l.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
                } else {
                    l
                }
            })
            .collect()
    }

    /// The `disk` line of the live display.
    fn disk_line(&self) -> String {
        let mut l = match &self.disk {
            Some(d) => format!(
                "  disk    db {} · free {} of {} · projected {} (≈{:.1}x source) · stops below {}",
                mb(self.db_len),
                mb(d.available),
                mb(d.total),
                mb(self.disk_projected),
                self.disk_ratio,
                mb(self.disk_min_free)
            ),
            None => format!(
                "  disk    db {} · free space unknown on this platform",
                mb(self.db_len)
            ),
        };
        if !self.disk_check {
            l.push_str(" (check off)");
        }
        if let Some(why) = &self.disk_stop {
            l.push_str(&format!(" ⚠ stopping: {why}"));
        }
        l
    }

    /// The end-of-run table (`--stats`).
    pub fn stats_table(&self) -> String {
        let wall = self.elapsed.as_secs_f64().max(0.001);
        let mut s = String::from(
            "stage   threads  busy%  waiting-upstream%  blocked-downstream%    items    MB/s\n",
        );
        for st in &self.stages {
            let cap = wall * st.threads.len() as f64;
            let pct = |d: Duration| 100.0 * d.as_secs_f64() / cap;
            s.push_str(&format!(
                "{:<7} {:>7}  {:>5.1}  {:>17.1}  {:>19.1}  {:>7}  {:>6.1}\n",
                st.name,
                st.threads.len(),
                pct(st.busy),
                pct(st.starved),
                pct(st.blocked),
                st.items,
                st.bytes as f64 / (1024.0 * 1024.0) / wall,
            ));
        }
        let parse = &self.stages[1];
        let writer = 100.0 * self.stages[2].busy.as_secs_f64() / wall;
        let p = 100.0 * parse.busy.as_secs_f64() / (wall * parse.threads.len() as f64);
        let verdict = if writer > 75.0 {
            format!("writer-bound: the database writer was busy {writer:.0}% of the run (parse threads {p:.0}%)")
        } else if p > 75.0 {
            format!("parse-bound: parse threads were busy {p:.0}% (writer {writer:.0}%)")
        } else {
            format!("parse threads busy {p:.0}%, writer busy {writer:.0}%")
        };
        let b = &self.budget;
        s.push_str(&format!(
            "sizing: {} parse threads ({} CPUs); memory budget {} at start, {}..{} of source during the run (about {} in memory at the end; {}), peak in flight {}; {} transactions\n",
            self.sizing.parse_threads,
            self.sizing.cpus,
            mb(self.sizing.memory_budget),
            mb(b.cap_min),
            mb(b.cap_max),
            mb((b.cap as f64 * self.expansion) as u64),
            b.reason,
            mb(self.mem_peak),
            self.txns,
        ));
        if let Some(m) = &self.memory {
            s.push_str(&format!(
                "memory: {} of {} free now{}; growth {:.1}x per source byte in flight; pressure {} time(s), {} in total  [{}]{}\n",
                mb(m.available),
                mb(m.total),
                m.rss
                    .map_or(String::new(), |r| format!(", this process {}", mb(r))),
                self.expansion,
                b.pressure_episodes,
                secs(b.pressure_time),
                m.source,
                self.memory_error
                    .as_deref()
                    .map_or(String::new(), |e| format!("; last probe failed: {e}")),
            ));
        } else {
            s.push_str(&format!(
                "memory: free RAM unknown ({}); assuming a fixed budget\n",
                self.memory_error.as_deref().unwrap_or("no reading")
            ));
        }
        s.push_str(&format!(
            "disk: db {}{}, projected {} ({:.1}x source), reserve {}{}{}\n",
            mb(self.db_len),
            self.disk.map_or(String::new(), |d| format!(
                ", {} free of {}",
                mb(d.available),
                mb(d.total)
            )),
            mb(self.disk_projected),
            self.disk_ratio,
            mb(self.disk_min_free),
            if self.disk_check { "" } else { " (check off)" },
            self.disk_stop
                .as_deref()
                .map_or(String::new(), |w| format!("; stopped: {w}")),
        ));
        s.push_str(&format!("{verdict}\n"));
        s
    }

    /// The same counters for `--json --stats`.
    pub fn stats_json(&self) -> serde_json::Value {
        let stages: Vec<_> = self
            .stages
            .iter()
            .map(|st| {
                serde_json::json!({
                    "stage": st.name, "threads": st.threads.len(),
                    "busy_ms": st.busy.as_millis() as u64,
                    "waiting_upstream_ms": st.starved.as_millis() as u64,
                    "blocked_downstream_ms": st.blocked.as_millis() as u64,
                    "items": st.items, "bytes": st.bytes,
                })
            })
            .collect();
        serde_json::json!({
            "stages": stages,
            "parse_threads": self.sizing.parse_threads,
            "cpus": self.sizing.cpus,
            "memory_budget": self.mem_cap,
            "peak_in_flight": self.mem_peak,
            "transactions": self.txns,
            "memory": {
                "budget_start": self.sizing.memory_budget,
                "budget_min": self.budget.cap_min,
                "budget_max": self.budget.cap_max,
                "budget_end": self.budget.cap,
                "reason": self.budget.reason,
                "pressure_episodes": self.budget.pressure_episodes,
                "pressure_ms": self.budget.pressure_time.as_millis() as u64,
                "total": self.memory.map(|m| m.total),
                "available": self.memory.map(|m| m.available),
                "rss": self.memory.and_then(|m| m.rss),
                "source": self.memory.map(|m| m.source),
                "error": self.memory_error,
                "expansion": self.expansion,
                "footprint_in_flight": self.footprint,
                "budget_end_in_memory": (self.budget.cap as f64 * self.expansion) as u64,
            },
            "disk": {
                "db_bytes": self.db_len,
                "total": self.disk.map(|d| d.total),
                "available": self.disk.map(|d| d.available),
                "projected_bytes": self.disk_projected,
                "ratio": self.disk_ratio,
                "min_free": self.disk_min_free,
                "check": self.disk_check,
                "stopped": self.disk_stop,
            },
        })
    }
}

/// Draws board views on stderr (or nowhere).
pub struct Display {
    multi: MultiProgress,
    lines: Vec<ProgressBar>,
}

impl Display {
    pub fn stderr() -> Self {
        Self::new(ProgressDrawTarget::stderr())
    }
    pub fn hidden() -> Self {
        Self::new(ProgressDrawTarget::hidden())
    }
    fn new(target: ProgressDrawTarget) -> Self {
        Self {
            multi: MultiProgress::with_draw_target(target),
            lines: Vec::new(),
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.multi.is_hidden()
    }

    /// Redraw from `v`.
    pub fn draw(&mut self, v: &BoardView) {
        let width = console::Term::stderr()
            .size_checked()
            .map_or(120, |(_, w)| w as usize);
        let text = v.render(width);
        while self.lines.len() < text.len() {
            let b = self.multi.add(ProgressBar::new_spinner());
            b.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
            self.lines.push(b);
        }
        for (i, b) in self.lines.iter().enumerate() {
            b.set_message(text.get(i).cloned().unwrap_or_default());
        }
    }

    /// Remove the display from the terminal.
    pub fn finish(&mut self) {
        for l in self.lines.drain(..) {
            l.finish_and_clear();
        }
        let _ = self.multi.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataflow::ThreadView;

    fn view() -> BoardView {
        let t = |act, ms| ThreadView {
            act,
            elapsed: Duration::from_millis(ms),
        };
        let busy = |w: &str| Activity::Busy {
            verb: "parsing",
            what: w.into(),
        };
        let st = |name, threads| StageView {
            name,
            threads,
            ..StageView::default()
        };
        BoardView {
            label: "o/r".into(),
            elapsed: Duration::from_secs(10),
            stages: [
                st("walk", vec![t(Activity::Done, 0)]),
                st(
                    "parse",
                    vec![
                        t(busy("a.cs"), 400),
                        t(busy("big.aspx"), 3100),
                        t(
                            Activity::Blocked {
                                on: "memory".into(),
                            },
                            50,
                        ),
                    ],
                ),
                st(
                    "commit",
                    vec![t(
                        Activity::Busy {
                            verb: "committing",
                            what: "txn 18: 1,412 files".into(),
                        },
                        1900,
                    )],
                ),
            ],
            found: 13204,
            found_bytes: 100 << 20,
            walk_done: true,
            handled: 6120,
            handled_bytes: 40 << 20,
            skipped: 3,
            failed: 0,
            txns: 17,
            last_txn: Duration::from_millis(2200),
            mem_used: 84 << 20,
            mem_cap: 1 << 30,
            mem_peak: 90 << 20,
            budget: BudgetView {
                cap: 1 << 30,
                used: 84 << 20,
                peak: 90 << 20,
                cap_min: 1 << 30,
                cap_max: 2 << 30,
                reason: "25% of 8.0 GB free above the 3.2 GB OS reserve".into(),
                pressure: false,
                pressure_episodes: 0,
                pressure_time: Duration::ZERO,
            },
            memory: Some(MemSample {
                total: 16 << 30,
                available: 8 << 30,
                rss: Some(1 << 30),
                psi_some_avg10: None,
                source: "test probe",
            }),
            memory_error: None,
            expansion: 6.0,
            footprint: 500 << 20,
            disk: Some(DiskSample {
                total: 931 << 30,
                available: 41 << 30,
            }),
            db_len: 3200 << 20,
            disk_projected: 12100 << 20,
            disk_ratio: 10.0,
            disk_min_free: 2 << 30,
            disk_stop: None,
            disk_check: true,
            sizing: Sizing::new(
                4,
                Ok(MemSample {
                    total: 16 << 30,
                    available: 8 << 30,
                    rss: None,
                    psi_some_avg10: None,
                    source: "test probe",
                }),
                3,
                None,
                1,
            ),
        }
    }

    #[test]
    fn memory_line_says_why() {
        let mut v = view();
        let t = v.render(200).join("\n");
        assert!(
            t.contains(
                "in flight 84 MB (≈500 MB in memory) · budget 1.0 GB (≈6.0 GB) (25% of 8.0 GB free"
            ),
            "{t}"
        );
        v.budget.pressure = true;
        v.budget.reason = "pressure: only 1.0 GB of 16.0 GB free".into();
        let t = v.render(200).join("\n");
        assert!(
            t.contains("budget 1.0 GB (≈6.0 GB) ⚠ pressure: only"),
            "{t}"
        );
        let s = v.stats_table();
        assert!(
            s.contains("1.0 GB..2.0 GB of source during the run (about 6.0 GB in memory"),
            "{s}"
        );
        assert!(
            s.contains("8.0 GB of 16.0 GB free now, this process 1.0 GB"),
            "{s}"
        );
        assert!(s.contains("in total  [test probe]\n"), "{s}");
        let j = v.stats_json();
        assert_eq!(j["memory"]["budget_max"], 2u64 << 30);
        assert_eq!(j["memory"]["total"], 16u64 << 30);
        assert_eq!(j["memory"]["source"], "test probe");
        assert_eq!(j["memory"]["error"], serde_json::Value::Null);
        // A probe that failed once keeps the last sample and says so; one
        // that never worked says why.
        v.memory_error = Some("/proc/meminfo: Permission denied".into());
        let s = v.stats_table();
        assert!(
            s.contains("[test probe]; last probe failed: /proc/meminfo: Permission denied"),
            "{s}"
        );
        v.memory = None;
        let s = v.stats_table();
        assert!(
            s.contains("memory: free RAM unknown (/proc/meminfo: Permission denied); assuming"),
            "{s}"
        );
        let j = v.stats_json();
        assert_eq!(j["memory"]["source"], serde_json::Value::Null);
        assert_eq!(j["memory"]["error"], "/proc/meminfo: Permission denied");
    }

    #[test]
    fn render_says_what_each_stage_does() {
        let v = view();
        let text = v.render(200).join("\n");
        assert!(text.contains("6,120/13,204 files"), "{text}");
        assert!(text.contains("walk    ✓ done"), "{text}");
        assert!(text.contains("▶ 2/3 busy"), "{text}");
        // Longest-running file first, so a stuck one stands out.
        assert!(
            text.find("big.aspx").unwrap() < text.find("a.cs").unwrap(),
            "{text}"
        );
        assert!(
            text.contains("1 blocked: memory budget full (84 MB"),
            "{text}"
        );
        assert!(text.contains("▶ txn 18: 1,412 files, 1.9s"), "{text}");
        assert!(text.contains("bottleneck: commit"), "{text}");
        assert!(text.contains("ETA"), "{text}");
        for l in v.render(40) {
            assert!(l.chars().count() <= 40, "{l}");
        }
    }

    #[test]
    fn bottleneck_and_stats() {
        let mut v = view();
        v.stages[2].threads[0].act = Activity::Starved {
            on: "big.aspx".into(),
        };
        v.stages[1].threads[2].act = Activity::Busy {
            verb: "parsing",
            what: "c.js".into(),
        };
        assert!(v.bottleneck().unwrap().starts_with("parse"));
        assert!(v
            .render(200)
            .join("\n")
            .contains("waiting 1.9s for big.aspx"));
        v.stages[2].busy = Duration::from_secs(9);
        let t = v.stats_table();
        assert!(t.contains("writer-bound"), "{t}");
        assert_eq!(v.stats_json()["stages"][2]["busy_ms"], 9000);
    }

    #[test]
    fn disk_line_and_stats() {
        let mut v = view();
        let t = v.render(200).join("\n");
        assert!(
            t.contains("disk    db 3.1 GB · free 41.0 GB of 931.0 GB · projected 11.8 GB (≈10.0x source) · stops below 2.0 GB"),
            "{t}"
        );
        v.disk_stop = Some("only 1.0 GB free".into());
        assert!(v
            .render(200)
            .join("\n")
            .contains("⚠ stopping: only 1.0 GB free"));
        let s = v.stats_table();
        assert!(s.contains("disk: db 3.1 GB, 41.0 GB free of 931.0 GB, projected 11.8 GB (10.0x source), reserve 2.0 GB; stopped: only"), "{s}");
        assert_eq!(v.stats_json()["disk"]["projected_bytes"], 12100u64 << 20);
        v.disk = None;
        v.disk_check = false;
        let t = v.render(200).join("\n");
        assert!(
            t.contains("free space unknown on this platform (check off)"),
            "{t}"
        );
    }

    #[test]
    fn hidden_display_draws_nothing() {
        let mut d = Display::hidden();
        assert!(d.is_hidden());
        d.draw(&view());
        d.finish();
    }
}
