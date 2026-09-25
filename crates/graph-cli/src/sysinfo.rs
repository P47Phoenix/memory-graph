//! Sizing the index pipeline from the machine it runs on: thread count from
//! the CPUs, in-flight memory from the RAM that is free right now, and kept
//! in step with it while the run goes on (see [`MemoryPolicy`]).

const MIB: u64 = 1024 * 1024;
/// Used when the platform does not report memory at all.
const FALLBACK_BUDGET: u64 = 512 * MIB;
/// A fraction budget never goes below this (a fixed `--memory` may).
pub const FLOOR: u64 = 256 * MIB;
/// Under pressure the budget stops growing but keeps this much, so files
/// still commit in reasonable groups rather than one at a time.
pub const PRESSURE_FLOOR: u64 = 64 * MIB;
/// Share of physical RAM kept free for the operating system and everything
/// else: the budget only ever targets what is free above it, and dropping
/// below it is memory pressure.
pub const OS_RESERVE: f64 = 0.20;
/// Default share of the free memory (above the reserve) to budget.
pub const DEFAULT_FRACTION: f64 = 0.25;

/// One reading of the machine's memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemSample {
    /// Physical RAM.
    pub total: u64,
    /// Free for new allocations right now (page cache counted as free).
    pub available: u64,
    /// This process's resident set, if the platform reports it.
    pub rss: Option<u64>,
    /// Linux PSI `some avg10` for memory (percent of time some task
    /// stalled on memory), if present.
    pub psi_some_avg10: Option<f64>,
}

/// Read the machine's memory, if the platform says.
pub fn sample_memory() -> Option<MemSample> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb = |key: &str| -> Option<u64> {
            let line = s.lines().find(|l| l.starts_with(key))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        };
        let total = kb("MemTotal:")? * 1024;
        let available = kb("MemAvailable:")? * 1024;
        // VmRSS is in kB whatever the page size (statm counts pages).
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
                line.split_whitespace().nth(1)?.parse::<u64>().ok()
            })
            .map(|kb| kb * 1024);
        let psi_some_avg10 = std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("some"))?;
                let f = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
                f["avg10=".len()..].parse::<f64>().ok()
            });
        Some(MemSample {
            total,
            available,
            rss,
            psi_some_avg10,
        })
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: both structs are plain data; their length fields are set as
        // the APIs require before the calls, and return values are checked.
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut m) == 0 {
                return None;
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let rss = (K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0)
                .then_some(pmc.WorkingSetSize as u64);
            Some(MemSample {
                total: m.ullTotalPhys,
                available: m.ullAvailPhys,
                rss,
                psi_some_avg10: None,
            })
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// Bytes of memory available to new allocations, if the platform says.
pub fn available_memory() -> Option<u64> {
    sample_memory().map(|m| m.available)
}

/// What `--memory` asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemorySpec {
    /// A fixed budget (`2G`): never re-sampled.
    Fixed(u64),
    /// A share of the free memory above the OS reserve (`25%`), followed
    /// during the run.
    Fraction(f64),
}

/// Parse `--memory`: a size like `512M` / `2G` / `1048576` (binary units),
/// or a percentage of free memory like `25%`. Empty (an unset-looking
/// `MEMORY_GRAPH_MEMORY=`) means the default.
pub fn parse_memory_spec(s: &str) -> Result<MemorySpec, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(MemorySpec::Fraction(DEFAULT_FRACTION));
    }
    if let Some(p) = t.strip_suffix('%') {
        let n: f64 = p
            .trim()
            .parse()
            .map_err(|_| format!("invalid percentage `{s}`"))?;
        if !(n > 0.0 && n <= 100.0) {
            return Err(format!("percentage `{s}` must be above 0 and at most 100"));
        }
        return Ok(MemorySpec::Fraction(n / 100.0));
    }
    parse_size(t).map(MemorySpec::Fixed)
}

/// Parse a size like `512M`, `2G`, `1048576` (binary units).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => {
            let unit = s[i..].to_ascii_uppercase();
            let mul = match unit.trim_end_matches("IB").trim_end_matches('B') {
                "" => 1,
                "K" => 1024,
                "M" => MIB,
                "G" => 1024 * MIB,
                _ => return Err(format!("unknown size unit in `{s}` (use K, M or G)")),
            };
            (&s[..i], mul)
        }
        None => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size `{s}`"))?;
    match n.checked_mul(mul) {
        Some(0) | None => Err(format!("size `{s}` out of range")),
        Some(v) => Ok(v),
    }
}

