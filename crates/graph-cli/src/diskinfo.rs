//! Disk space for `memory-graph index`: how much the database's volume has
//! free, how big the database is likely to get, and when to stop before the
//! disk fills (a 10 GB tree once produced a 420 GB v1 database and died on
//! `No space left on device`).
use std::path::Path;
use std::sync::Arc;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// One reading of the volume the database lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSample {
    pub total: u64,
    /// Free for this user right now.
    pub available: u64,
}

/// A source of disk readings; tests inject one that scripts a shrinking disk.
pub type DiskProbe = Arc<dyn Fn(&Path) -> Option<DiskSample> + Send + Sync>;

/// Read the volume holding `path` (a file or directory), if the platform says.
pub fn sample_disk(path: &Path) -> Option<DiskSample> {
    // The database may not exist yet: ask about its directory.
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| Path::new(".").to_path_buf(), Path::to_path_buf)
    };
    sample_dir(&dir)
}

/// Which call reads the volume on this platform (for `sysinfo`).
#[cfg(windows)]
pub const DISK_SOURCE: &str = "GetDiskFreeSpaceExW";
#[cfg(unix)]
pub const DISK_SOURCE: &str = "statvfs";
#[cfg(not(any(windows, unix)))]
pub const DISK_SOURCE: &str = "none";

#[cfg(windows)]
fn sample_dir(dir: &Path) -> Option<DiskSample> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the three out
    // pointers are valid u64s; the return value is checked.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(DiskSample {
        total,
        available: avail,
    })
}

#[cfg(unix)]
fn sample_dir(dir: &Path) -> Option<DiskSample> {
    let s = rustix::fs::statvfs(dir).ok()?;
    let frsize = if s.f_frsize > 0 {
        s.f_frsize
    } else {
        s.f_bsize
    };
    Some(DiskSample {
        total: s.f_blocks.saturating_mul(frsize),
        available: s.f_bavail.saturating_mul(frsize),
    })
}

#[cfg(not(any(windows, unix)))]
fn sample_dir(_dir: &Path) -> Option<DiskSample> {
    None
}

/// Database bytes per source byte assumed until measured. `scripts/
/// measure-size.py` on a 20x copy of `testdata/corpus` (2026-09-25): 8.0x
/// fresh whatever the commit mode, no growth on an unchanged rerun, 16x
/// after `--reindex` until `vacuum --compact`; real monorepos report 7-8x
/// (#67). The margin covers redb growing its file in region steps.
pub const DISK_RATIO: f64 = 10.0;
/// The projection trusts the measured ratio once this much source is stored.
const RATIO_CALIBRATE_MIN: u64 = 64 * MIB;
/// Never let the volume drop below this (or 5% of it, whichever is more,
/// up to `MIN_FREE_CEILING`) unless told otherwise: the OS, logs and other
/// programs need room too, but a multi-terabyte volume does not need
/// hundreds of gigabytes kept idle.
pub const MIN_FREE_FLOOR: u64 = 2 * GIB;
pub const MIN_FREE_FRACTION: f64 = 0.05;
pub const MIN_FREE_CEILING: u64 = 32 * GIB;

/// How much free space to keep: `Bytes` from `--min-free-disk 4G`,
/// `Fraction` from `5%`, or the default rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinFree {
    Default,
    Bytes(u64),
    Fraction(f64),
}

/// Parse `--min-free-disk`: a size (`4G`) or a share of the volume (`5%`).
pub fn parse_min_free(s: &str) -> Result<MinFree, String> {
    Ok(match crate::sysinfo::parse_memory_spec(s)? {
        crate::sysinfo::MemorySpec::Fixed(b) => MinFree::Bytes(b),
        crate::sysinfo::MemorySpec::Fraction(f) => MinFree::Fraction(f),
    })
}

impl MinFree {
    pub fn resolve(self, total: Option<u64>) -> u64 {
        match self {
            MinFree::Bytes(b) => b,
            MinFree::Fraction(f) => total.map_or(MIN_FREE_FLOOR, |t| (t as f64 * f) as u64),
            MinFree::Default => total.map_or(MIN_FREE_FLOOR, |t| {
                ((t as f64 * MIN_FREE_FRACTION) as u64).clamp(MIN_FREE_FLOOR, MIN_FREE_CEILING)
            }),
        }
    }
}

