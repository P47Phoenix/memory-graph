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
/// Default share of the free memory (above the reserve) this process may
/// grow into. The budget itself is that share divided by the measured
/// growth per source byte (see [`MemoryPolicy`]), so it is in RSS terms.
/// The footprint leaves out allocator rounding and the store's caches
/// (measured 6-25% under the real growth), which the 30% slack covers.
pub const DEFAULT_FRACTION: f64 = 0.70;
/// Heap per source byte in flight assumed until measured. Measured on the
/// test corpus: about 25x (the extraction's tokens, symbols and their
/// strings, plus the pre-encoded stream and postings the writer takes).
pub const INITIAL_EXPANSION: f64 = 25.0;
/// The measured growth is trusted only once this much is in flight.
const CALIBRATE_MIN_HELD: u64 = 16 * MIB;

/// One reading of the machine's memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemSample {
    /// Physical RAM (or the container's limit when there is one below it).
    pub total: u64,
    /// Free for new allocations right now (page cache counted as free).
    pub available: u64,
    /// This process's resident set, if the platform reports it.
    pub rss: Option<u64>,
    /// Linux PSI `some avg10` for memory (percent of time some task
    /// stalled on memory), if present.
    pub psi_some_avg10: Option<f64>,
    /// Which probe produced it (`/proc/meminfo`, `/proc/meminfo+cgroup v2`,
    /// `sysinfo(2)`, `GlobalMemoryStatusEx`, `host_statistics64`), so a
    /// report from any machine says where its numbers came from.
    pub source: &'static str,
}

/// Read the machine's memory, or say why the platform would not tell:
/// `"/proc/meminfo: No such file or directory; sysinfo(2): ..."`,
/// `"host_statistics64 failed: 5"`, `"unsupported platform: freebsd"`.
pub fn sample_memory() -> Result<MemSample, String> {
    #[cfg(target_os = "linux")]
    {
        linux::sample()
    }
    #[cfg(target_os = "macos")]
    {
        macos::sample()
    }
    #[cfg(windows)]
    {
        windows::sample()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(format!("unsupported platform: {}", std::env::consts::OS))
    }
}

/// Bytes of memory available to new allocations, if the platform says.
pub fn available_memory() -> Option<u64> {
    sample_memory().ok().map(|m| m.available)
}

/// `MemTotal` and `MemAvailable` (bytes) from `/proc/meminfo` text.
/// Kernels before 3.14 have no `MemAvailable`: then the classic estimate
/// `MemFree + Buffers + Cached + SReclaimable - Shmem`. A missing or
/// malformed line is an error that quotes it.
pub fn parse_meminfo(text: &str) -> Result<(u64, u64), String> {
    let field = |key: &str| -> Result<Option<u64>, String> {
        let Some(line) = text.lines().find(|l| l.starts_with(key)) else {
            return Ok(None);
        };
        let mut it = line[key.len()..].split_whitespace();
        let n: u64 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("/proc/meminfo: cannot parse `{}`", line.trim()))?;
        // Every field is in kB (the unit is spelled out, so check it).
        match it.next() {
            None | Some("kB") => Ok(Some(n * 1024)),
            Some(_) => Err(format!(
                "/proc/meminfo: unexpected unit in `{}`",
                line.trim()
            )),
        }
    };
    let total = field("MemTotal:")?.ok_or("/proc/meminfo: no MemTotal line")?;
    let available = match field("MemAvailable:")? {
        Some(a) => a,
        None => {
            let free = field("MemFree:")?.ok_or("/proc/meminfo: no MemAvailable or MemFree")?;
            let cached = field("Buffers:")?.unwrap_or(0)
                + field("Cached:")?.unwrap_or(0)
                + field("SReclaimable:")?.unwrap_or(0);
            (free + cached).saturating_sub(field("Shmem:")?.unwrap_or(0))
        }
    };
    if total == 0 {
        return Err("/proc/meminfo: MemTotal is 0".into());
    }
    Ok((total, available.min(total)))
}