/// How the budget follows the machine: computed from each [`MemSample`],
/// with hysteresis so the line does not flicker. Pure, so tests can script
/// samples.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryPolicy {
    pub spec: MemorySpec,
    /// Lowest budget ever set (deterministic mode raises it).
    pub floor: u64,
    /// Whether the last sample put us under pressure (hysteresis state).
    pub under_pressure: bool,
    /// Why the current cap is what it is, for the display.
    pub reason: String,
    pub cap: u64,
}

/// A change the policy decided on.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub cap: u64,
    pub under_pressure: bool,
    pub reason: String,
}

/// A share as a percentage, with a decimal only when it needs one.
fn pct(f: f64) -> String {
    let p = f * 100.0;
    if (p - p.round()).abs() < 0.05 {
        format!("{p:.0}")
    } else {
        format!("{p:.1}")
    }
}

fn mb(b: u64) -> String {
    let m = b as f64 / MIB as f64;
    if m >= 1024.0 {
        format!("{:.1} GB", m / 1024.0)
    } else {
        format!("{m:.0} MB")
    }
}

impl MemoryPolicy {
    /// Start from the first sample (or none).
    pub fn new(spec: MemorySpec, floor: u64, first: Option<&MemSample>) -> Self {
        let mut p = Self {
            spec,
            floor: floor.max(1),
            under_pressure: false,
            reason: String::new(),
            cap: 1,
        };
        let d = p.decide(first, 0);
        p.apply(&d);
        p
    }

    fn apply(&mut self, d: &Decision) {
        self.cap = d.cap;
        self.under_pressure = d.under_pressure;
        self.reason.clone_from(&d.reason);
    }

    /// Feed a sample; returns the new cap when it changed enough to matter.
    pub fn update(&mut self, sample: Option<&MemSample>, held: u64) -> Option<Decision> {
        let d = self.decide(sample, held);
        let moved = d.under_pressure != self.under_pressure
            || (d.cap as f64 - self.cap as f64).abs() > 0.10 * self.cap as f64;
        if !moved {
            return None;
        }
        self.apply(&d);
        Some(d)
    }

    /// The cap for `sample`, given `held` bytes we hold right now.
    fn decide(&self, sample: Option<&MemSample>, held: u64) -> Decision {
        let fraction = match self.spec {
            MemorySpec::Fixed(bytes) => {
                return Decision {
                    cap: bytes.max(self.floor),
                    under_pressure: false,
                    reason: "fixed by --memory".into(),
                }
            }
            MemorySpec::Fraction(f) => f,
        };
        // A reading without a total is no reading (it would pin us under
        // pressure through the RSS check).
        let Some(m) = sample.filter(|m| m.total > 0) else {
            return Decision {
                cap: FALLBACK_BUDGET.max(self.floor),
                under_pressure: false,
                reason: "free RAM unknown on this platform".into(),
            };
        };
        let reserve = (m.total as f64 * OS_RESERVE) as u64;
        // Hysteresis: pressure starts below the reserve and ends only once
        // free memory is comfortably above it again.
        // (pressure starts below 20% free and ends only above 30%).
        let leave = reserve + reserve / 2;
        let low = m.available < reserve;
        let rss_high = m.rss.is_some_and(|r| r as f64 > 0.60 * m.total as f64);
        let stalled = m.psi_some_avg10.is_some_and(|p| p > 10.0);
        let pressure = if self.under_pressure {
            m.available < leave || rss_high || stalled
        } else {
            low || rss_high || stalled
        };
        if pressure {
            let why = if stalled {
                "the system is stalling on memory".to_string()
            } else if rss_high {
                format!(
                    "this process uses {} of {}",
                    mb(m.rss.unwrap_or(0)),
                    mb(m.total)
                )
            } else {
                format!(
                    "only {} of {} free (keeping {} for the OS)",
                    mb(m.available),
                    mb(m.total),
                    mb(reserve)
                )
            };
            // Each sample halves what is held, so the cap ratchets down to
            // the pressure floor and stays there until free memory is back
            // above 30% of RAM.
            return Decision {
                cap: (held / 2).max(self.floor).max(PRESSURE_FLOOR),
                under_pressure: true,
                reason: format!("pressure: {why}; budget held down until 30% is free"),
            };
        }
        // What we hold is part of what is no longer "available": add it back
        // so the budget does not shrink itself as it fills.
        let spare = (m.available + held).saturating_sub(reserve);
        let target = (spare as f64 * fraction) as u64;
        let ceiling = m.total / 2;
        let floor = self.floor.max(FLOOR);
        let cap = target.clamp(floor, ceiling.max(floor));
        Decision {
            cap,
            under_pressure: false,
            reason: format!(
                "{}% of {} free above the {} OS reserve{}",
                pct(fraction),
                mb(m.available),
                mb(reserve),
                m.rss.map_or(String::new(), |r| format!("; RSS {}", mb(r)))
            ),
        }
    }
}