/// What the policy decided from one reading.
#[derive(Debug, Clone, PartialEq)]
pub struct DiskDecision {
    /// Bytes the database will probably reach once every found file is stored.
    pub projected_final: u64,
    /// The database-bytes-per-newly-stored-source-byte in use (measured or
    /// assumed).
    pub ratio: f64,
    /// The free space to keep.
    pub min_free: u64,
    /// Why the run must stop now, if it must.
    pub stop: Option<String>,
}

/// What the pipeline knows when the policy is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskInputs {
    /// The database file now, and when this run started (a rerun starts on
    /// a file that already holds earlier runs' data).
    pub db_len: u64,
    pub db_len_start: u64,
    /// Source bytes the walk has found, the writer has handled, and of those
    /// how many were already stored (unchanged: they cost no space).
    pub found_bytes: u64,
    pub handled_bytes: u64,
    pub unchanged_bytes: u64,
    /// Only then is the projection complete enough to refuse on.
    pub walk_done: bool,
    /// Source bytes one group commit may take: the stop must leave room for
    /// the commit in progress, which cannot be interrupted.
    pub group_bytes: u64,
}

/// Pure decision logic, fed each sample by the pipeline's sampler.
#[derive(Debug, Clone, PartialEq)]
pub struct DiskPolicy {
    pub min_free: MinFree,
    /// `--no-disk-check`: report, never stop.
    pub enforce: bool,
}

pub(crate) fn mb(b: u64) -> String {
    let m = b as f64 / MIB as f64;
    if m >= 1024.0 * 1024.0 {
        format!("{:.1} TB", m / (1024.0 * 1024.0))
    } else if m >= 1024.0 {
        format!("{:.1} GB", m / 1024.0)
    } else {
        format!("{m:.0} MB")
    }
}

impl DiskPolicy {
    /// Decide from `sample` (None when the platform says nothing) and what
    /// the run has seen so far. The ratio is the file's growth this run over
    /// the source bytes newly stored this run (so a rerun onto a full file
    /// does not count earlier runs' data), and the projection assumes the
    /// rest of the tree is new in the same proportion as what was handled
    /// (a resume walks over already-stored files first and projects little).
    pub fn decide(&self, sample: Option<&DiskSample>, i: DiskInputs) -> DiskDecision {
        let stored = i.handled_bytes.saturating_sub(i.unchanged_bytes);
        let growth = i.db_len.saturating_sub(i.db_len_start);
        let ratio = if stored >= RATIO_CALIBRATE_MIN {
            (growth as f64 / stored as f64).max(2.0)
        } else {
            DISK_RATIO
        };
        let new_share = if i.handled_bytes > 0 {
            stored as f64 / i.handled_bytes as f64
        } else {
            1.0
        };
        let remaining = i.found_bytes.saturating_sub(i.handled_bytes);
        let projected_final = i
            .db_len
            .saturating_add((remaining as f64 * ratio * new_share) as u64);
        let min_free = self.min_free.resolve(sample.map(|s| s.total));
        let mut d = DiskDecision {
            projected_final,
            ratio,
            min_free,
            stop: None,
        };
        let Some(s) = sample else {
            return d;
        };
        if !self.enforce {
            return d;
        }
        // A commit in progress cannot be interrupted, so stop while there is
        // still room for one whole group on top of the reserve.
        let group_room = (i.group_bytes as f64 * ratio) as u64;
        if s.available < min_free.saturating_add(group_room) {
            d.stop = Some(format!(
                "only {} free on the database's volume (keeping {} plus {} for the commit in progress)",
                mb(s.available),
                mb(min_free),
                mb(group_room)
            ));
        } else if i.walk_done {
            let needed = projected_final.saturating_sub(i.db_len);
            if s.available.saturating_sub(needed) < min_free {
                d.stop = Some(format!(
                    "the database would reach about {} ({:.1}x the new source) but only {} is free (keeping {})",
                    mb(projected_final),
                    ratio,
                    mb(s.available),
                    mb(min_free)
                ));
            }
        }
        d
    }
}

/// Source bytes one group commit may take so that it fits: half of what is
/// free above the reserve, at the current ratio, and at least 1 MiB (below
/// that the run stops anyway). A small volume gets small commits instead of
/// an early refusal.
pub fn group_fit(available: u64, min_free: u64, ratio: f64) -> u64 {
    let room = available.saturating_sub(min_free) / 2;
    ((room as f64 / ratio.max(1.0)) as u64).max(MIB)
}

