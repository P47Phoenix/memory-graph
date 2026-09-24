//! Live view of `memory-graph index`: one line per pipeline stage saying what
//! it is working on, or what it is waiting for and why, plus the overall bar,
//! the bottleneck and memory in flight. Drawn on stderr (stdout keeps only the
//! final summary); `--stats` prints the same counters as a table at the end.
use crate::dataflow::{Activity, Budget, Stage, StageView, Trace};
use crate::sysinfo::Sizing;
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
            budget: Budget::new(sizing.memory_budget),
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
        BoardView {
            label: self.label.clone(),
            elapsed: self.start.elapsed(),
            stages: [self.walk.view(), self.parse.view(), self.commit.view()],
            found: self.found.load(Relaxed),
            found_bytes: self.found_bytes.load(Relaxed),
            walk_done: self.walk_done.load(Relaxed),
            handled: self.handled.load(Relaxed),
            handled_bytes: self.handled_bytes.load(Relaxed),
            skipped: self.skipped.load(Relaxed),
            failed: self.failed.load(Relaxed),
            txns: self.txns.load(Relaxed),
            last_txn: Duration::from_millis(self.last_txn.load(Relaxed)),
            mem_used: self.budget.used(),
            mem_cap: self.budget.cap(),
            mem_peak: self.budget.peak(),
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
        lines.push(format!(
            "  memory  {} / {} budget{bn}",
            mb(self.mem_used),
            mb(self.mem_cap)
        ));
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
        s.push_str(&format!(
            "sizing: {} parse threads ({} CPUs), memory budget {} ({} free), peak in flight {}; {} transactions\n{verdict}\n",
            self.sizing.parse_threads,
            self.sizing.cpus,
            mb(self.mem_cap),
            self.sizing.available_memory.map_or("unknown".into(), mb),
            mb(self.mem_peak),
            self.txns,
        ));
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
            sizing: Sizing::new(4, Some(8 << 30), 3, None),
        }
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
    fn hidden_display_draws_nothing() {
        let mut d = Display::hidden();
        assert!(d.is_hidden());
        d.draw(&view());
        d.finish();
    }
}