/// How the pipeline is sized, and why (printed by `--stats`).
#[derive(Debug, Clone, PartialEq)]
pub struct Sizing {
    /// Threads that read and parse files.
    pub parse_threads: usize,
    /// The initial cap on source bytes read but not yet committed.
    pub memory_budget: u64,
    pub cpus: usize,
    pub memory: Option<MemSample>,
    pub policy: MemoryPolicy,
}

impl Sizing {
    /// Parse threads: every CPU but one (the committing thread keeps one
    /// busy), at least one. Budget: from `spec` (default: a quarter of the
    /// free memory above the 20% kept for the OS, re-sampled during the run;
    /// see [`MemoryPolicy`]). `floor` is the least budget ever set.
    pub fn new(
        cpus: usize,
        memory: Option<MemSample>,
        jobs: usize,
        spec: Option<MemorySpec>,
        floor: u64,
    ) -> Self {
        let parse_threads = if jobs > 0 {
            jobs
        } else {
            cpus.saturating_sub(1).max(1)
        };
        let spec = spec.unwrap_or(MemorySpec::Fraction(DEFAULT_FRACTION));
        let policy = MemoryPolicy::new(spec, floor, memory.as_ref());
        Self {
            parse_threads,
            memory_budget: policy.cap,
            cpus,
            memory,
            policy,
        }
    }