/// Apply a container's memory limit to a machine reading: when `limit`
/// (a cgroup `memory.max` / `memory.limit_in_bytes`, `None` for `max`) is
/// below the machine's `total`, the container's total is the limit and what
/// is available is at most the limit less the cgroup's `usage` (its
/// `memory.current` less reclaimable file pages). Returns `None` when the
/// limit does not bind (absent, `max`, or v1's huge "unlimited" sentinel
/// at or above physical RAM).
pub fn apply_cgroup(
    total: u64,
    available: u64,
    limit: Option<u64>,
    usage: Option<u64>,
) -> Option<(u64, u64)> {
    let limit = limit.filter(|&l| l > 0 && l < total)?;
    let headroom = limit.saturating_sub(usage.unwrap_or(0));
    Some((limit, available.min(headroom)))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{apply_cgroup, parse_meminfo, MemSample};
    use std::path::{Path, PathBuf};

    pub fn sample() -> Result<MemSample, String> {
        let (total, available, source) = match std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| format!("/proc/meminfo: {e}"))
            .and_then(|s| parse_meminfo(&s))
        {
            Ok((t, a)) => (t, a, "/proc/meminfo"),
            Err(proc_err) => {
                let (t, a) = sysinfo2().map_err(|e| format!("{proc_err}; sysinfo(2): {e}"))?;
                (t, a, "sysinfo(2)")
            }
        };
        let (total, available, source) = match cgroup_limit() {
            Some((v, limit, usage)) => match apply_cgroup(total, available, limit, usage) {
                Some((t, a)) => (
                    t,
                    a,
                    match (source, v) {
                        ("/proc/meminfo", 2) => "/proc/meminfo+cgroup v2",
                        ("/proc/meminfo", _) => "/proc/meminfo+cgroup v1",
                        (_, 2) => "sysinfo(2)+cgroup v2",
                        _ => "sysinfo(2)+cgroup v1",
                    },
                ),
                None => (total, available, source),
            },
            None => (total, available, source),
        };
        Ok(MemSample {
            total,
            available,
            rss: rss(),
            psi_some_avg10: psi(),
            source,
        })
    }

    /// `sysinfo(2)`: total and free RAM plus buffers, for a box without
    /// `/proc` (a sandbox, a minimal container). No page cache figure, so
    /// "available" is on the low side.
    fn sysinfo2() -> Result<(u64, u64), String> {
        // SAFETY: `sysinfo` fills a plain struct; the return value is checked.
        let mut s: libc::sysinfo = unsafe { std::mem::zeroed() };
        if unsafe { libc::sysinfo(&mut s) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let unit = (s.mem_unit as u64).max(1);
        let total = (s.totalram as u64).saturating_mul(unit);
        let available = (s.freeram as u64)
            .saturating_add(s.bufferram as u64)
            .saturating_mul(unit);
        if total == 0 {
            return Err("reported 0 total".into());
        }
        Ok((total, available.min(total)))
    }

    /// The tightest memory limit on this process's cgroup (v2 or v1) and
    /// that cgroup's usage (`current` less inactive file pages, the way
    /// `docker stats` counts it): `(version, limit, usage)`. `None` when no
    /// cgroup memory controller is visible.
    fn cgroup_limit() -> Option<(u8, Option<u64>, Option<u64>)> {
        let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let mut v2_path = None;
        let mut v1_path = None;
        for line in text.lines() {
            let mut parts = line.splitn(3, ':');
            let (Some(_), Some(ctl), Some(path)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if ctl.is_empty() {
                v2_path = Some(path.to_string());
            } else if ctl.split(',').any(|c| c == "memory") {
                v1_path = Some(path.to_string());
            }
        }
        if let Some(p) = v2_path {
            if let Some(r) = walk_up(
                Path::new("/sys/fs/cgroup"),
                &p,
                "memory.max",
                "memory.current",
                "inactive_file",
            ) {
                return Some((2, r.0, r.1));
            }
        }
        if let Some(p) = v1_path {
            if let Some(r) = walk_up(
                Path::new("/sys/fs/cgroup/memory"),
                &p,
                "memory.limit_in_bytes",
                "memory.usage_in_bytes",
                "total_inactive_file",
            ) {
                return Some((1, r.0, r.1));
            }
        }
        None
    }

    /// From the cgroup at `root/path` up to `root` (a limit may sit on an
    /// ancestor, and a container sees its own cgroup at the root), the
    /// smallest limit found and the usage of the cgroup that carries it.
    /// `None` when no level has a readable limit file.
    fn walk_up(
        root: &Path,
        path: &str,
        limit_file: &str,
        usage_file: &str,
        inactive_key: &str,
    ) -> Option<(Option<u64>, Option<u64>)> {
        let mut dir: PathBuf = root.join(path.trim_start_matches('/'));
        let mut best: Option<(Option<u64>, Option<u64>)> = None;
        let mut seen_any = false;
        loop {
            if let Some(raw) = read_trimmed(&dir.join(limit_file)) {
                seen_any = true;
                // `max` (v2) or a number; v1's unlimited sentinel is a huge
                // number that `apply_cgroup` treats as no limit.
                let limit = raw.parse::<u64>().ok();
                let usage = read_trimmed(&dir.join(usage_file))
                    .and_then(|u| u.parse::<u64>().ok())
                    .map(|u| {
                        let inactive = std::fs::read_to_string(dir.join("memory.stat"))
                            .ok()
                            .and_then(|s| {
                                let l = s.lines().find(|l| l.starts_with(inactive_key))?;
                                l.split_whitespace().nth(1)?.parse::<u64>().ok()
                            })
                            .unwrap_or(0);
                        u.saturating_sub(inactive)
                    });
                best = match (best, limit) {
                    (None, _) => Some((limit, usage)),
                    (Some((Some(b), _)), Some(l)) if l < b => Some((Some(l), usage)),
                    (Some((None, _)), Some(l)) => Some((Some(l), usage)),
                    (b, _) => b,
                };
            }
            if !dir.starts_with(root) || dir == root {
                break;
            }
            match dir.parent() {
                Some(p) => dir = p.to_path_buf(),
                None => break,
            }
        }
        seen_any.then_some(best.unwrap_or((None, None)))
    }

    fn read_trimmed(p: &Path) -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// This process's resident set: `VmRSS` from `/proc/self/status` (kB
    /// whatever the page size), else `/proc/self/statm` in pages.
    fn rss() -> Option<u64> {
        let from_status = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
                line.split_whitespace().nth(1)?.parse::<u64>().ok()
            })
            .map(|kb| kb * 1024);
        from_status.or_else(|| {
            let s = std::fs::read_to_string("/proc/self/statm").ok()?;
            let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
            // SAFETY: sysconf has no preconditions.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            (page > 0).then(|| pages.saturating_mul(page as u64))
        })
    }

    fn psi() -> Option<f64> {
        let s = std::fs::read_to_string("/proc/pressure/memory").ok()?;
        let line = s.lines().find(|l| l.starts_with("some"))?;
        let f = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
        f["avg10=".len()..].parse::<f64>().ok()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::MemSample;

    pub fn sample() -> Result<MemSample, String> {
        let total = sysctl_u64("hw.memsize")?;
        let page = sysctl_u64("hw.pagesize").unwrap_or(4096).max(1);
        let vm = vm_statistics()?;
        // Activity Monitor's "memory used": app memory (internal pages less
        // the purgeable ones) + wired + compressed. Free, inactive, file-
        // backed and purgeable pages are all reclaimable, so everything else
        // is available. (Counting free + inactive + purgeable + file-backed
        // instead would count inactive file pages twice.)
        let used = (vm.wire_count as u64)
            .saturating_add(vm.compressor_page_count as u64)
            .saturating_add(
                (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64),
            )
            .saturating_mul(page);
        Ok(MemSample {
            total,
            available: total.saturating_sub(used),
            rss: rss(),
            psi_some_avg10: None,
            source: "host_statistics64",
        })
    }

    fn sysctl_u64(name: &str) -> Result<u64, String> {
        let cname = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `cname` is NUL-terminated; `v`/`len` are valid for the
        // sizes passed; no new value is set; the return value is checked.
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                &mut v as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(format!(
                "sysctl {name}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if v == 0 {
            return Err(format!("sysctl {name}: reported 0"));
        }
        Ok(v)
    }

    extern "C" {
        // In libSystem, not exported by `libc`: releases the host port
        // `mach_host_self` hands out (each call takes a reference).
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    // `mach_host_self` is deprecated in `libc` (it would rather we used the
    // `mach2` crate for Mach calls); it is the only way to name the host
    // and this is the only place we do.
    #[allow(deprecated)]
    fn vm_statistics() -> Result<libc::vm_statistics64, String> {
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `vm` is a plain struct of exactly `count` integers;
        // `host_statistics64` writes at most that many; the host port is
        // released after the call; the return value is checked.
        let kr = unsafe {
            let host = libc::mach_host_self();
            let kr = libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                &mut vm as *mut libc::vm_statistics64 as libc::host_info64_t,
                &mut count,
            );
            mach_port_deallocate(libc::mach_task_self(), host);
            kr
        };
        if kr != libc::KERN_SUCCESS {
            return Err(format!("host_statistics64 failed: {kr}"));
        }
        Ok(vm)
    }

    fn rss() -> Option<u64> {
        let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `ti` is a plain struct of `size` bytes; the return value
        // (bytes written) is checked against it.
        let n = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                &mut ti as *mut libc::proc_taskinfo as *mut libc::c_void,
                size,
            )
        };
        (n == size).then_some(ti.pti_resident_size)
    }
}

