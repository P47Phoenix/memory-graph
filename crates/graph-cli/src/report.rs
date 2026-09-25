//! `memory-graph sysinfo`: what the machine probes see, so a report from
//! any box (a sandbox without `/proc`, a container with a memory limit, a
//! Mac) says where its numbers came from or why there are none. The same
//! probes and policy size `index`; nothing here is a second opinion.
use crate::diskinfo::{self, DiskSample, MinFree};
use crate::sysinfo::{MemorySpec, Sizing};
use std::path::{Path, PathBuf};

/// One reading of everything `index` sizes itself from.
#[derive(Debug, Clone)]
pub struct Report {
    /// The `--db` path; the volume reported is the one holding it.
    pub db: PathBuf,
    pub sizing: Sizing,
    pub disk: Option<DiskSample>,
    pub disk_source: &'static str,
    /// The free space `index` would keep on that volume.
    pub disk_min_free: u64,
}

impl Report {
    /// Probe this machine as `index --db <db>` would with `spec` and
    /// `min_free`.
    pub fn detect(db: &Path, spec: Option<MemorySpec>, min_free: MinFree) -> Self {
        let disk = diskinfo::sample_disk(db);
        Self {
            db: db.to_path_buf(),
            sizing: Sizing::detect(0, spec, 1),
            disk,
            disk_source: diskinfo::DISK_SOURCE,
            disk_min_free: min_free.resolve(disk.map(|d| d.total)),
        }
    }

    /// The human-readable lines.
    pub fn text(&self) -> String {
        let mb = diskinfo::mb;
        let s = &self.sizing;
        let mut out = format!("cpus: {} (parse threads {})\n", s.cpus, s.parse_threads);
        match &s.memory {
            Ok(m) => out.push_str(&format!(
                "memory: {} total, {} free{}{}  [{}]\n",
                mb(m.total),
                mb(m.available),
                m.rss
                    .map_or(String::new(), |r| format!(", this process {}", mb(r))),
                m.psi_some_avg10
                    .map_or(String::new(), |p| format!(", PSI some avg10 {p}%")),
                m.source
            )),
            Err(e) => out.push_str(&format!("memory: unknown ({e})\n")),
        }
        out.push_str(&format!(
            "budget: {} of source (≈{} in memory): {}\n",
            mb(s.memory_budget),
            mb((s.memory_budget as f64 * s.policy.expansion) as u64),
            s.policy.reason
        ));
        let dir = self.db.display();
        match &self.disk {
            Some(d) => out.push_str(&format!(
                "disk ({dir}): {} total, {} free, reserve {}  [{}]\n",
                mb(d.total),
                mb(d.available),
                mb(self.disk_min_free),
                self.disk_source
            )),
            None => out.push_str(&format!(
                "disk ({dir}): unknown ({}); reserve {}\n",
                if self.disk_source == "none" {
                    format!("unsupported platform: {}", std::env::consts::OS)
                } else {
                    format!("{} failed for this path", self.disk_source)
                },
                mb(self.disk_min_free)
            )),
        }
        out
    }

    /// The same as an object (`--json`).
    pub fn json(&self) -> serde_json::Value {
        let s = &self.sizing;
        let m = s.memory.as_ref();
        serde_json::json!({
            "cpus": s.cpus,
            "parse_threads": s.parse_threads,
            "memory": {
                "total": m.ok().map(|m| m.total),
                "available": m.ok().map(|m| m.available),
                "rss": m.ok().and_then(|m| m.rss),
                "psi_some_avg10": m.ok().and_then(|m| m.psi_some_avg10),
                "source": m.ok().map(|m| m.source),
                "error": m.err(),
            },
            "budget": {
                "bytes": s.memory_budget,
                "in_memory": (s.memory_budget as f64 * s.policy.expansion) as u64,
                "expansion": s.policy.expansion,
                "reason": s.policy.reason,
            },
            "disk": {
                "path": self.db.display().to_string(),
                "total": self.disk.map(|d| d.total),
                "available": self.disk.map(|d| d.available),
                "min_free": self.disk_min_free,
                "source": self.disk.map(|_| self.disk_source),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysinfo::MemSample;

    fn report(memory: Result<MemSample, String>, disk: Option<DiskSample>) -> Report {
        Report {
            db: PathBuf::from("g.redb"),
            sizing: Sizing::new(8, memory, 0, None, 1),
            disk,
            disk_source: "test disk",
            disk_min_free: 2 << 30,
        }
    }

    #[test]
    fn report_names_sources_or_causes() {
        let r = report(
            Ok(MemSample {
                total: 64 << 30,
                available: 32 << 30,
                rss: Some(40 << 20),
                psi_some_avg10: None,
                source: "test probe",
            }),
            Some(DiskSample {
                total: 100 << 30,
                available: 50 << 30,
            }),
        );
        let t = r.text();
        assert!(t.starts_with("cpus: 8 (parse threads 7)\n"), "{t}");
        assert!(
            t.contains("memory: 64.0 GB total, 32.0 GB free, this process 40 MB  [test probe]\n"),
            "{t}"
        );
        assert!(
            t.contains("budget: ") && t.contains("70% of 32.0 GB free"),
            "{t}"
        );
        assert!(
            t.contains(
                "disk (g.redb): 100.0 GB total, 50.0 GB free, reserve 2.0 GB  [test disk]\n"
            ),
            "{t}"
        );
        let j = r.json();
        assert_eq!(j["memory"]["total"], 64u64 << 30);
        assert_eq!(j["memory"]["source"], "test probe");
        assert_eq!(j["memory"]["error"], serde_json::Value::Null);
        assert_eq!(j["disk"]["source"], "test disk");
        assert!(j["budget"]["bytes"].as_u64().unwrap() > 0);

        let r = report(Err("/proc/meminfo: No such file or directory".into()), None);
        let t = r.text();
        assert!(
            t.contains("memory: unknown (/proc/meminfo: No such file or directory)\n"),
            "{t}"
        );
        assert!(
            t.contains(
                "free RAM unknown (/proc/meminfo: No such file or directory); assuming 512 MB"
            ),
            "{t}"
        );
        assert!(
            t.contains("disk (g.redb): unknown (test disk failed for this path); reserve 2.0 GB"),
            "{t}"
        );
        let j = r.json();
        assert_eq!(j["memory"]["total"], serde_json::Value::Null);
        assert_eq!(
            j["memory"]["error"],
            "/proc/meminfo: No such file or directory"
        );
        assert_eq!(j["disk"]["source"], serde_json::Value::Null);
    }
}