    /// Size for this machine.
    pub fn detect(jobs: usize, spec: Option<MemorySpec>, floor: u64) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        Self::new(cpus, sample_memory(), jobs, spec, floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(total_gb: u64, avail_gb: u64) -> MemSample {
        MemSample {
            total: total_gb << 30,
            available: avail_gb << 30,
            rss: None,
            psi_some_avg10: None,
        }
    }

    #[test]
    fn sizing_follows_the_hardware() {
        let s = Sizing::new(32, Some(mem(64, 60)), 0, None, FLOOR);
        assert_eq!(s.parse_threads, 31);
        // 25% of (60 - 12.8 reserve) GB.
        assert_eq!(s.memory_budget, ((60u64 << 30) - (64u64 << 30) / 5) / 4);
        let s = Sizing::new(1, Some(mem(2, 1)), 0, None, 1);
        assert_eq!(s.parse_threads, 1);
        assert_eq!(s.memory_budget, FLOOR, "fraction floor");
        let s = Sizing::new(8, None, 0, None, FLOOR);
        assert_eq!(s.memory_budget, FALLBACK_BUDGET);
        assert!(s.policy.reason.contains("unknown"));
        let s = Sizing::new(
            8,
            Some(mem(64, 60)),
            3,
            Some(MemorySpec::Fixed(10 * MIB)),
            1,
        );
        assert_eq!((s.parse_threads, s.memory_budget), (3, 10 * MIB));
        // A fixed budget never goes below the floor.
        let s = Sizing::new(
            8,
            Some(mem(64, 60)),
            3,
            Some(MemorySpec::Fixed(10 * MIB)),
            FLOOR,
        );
        assert_eq!(s.memory_budget, FLOOR);
        // The ceiling is half of RAM.
        let s = Sizing::new(
            8,
            Some(mem(16, 16)),
            0,
            Some(MemorySpec::Fraction(1.0)),
            FLOOR,
        );
        assert_eq!(s.memory_budget, 8 << 30);
    }

    #[test]
    fn policy_follows_free_memory_and_backs_off_under_pressure() {
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Some(&mem(16, 8)));
        let reserve = (16u64 << 30) / 5;
        assert_eq!(p.cap, ((8u64 << 30) - reserve) / 4);
        assert!(!p.under_pressure);
        // A wobble under the 10% deadband is not reported; a bigger move is.
        assert!(p.update(Some(&mem(16, 8)), 0).is_none());
        let cap0 = p.cap;
        let mut wobble = mem(16, 8);
        wobble.available += cap0 / 4; // target moves by 5% (a quarter of the 25% share)
        assert!(p.update(Some(&wobble), 0).is_none());
        wobble.available = (8u64 << 30) + cap0 * 3 / 5; // ~15%
        assert!(p.update(Some(&wobble), 0).is_some());
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Some(&mem(16, 8)));
        // Plenty more free: the cap grows.
        let d = p.update(Some(&mem(16, 12)), 0).unwrap();
        assert!(d.cap > ((8u64 << 30) - reserve) / 4);
        // What we hold is added back, so filling the budget does not shrink it.
        let held = 1u64 << 30;
        let d = p.update(Some(&mem(16, 11)), held);
        assert!(d.is_none(), "{d:?}");
        // Free memory falls below the 20% reserve: pressure, cap = held/2.
        let d = p.update(Some(&mem(16, 3)), 2 << 30).unwrap();
        assert!(d.under_pressure);
        assert_eq!(d.cap, 1 << 30);
        assert!(d.reason.contains("pressure") && d.reason.contains("for the OS"));
        // Slightly above the reserve is not enough to leave (hysteresis).
        assert!(p.update(Some(&mem(16, 3)), 2 << 30).is_none());
        let d = p.update(Some(&mem(16, 4)), 2 << 30);
        assert!(d.is_none() || d.as_ref().unwrap().under_pressure);
        // Comfortably above it: back to the fraction target.
        let d = p.update(Some(&mem(16, 6)), 2 << 30).unwrap();
        assert!(!d.under_pressure);
        assert!(d.cap >= FLOOR);
        // Pressure never goes below the hard floor.
        let d = p.update(Some(&mem(16, 1)), 0).unwrap();
        assert_eq!(d.cap, FLOOR);
        let mut q = MemoryPolicy::new(MemorySpec::Fraction(0.25), 1, Some(&mem(16, 10)));
        assert_eq!(q.update(Some(&mem(16, 1)), 0).unwrap().cap, 64 << 20);
        // Our own growth counts too.
        let mut big = mem(16, 10);
        big.rss = Some(11 << 30);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Some(&mem(16, 10)));
        let d = p.update(Some(&big), 0).unwrap();
        assert!(d.under_pressure && d.reason.contains("this process"));
        // And a Linux PSI stall.
        let mut stall = mem(16, 10);
        stall.psi_some_avg10 = Some(25.0);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Some(&mem(16, 10)));
        assert!(p
            .update(Some(&stall), 0)
            .unwrap()
            .reason
            .contains("stalling"));
        // A bogus total is treated as no reading, not as pressure.
        let mut bogus = mem(0, 0);
        bogus.rss = Some(1 << 20);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), 1, Some(&bogus));
        assert!(!p.under_pressure && p.reason.contains("unknown"));
        assert!(p.update(Some(&bogus), 0).is_none());
        // Fixed never moves.
        let mut p = MemoryPolicy::new(MemorySpec::Fixed(1 << 30), 1, Some(&mem(16, 10)));
        assert!(p.update(Some(&mem(16, 1)), 5 << 30).is_none());
        assert_eq!(p.cap, 1 << 30);
    }

    #[test]
    fn memory_specs_parse() {
        assert_eq!(parse_memory_spec("512M"), Ok(MemorySpec::Fixed(512 * MIB)));
        assert_eq!(parse_memory_spec("25%"), Ok(MemorySpec::Fraction(0.25)));
        assert_eq!(parse_memory_spec(" 100 % "), Ok(MemorySpec::Fraction(1.0)));
        assert_eq!(parse_memory_spec("0.5%"), Ok(MemorySpec::Fraction(0.005)));
        assert_eq!(
            parse_memory_spec(""),
            Ok(MemorySpec::Fraction(DEFAULT_FRACTION))
        );
        assert_eq!(pct(0.25), "25");
        assert_eq!(pct(0.005), "0.5");
        assert!(parse_memory_spec("0%").is_err());
        assert!(parse_memory_spec("150%").is_err());
        assert!(parse_memory_spec("x%").is_err());
        assert_eq!(parse_size("2g"), Ok(2048 * MIB));
        assert_eq!(parse_size("64KiB"), Ok(64 * 1024));
        assert_eq!(parse_size("1000"), Ok(1000));
        assert!(parse_size("0").is_err());
        assert!(parse_size("5T").is_err());
        assert!(parse_size("x").is_err());
    }

    #[test]
    fn memory_is_sampled_here() {
        if cfg!(any(target_os = "linux", windows)) {
            let m = sample_memory().unwrap();
            assert!(m.total >= m.available && m.available > 0);
            assert!(m.rss.unwrap() > 0);
        }
    }
}