/// Whether a storage error is the disk filling up (Linux ENOSPC 28, Windows
/// ERROR_DISK_FULL 112 / ERROR_HANDLE_DISK_FULL 39), by text since redb
/// stringifies the `io::Error`.
pub fn is_disk_full(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    let code = |n: u32| m.contains(&format!("os error {n})"));
    m.contains("no space left on device")
        || m.contains("not enough space on the disk")
        || (cfg!(unix) && code(28))
        || (cfg!(windows) && (code(112) || code(39)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(total_gb: u64, avail_gb: u64) -> DiskSample {
        DiskSample {
            total: total_gb * GIB,
            available: avail_gb * GIB,
        }
    }

    #[test]
    fn min_free_flag_parses() {
        assert_eq!(parse_min_free("4G"), Ok(MinFree::Bytes(4 * GIB)));
        assert_eq!(parse_min_free("5%"), Ok(MinFree::Fraction(0.05)));
        assert_eq!(mb(1 << 40), "1.0 TB");
        assert!(parse_min_free("0").is_err());
    }

    #[test]
    fn min_free_rules() {
        assert_eq!(MinFree::Default.resolve(Some(20 * GIB)), 2 * GIB, "floor");
        assert_eq!(MinFree::Default.resolve(Some(100 * GIB)), 5 * GIB, "5%");
        assert_eq!(
            MinFree::Default.resolve(Some(4000 * GIB)),
            32 * GIB,
            "ceiling"
        );
        assert_eq!(MinFree::Default.resolve(None), 2 * GIB);
        assert_eq!(MinFree::Bytes(7).resolve(Some(100 * GIB)), 7);
        assert_eq!(MinFree::Fraction(0.1).resolve(Some(100 * GIB)), 10 * GIB);
    }

    fn inputs(db_len: u64, found: u64, handled: u64, walk_done: bool) -> DiskInputs {
        DiskInputs {
            db_len,
            db_len_start: 0,
            found_bytes: found,
            handled_bytes: handled,
            unchanged_bytes: 0,
            walk_done,
            group_bytes: 0,
        }
    }

    #[test]
    fn projection_and_calibration() {
        let p = DiskPolicy {
            min_free: MinFree::Default,
            enforce: true,
        };
        // Nothing stored yet: the assumed ratio projects the whole tree.
        let d = p.decide(Some(&disk(1000, 500)), inputs(0, 10 * GIB, 0, false));
        assert_eq!(d.ratio, DISK_RATIO);
        assert_eq!(d.projected_final, 100 * GIB);
        assert!(d.stop.is_none());
        // Once 64 MiB is newly stored the measured ratio takes over.
        let d = p.decide(
            Some(&disk(1000, 500)),
            inputs(800 * MIB, 10 * GIB, 100 * MIB, true),
        );
        assert!((d.ratio - 8.0).abs() < 1e-9);
        // ...never below 2x.
        let d = p.decide(
            Some(&disk(1000, 500)),
            inputs(100 * MIB, 10 * GIB, 100 * MIB, true),
        );
        assert_eq!(d.ratio, 2.0);
    }

    #[test]
    fn a_rerun_onto_a_full_file_does_not_over_project() {
        let p = DiskPolicy {
            min_free: MinFree::Bytes(GIB),
            enforce: true,
        };
        // Run 1 stopped with a 30 GB file after 3 GB of a 10 GB tree; the
        // rerun starts on that file, walks the stored files first (all
        // unchanged), and must not treat the old data as this run's growth.
        let d = p.decide(
            Some(&disk(100, 3)),
            DiskInputs {
                db_len: 30 * GIB,
                db_len_start: 30 * GIB,
                found_bytes: 10 * GIB,
                handled_bytes: 200 * MIB,
                unchanged_bytes: 200 * MIB,
                walk_done: true,
                group_bytes: 0,
            },
        );
        assert_eq!(d.ratio, DISK_RATIO, "nothing new stored yet: assumed ratio");
        assert_eq!(
            d.projected_final,
            30 * GIB,
            "everything seen so far was unchanged"
        );
        assert!(d.stop.is_none(), "{d:?}");
        // Past the stored files, new ones cost the measured ratio: 3 GB of
        // the seen 3.5 GB were unchanged, so 1/7 of the rest counts.
        let d = p.decide(
            Some(&disk(100, 60)),
            DiskInputs {
                db_len: 30 * GIB + 640 * MIB,
                db_len_start: 30 * GIB,
                found_bytes: 10 * GIB,
                handled_bytes: 3 * GIB + 512 * MIB,
                unchanged_bytes: 3 * GIB,
                walk_done: true,
                group_bytes: 0,
            },
        );
        assert!((d.ratio - 1.25f64.max(2.0)).abs() < 1e-9, "{d:?}");
        let remaining = (10 * GIB - (3 * GIB + 512 * MIB)) as f64;
        let want = (30 * GIB + 640 * MIB) as f64 + remaining * 2.0 / 7.0;
        assert!((d.projected_final as f64 - want).abs() < 1e6, "{d:?}");
        assert!(d.stop.is_none());
    }

    #[test]
    fn stops_on_headroom_and_on_projection() {
        let p = DiskPolicy {
            min_free: MinFree::Default,
            enforce: true,
        };
        // Below the reserve: stop whatever the projection.
        let d = p.decide(Some(&disk(1000, 3)), inputs(0, GIB, 0, false));
        assert!(
            d.stop.as_deref().unwrap().contains("only 3.0 GB free"),
            "{d:?}"
        );
        // Projection needs 100 GB but only 58 GB is free above the 32 GB
        // reserve... only once the walk is done.
        let d = p.decide(Some(&disk(1000, 90)), inputs(0, 10 * GIB, 0, false));
        assert!(d.stop.is_none(), "{d:?}");
        let d = p.decide(Some(&disk(1000, 90)), inputs(0, 10 * GIB, 0, true));
        assert!(
            d.stop
                .as_deref()
                .unwrap()
                .contains("would reach about 100.0 GB"),
            "{d:?}"
        );
        // Enough room: no stop.
        let d = p.decide(Some(&disk(1000, 200)), inputs(0, 10 * GIB, 0, true));
        assert!(d.stop.is_none());
        // The commit in progress needs room too: 512 MB of source at 10x is
        // 5 GB, so 35 GB free is not enough above the 32 GB reserve.
        let mut i = inputs(0, GIB, 0, false);
        i.group_bytes = 512 * MIB;
        let d = p.decide(Some(&disk(1000, 35)), i);
        assert!(
            d.stop
                .as_deref()
                .unwrap()
                .contains("for the commit in progress"),
            "{d:?}"
        );
        i.group_bytes = 64 * MIB;
        assert!(p.decide(Some(&disk(1000, 35)), i).stop.is_none());
        // Unknown platform never stops; --no-disk-check never stops.
        assert!(p.decide(None, inputs(0, 10 * GIB, 0, true)).stop.is_none());
        let off = DiskPolicy {
            min_free: MinFree::Default,
            enforce: false,
        };
        assert!(off
            .decide(Some(&disk(1000, 1)), inputs(0, 10 * GIB, 0, true))
            .stop
            .is_none());
    }

    #[test]
    fn groups_shrink_to_what_fits() {
        // 46 MB free, 4 MB reserve, 10x: 21 MB of room, 2.1 MB of source.
        assert_eq!(
            group_fit(46 * MIB, 4 * MIB, 10.0),
            (21.0 * MIB as f64 / 10.0) as u64
        );
        // Below the reserve the floor applies (the guard stops the run).
        assert_eq!(group_fit(3 * MIB, 4 * MIB, 10.0), MIB);
        // Plenty of room: huge.
        assert!(group_fit(500 * GIB, 32 * GIB, 8.0) > 20 * GIB);
    }

    #[test]
    fn disk_full_errors_are_recognized() {
        assert!(is_disk_full(
            "storage error: I/O error: No space left on device (os error 28)"
        ));
        assert!(is_disk_full(
            "There is not enough space on the disk. (os error 112)"
        ));
        if cfg!(windows) {
            let e = std::io::Error::from_raw_os_error(112);
            assert!(is_disk_full(&format!("{e}")));
        } else {
            let e = std::io::Error::from_raw_os_error(28);
            assert!(is_disk_full(&format!("{e}")));
        }
        assert!(!is_disk_full(
            "storage error: I/O error: permission denied (os error 13)"
        ));
    }

    #[test]
    fn this_volume_is_sampled_here() {
        if cfg!(any(windows, unix)) {
            let s = sample_disk(Path::new(".")).unwrap();
            assert!(s.total >= s.available && s.total > 0);
            let f = sample_disk(Path::new("./does-not-exist.redb")).unwrap();
            assert_eq!(f.total, s.total);
        }
    }
}