#[cfg(windows)]
mod windows {
    use super::MemSample;

    pub fn sample() -> Result<MemSample, String> {
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
                return Err(format!(
                    "GlobalMemoryStatusEx failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let rss = (K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0)
                .then_some(pmc.WorkingSetSize as u64);
            Ok(MemSample {
                total: m.ullTotalPhys,
                available: m.ullAvailPhys,
                rss,
                psi_some_avg10: None,
                source: "GlobalMemoryStatusEx",
            })
        }
    }
}

/// What `--memory` asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemorySpec {
    /// A fixed budget (`2G`): never re-sampled.
    Fixed(u64),
    /// A share of the free memory above the OS reserve (`80%`) that this
    /// process may grow into, followed during the run and divided by the
    /// measured growth per source byte.
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
                "T" => 1024 * 1024 * MIB,
                _ => return Err(format!("unknown size unit in `{s}` (use K, M, G or T)")),
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
    /// Measured heap bytes per source byte in flight (a running average
    /// of footprint ÷ held, starting at [`INITIAL_EXPANSION`]).
    pub expansion: f64,
    /// Why the platform gave no first reading (shown in the fallback
    /// reason), if it gave none.
    pub unknown_cause: Option<String>,
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
    /// Start from the first sample, or from why there is none.
    pub fn new(spec: MemorySpec, floor: u64, first: Result<&MemSample, &str>) -> Self {
        let mut p = Self {
            spec,
            floor: floor.max(1),
            under_pressure: false,
            reason: String::new(),
            cap: 1,
            expansion: INITIAL_EXPANSION,
            unknown_cause: first.err().map(str::to_string),
        };
        let d = p.decide(first.ok(), 0, 0);
        p.apply(&d);
        p
    }

