//! Sizing the index pipeline from the machine it runs on: thread count from
//! the CPUs, in-flight memory from the RAM that is free right now.

/// Bytes of memory available to new allocations, if the platform says.
pub fn available_memory() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: MEMORYSTATUSEX is plain data; dwLength is set as the API
        // requires before the call.
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            (GlobalMemoryStatusEx(&mut m) != 0).then_some(m.ullAvailPhys)
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// How the pipeline is sized, and why (printed by `--stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sizing {
    /// Threads that read and parse files.
    pub parse_threads: usize,
    /// Cap on source bytes read but not yet committed.
    pub memory_budget: u64,
    pub cpus: usize,
    pub available_memory: Option<u64>,
}

const MIB: u64 = 1024 * 1024;
/// Used when the platform does not report free memory.
const FALLBACK_BUDGET: u64 = 512 * MIB;

impl Sizing {
    /// Parse threads: every CPU but one (the committing thread keeps one
    /// busy), at least one. Budget: an eighth of free memory, between 256 MiB
    /// and 4 GiB (parsed files take several times their source size, and the
    /// database's own cache needs room too). `jobs` / `memory` override.
    pub fn new(cpus: usize, available: Option<u64>, jobs: usize, memory: Option<u64>) -> Self {
        let parse_threads = if jobs > 0 {
            jobs
        } else {
            cpus.saturating_sub(1).max(1)
        };
        let memory_budget = memory.unwrap_or_else(|| {
            available.map_or(FALLBACK_BUDGET, |a| (a / 8).clamp(256 * MIB, 4096 * MIB))
        });
        Self {
            parse_threads,
            memory_budget: memory_budget.max(1),
            cpus,
            available_memory: available,
        }
    }

    /// Size for this machine.
    pub fn detect(jobs: usize, memory: Option<u64>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        Self::new(cpus, available_memory(), jobs, memory)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_follows_the_hardware() {
        let s = Sizing::new(32, Some(64 * 1024 * MIB), 0, None);
        assert_eq!(s.parse_threads, 31);
        assert_eq!(s.memory_budget, 4096 * MIB, "capped");
        let s = Sizing::new(1, Some(512 * MIB), 0, None);
        assert_eq!(s.parse_threads, 1);
        assert_eq!(s.memory_budget, 256 * MIB, "floor");
        let s = Sizing::new(8, Some(8 * 1024 * MIB), 0, None);
        assert_eq!(s.memory_budget, 1024 * MIB);
        assert_eq!(Sizing::new(8, None, 0, None).memory_budget, FALLBACK_BUDGET);
        let s = Sizing::new(8, Some(1 << 40), 3, Some(10 * MIB));
        assert_eq!((s.parse_threads, s.memory_budget), (3, 10 * MIB));
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("512M"), Ok(512 * MIB));
        assert_eq!(parse_size("2g"), Ok(2048 * MIB));
        assert_eq!(parse_size("64KiB"), Ok(64 * 1024));
        assert_eq!(parse_size("1000"), Ok(1000));
        assert!(parse_size("0").is_err());
        assert!(parse_size("5T").is_err());
        assert!(parse_size("x").is_err());
    }

    #[test]
    fn available_memory_is_reported_here() {
        if cfg!(any(target_os = "linux", windows)) {
            assert!(available_memory().unwrap() > 0);
        }
    }
}