    fn apply(&mut self, d: &Decision) {
        self.cap = d.cap;
        self.under_pressure = d.under_pressure;
        self.reason.clone_from(&d.reason);
    }

    /// Feed a sample, with `held` source bytes in flight, all of them
    /// prepared and taking `footprint` bytes of heap. See `update_measured`.
    pub fn update(
        &mut self,
        sample: Option<&MemSample>,
        held: u64,
        footprint: u64,
    ) -> Option<Decision> {
        self.update_measured(sample, held, held, footprint)
    }

    /// Feed a sample, with `held` source bytes in flight of which `prepared`
    /// are parsed and take `footprint` bytes of heap (the rest are still
    /// being read or parsed); returns the new cap when it changed enough to
    /// matter. Refines the growth estimate from footprint ÷ prepared once
    /// enough is prepared for the ratio to mean something.
    pub fn update_measured(
        &mut self,
        sample: Option<&MemSample>,
        held: u64,
        prepared: u64,
        footprint: u64,
    ) -> Option<Decision> {
        // (A zero footprint means nothing measured yet, not free parsing.)
        if prepared >= CALIBRATE_MIN_HELD && footprint > 0 {
            let seen = (footprint as f64 / prepared as f64).clamp(1.0, 64.0);
            self.expansion = 0.7 * self.expansion + 0.3 * seen;
        }
        // Bytes still being parsed will take about the estimate.
        let growth = footprint + (held.saturating_sub(prepared) as f64 * self.expansion) as u64;
        let d = self.decide(sample, held, growth);
        let moved = d.under_pressure != self.under_pressure
            || (d.cap as f64 - self.cap as f64).abs() > 0.10 * self.cap as f64;
        if !moved {
            return None;
        }
        self.apply(&d);
        Some(d)
    }

    /// The cap for `sample`, given `held` source bytes in flight taking
    /// `growth` bytes of heap (measured, or estimated when 0).
    fn decide(&self, sample: Option<&MemSample>, held: u64, growth: u64) -> Decision {
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
            let cause = match (sample, &self.unknown_cause) {
                (Some(_), _) => "reported 0 total".to_string(),
                (None, Some(c)) => c.clone(),
                (None, None) => "no reading".to_string(),
            };
            return Decision {
                cap: FALLBACK_BUDGET.max(self.floor),
                under_pressure: false,
                reason: format!(
                    "free RAM unknown ({cause}); assuming {}",
                    mb(FALLBACK_BUDGET)
                ),
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
        // so the budget does not shrink itself as it fills. The share of that
        // headroom divided by the heap per source byte gives the budget in
        // source bytes.
        let growth = if growth > 0 {
            growth
        } else {
            (held as f64 * self.expansion) as u64
        };
        let spare = m.available.saturating_add(growth).saturating_sub(reserve);
        let target = (spare as f64 * fraction / self.expansion) as u64;
        let ceiling = m.total / 2;
        let floor = self.floor.max(FLOOR);
        let cap = target.clamp(floor, ceiling.max(floor));
        Decision {
            cap,
            under_pressure: false,
            reason: format!(
                "{}% of {} free above the {} OS reserve ÷ {:.1}× growth per source byte{}",
                pct(fraction),
                mb(m.available),
                mb(reserve),
                self.expansion,
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
    /// The first reading, or why there is none.
    pub memory: Result<MemSample, String>,
    pub policy: MemoryPolicy,
}

impl Sizing {
    /// Parse threads: every CPU but one (the committing thread keeps one
    /// busy), at least one. Budget: from `spec` (default: a quarter of the
    /// free memory above the 20% kept for the OS, re-sampled during the run;
    /// see [`MemoryPolicy`]). `floor` is the least budget ever set.
    pub fn new(
        cpus: usize,
        memory: Result<MemSample, String>,
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
        let policy = MemoryPolicy::new(spec, floor, memory.as_ref().map_err(String::as_str));
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
            source: "test",
        }
    }

    const KB: u64 = 1024;

    #[test]
    fn meminfo_parses_with_and_without_memavailable() {
        let modern = "MemTotal:       32403152 kB\nMemFree:        30759580 kB\nMemAvailable:   31116752 kB\nBuffers:           44848 kB\nCached:           661000 kB\n";
        assert_eq!(parse_meminfo(modern), Ok((32403152 * KB, 31116752 * KB)));
        // Pre-3.14: the classic estimate, shared memory not counted as free.
        let old = "MemTotal:  1000 kB\nMemFree:   100 kB\nBuffers:   50 kB\nCached:    300 kB\nShmem:     20 kB\nSReclaimable: 10 kB\n";
        assert_eq!(parse_meminfo(old), Ok((1000 * KB, 440 * KB)));
        // Available never exceeds total.
        assert_eq!(
            parse_meminfo("MemTotal: 10 kB\nMemAvailable: 20 kB\n"),
            Ok((10 * KB, 10 * KB))
        );
        // Missing or malformed lines say which.
        assert_eq!(
            parse_meminfo(""),
            Err("/proc/meminfo: no MemTotal line".into())
        );
        assert_eq!(
            parse_meminfo("MemTotal: 10 kB\n"),
            Err("/proc/meminfo: no MemAvailable or MemFree".into())
        );
        assert_eq!(
            parse_meminfo("MemTotal: lots kB\n"),
            Err("/proc/meminfo: cannot parse `MemTotal: lots kB`".into())
        );
        assert_eq!(
            parse_meminfo("MemTotal: 10 MB\nMemAvailable: 1 kB\n"),
            Err("/proc/meminfo: unexpected unit in `MemTotal: 10 MB`".into())
        );
        assert_eq!(
            parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"),
            Err("/proc/meminfo: MemTotal is 0".into())
        );
    }

    #[test]
    fn cgroup_limit_caps_total_and_available() {
        let (t, a) = (32u64 << 30, 28u64 << 30);
        // `max`, or a v1 "unlimited" sentinel at/above RAM: no limit.
        assert_eq!(apply_cgroup(t, a, None, Some(1 << 20)), None);
        assert_eq!(apply_cgroup(t, a, Some(9223372036854771712), None), None);
        assert_eq!(apply_cgroup(t, a, Some(t), None), None);
        assert_eq!(apply_cgroup(t, a, Some(0), None), None);
        // A 512 MB container: total is the limit, available what is left of it.
        assert_eq!(
            apply_cgroup(t, a, Some(512 << 20), Some(100 << 20)),
            Some((512 << 20, 412 << 20))
        );
        // Usage unknown: the whole limit.
        assert_eq!(
            apply_cgroup(t, a, Some(512 << 20), None),
            Some((512 << 20, 512 << 20))
        );
        // Usage above the limit (racing a reclaim): nothing left, not a wrap.
        assert_eq!(
            apply_cgroup(t, a, Some(512 << 20), Some(600 << 20)),
            Some((512 << 20, 0))
        );
        // The machine itself being fuller than the container wins.
        assert_eq!(
            apply_cgroup(t, 1 << 20, Some(512 << 20), Some(0)),
            Some((512 << 20, 1 << 20))
        );
    }

    #[test]
    fn unknown_memory_says_why() {
        let p = MemoryPolicy::new(
            MemorySpec::Fraction(0.7),
            FLOOR,
            Err("/proc/meminfo: No such file or directory"),
        );
        assert_eq!(p.cap, FALLBACK_BUDGET);
        assert_eq!(
            p.reason,
            "free RAM unknown (/proc/meminfo: No such file or directory); assuming 512 MB"
        );
        let s = Sizing::new(4, Err("unsupported platform: freebsd".into()), 0, None, 1);
        assert!(
            s.policy.reason.contains("(unsupported platform: freebsd)"),
            "{}",
            s.policy.reason
        );
    }

    /// The fraction budget for `avail` GB free of `total` GB, `held` bytes in
    /// flight, at growth `x`.
    fn expect(total_gb: u64, avail_gb: u64, held: u64, f: f64, x: f64) -> u64 {
        let total = total_gb << 30;
        let reserve = total / 5;
        let spare = ((avail_gb << 30) + held * x as u64).saturating_sub(reserve);
        ((spare as f64 * f / x) as u64).clamp(FLOOR, total / 2)
    }

    #[test]
    fn sizing_follows_the_hardware() {
        let s = Sizing::new(32, Ok(mem(64, 60)), 0, None, FLOOR);
        assert_eq!(s.parse_threads, 31);
        assert_eq!(
            s.memory_budget,
            expect(64, 60, 0, DEFAULT_FRACTION, INITIAL_EXPANSION)
        );
        assert!(
            s.policy.reason.contains(&format!(
                "{}% of 60.0 GB free",
                (DEFAULT_FRACTION * 100.0) as u64
            )),
            "{}",
            s.policy.reason
        );
        assert!(
            s.policy
                .reason
                .contains(&format!("{INITIAL_EXPANSION:.1}× growth")),
            "{}",
            s.policy.reason
        );
        let s = Sizing::new(1, Ok(mem(2, 1)), 0, None, 1);
        assert_eq!(s.parse_threads, 1);
        assert_eq!(s.memory_budget, FLOOR, "fraction floor");
        let s = Sizing::new(8, Err("no probe".into()), 0, None, FLOOR);
        assert_eq!(s.memory_budget, FALLBACK_BUDGET);
        assert!(s.policy.reason.contains("unknown"));
        let s = Sizing::new(8, Ok(mem(64, 60)), 3, Some(MemorySpec::Fixed(10 * MIB)), 1);
        assert_eq!((s.parse_threads, s.memory_budget), (3, 10 * MIB));
        // A fixed budget never goes below the floor.
        let s = Sizing::new(
            8,
            Ok(mem(64, 60)),
            3,
            Some(MemorySpec::Fixed(10 * MIB)),
            FLOOR,
        );
        assert_eq!(s.memory_budget, FLOOR);
        // The ceiling is half of RAM (at growth 1, 100% of 12.8 GB spare).
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(1.0), FLOOR, Ok(&mem(16, 16)));
        p.expansion = 1.0;
        assert_eq!(p.update(Some(&mem(16, 16)), 0, 0).unwrap().cap, 8 << 30);
    }

    /// A policy on `m` with the growth estimate at 1x, so a 16 GB fixture
    /// sits above the 256 MiB floor.
    fn unit_growth(f: f64, m: &MemSample) -> MemoryPolicy {
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(f), FLOOR, Ok(m));
        p.expansion = 1.0;
        let d = p.decide(Some(m), 0, 0);
        p.apply(&d);
        p
    }

    #[test]
    fn growth_is_measured_and_sets_the_budget() {
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.8), FLOOR, Ok(&mem(64, 40)));
        assert_eq!(p.cap, expect(64, 40, 0, 0.8, INITIAL_EXPANSION));
        // One sample moves the running average by 30% of the difference.
        let mut e = MemoryPolicy::new(MemorySpec::Fraction(0.8), FLOOR, Ok(&mem(64, 40)));
        e.update(Some(&mem(64, 40)), 1 << 30, 3 << 30);
        assert!((e.expansion - (0.7 * INITIAL_EXPANSION + 0.3 * 3.0)).abs() < 1e-9);
        // Too little in flight to trust a ratio: the estimate stays.
        p.update(Some(&mem(64, 40)), 8 << 20, (8 << 20) * 3);
        assert_eq!(p.expansion, INITIAL_EXPANSION);
        // Prepared files take 3x their source: the estimate converges to 3
        // and the budget doubles against the initial 6x guess.
        let held = 1u64 << 30;
        let mut m = mem(64, 40);
        m.available -= held * 3;
        for _ in 0..20 {
            p.update(Some(&m), held, held * 3);
        }
        assert!((p.expansion - 3.0).abs() < 0.05, "{}", p.expansion);
        // The cap follows (within the 10% deadband of the last move).
        let want = expect(64, 40, 0, 0.8, p.expansion) as f64;
        assert!(
            (p.cap as f64 / want - 1.0).abs() < 0.10,
            "{} vs {want}",
            p.cap
        );
        assert!(
            p.reason.contains("× growth per source byte"),
            "{}",
            p.reason
        );
        // Bytes admitted but still being parsed do not drag the ratio down:
        // a few large files mid-parse next to a few small prepared ones.
        let x = p.expansion;
        let d = p.update_measured(Some(&m), 64 << 20, 1 << 20, (1 << 20) * 3);
        assert_eq!(p.expansion, x, "{d:?}");
        // ...and they count at the estimate towards what is held.
        let mut q = MemoryPolicy::new(MemorySpec::Fraction(0.8), FLOOR, Ok(&mem(64, 40)));
        let d = q.update_measured(Some(&mem(64, 30)), 3 << 30, 1 << 30, 2 << 30);
        // (That call also refined the estimate from the 2x it measured.)
        let x = q.expansion;
        assert!(x < INITIAL_EXPANSION && x > 2.0);
        let growth = (2u64 << 30) + ((2u64 << 30) as f64 * x) as u64;
        let spare = ((30u64 << 30) + growth) - (64u64 << 30) / 5;
        assert_eq!(
            d.unwrap().cap,
            ((spare as f64 * 0.8 / x) as u64).min(32 << 30)
        );
        // Wild ratios are clamped.
        for _ in 0..40 {
            p.update(Some(&m), held, held * 500);
        }
        assert!(p.expansion <= 64.0);
        // Before any footprint is known, held bytes stand in at the estimate.
        let mut q = MemoryPolicy::new(MemorySpec::Fraction(0.8), FLOOR, Ok(&mem(64, 40)));
        let d = q.update(Some(&mem(64, 30)), 2 << 30, 0).unwrap();
        assert_eq!(d.cap, expect(64, 30, 2 << 30, 0.8, INITIAL_EXPANSION));
    }

    #[test]
    fn policy_follows_free_memory_and_backs_off_under_pressure() {
        let mut p = unit_growth(0.25, &mem(16, 8));
        assert_eq!(p.cap, expect(16, 8, 0, 0.25, 1.0));
        assert!(!p.under_pressure);
        // A wobble under the 10% deadband is not reported; a bigger move is.
        assert!(p.update(Some(&mem(16, 8)), 0, 0).is_none());
        // (On a box big enough that the floor is not what sets the cap.)
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Ok(&mem(64, 40)));
        let cap0 = p.cap;
        assert!(cap0 > FLOOR);
        let mut wobble = mem(64, 40);
        // Free memory that moves the target by this share of the cap.
        let step = |k: f64| (cap0 as f64 * k * INITIAL_EXPANSION / 0.25) as u64;
        wobble.available += step(0.05);
        assert!(p.update(Some(&wobble), 0, 0).is_none());
        wobble.available = (40u64 << 30) + step(0.15);
        assert!(p.update(Some(&wobble), 0, 0).is_some());
        let mut p = unit_growth(0.25, &mem(16, 8));
        // Plenty more free: the cap grows.
        let d = p.update(Some(&mem(16, 12)), 0, 0).unwrap();
        assert!(d.cap > expect(16, 8, 0, 0.25, 1.0));
        // What we hold (at the growth estimate) is added back, so filling
        // the budget does not shrink it.
        let held = 1u64 << 30;
        let mut fuller = mem(16, 12);
        fuller.available -= held;
        let d = p.update(Some(&fuller), held, 0);
        assert!(d.is_none(), "{d:?}");
        // Free memory falls below the 20% reserve: pressure, cap = held/2.
        let d = p.update(Some(&mem(16, 3)), 2 << 30, 0).unwrap();
        assert!(d.under_pressure);
        assert_eq!(d.cap, 1 << 30);
        assert!(d.reason.contains("pressure") && d.reason.contains("for the OS"));
        // Slightly above the reserve is not enough to leave (hysteresis).
        assert!(p.update(Some(&mem(16, 3)), 2 << 30, 0).is_none());
        let d = p.update(Some(&mem(16, 4)), 2 << 30, 0);
        assert!(d.is_none() || d.as_ref().unwrap().under_pressure);
        // Comfortably above it: back to the fraction target.
        let d = p.update(Some(&mem(16, 6)), 2 << 30, 0).unwrap();
        assert!(!d.under_pressure);
        assert!(d.cap >= FLOOR);
        // Pressure never goes below the hard floor.
        let d = p.update(Some(&mem(16, 1)), 0, 0).unwrap();
        assert_eq!(d.cap, FLOOR);
        let mut q = MemoryPolicy::new(MemorySpec::Fraction(0.25), 1, Ok(&mem(16, 10)));
        assert_eq!(q.update(Some(&mem(16, 1)), 0, 0).unwrap().cap, 64 << 20);
        // Our own growth counts too.
        let mut big = mem(16, 10);
        big.rss = Some(11 << 30);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Ok(&mem(16, 10)));
        let d = p.update(Some(&big), 0, 0).unwrap();
        assert!(d.under_pressure && d.reason.contains("this process"));
        // And a Linux PSI stall.
        let mut stall = mem(16, 10);
        stall.psi_some_avg10 = Some(25.0);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), FLOOR, Ok(&mem(16, 10)));
        assert!(p
            .update(Some(&stall), 0, 0)
            .unwrap()
            .reason
            .contains("stalling"));
        // A bogus total is treated as no reading, not as pressure.
        let mut bogus = mem(0, 0);
        bogus.rss = Some(1 << 20);
        let mut p = MemoryPolicy::new(MemorySpec::Fraction(0.25), 1, Ok(&bogus));
        assert!(!p.under_pressure && p.reason.contains("unknown"));
        assert!(p.update(Some(&bogus), 0, 0).is_none());
        // Fixed never moves.
        let mut p = MemoryPolicy::new(MemorySpec::Fixed(1 << 30), 1, Ok(&mem(16, 10)));
        assert!(p.update(Some(&mem(16, 1)), 5 << 30, 0).is_none());
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
        assert_eq!(parse_size("5T"), Ok(5 * 1024 * 1024 * MIB));
        assert!(parse_size("5P").is_err());
        assert!(parse_size("x").is_err());
    }

    #[test]
    fn memory_is_sampled_here() {
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            let m = sample_memory().unwrap();
            assert!(m.total >= m.available && m.available > 0, "{m:?}");
            assert!(m.rss.unwrap() > 0, "{m:?}");
            assert!(!m.source.is_empty());
        } else {
            let e = sample_memory().unwrap_err();
            assert!(e.starts_with("unsupported platform: "), "{e}");
        }
    }
}
