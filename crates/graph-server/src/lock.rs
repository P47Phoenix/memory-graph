//! The `<db>.LOCK` sidecar (ADR 0004 D4): a small JSON file next to the
//! store naming the `serve` process that holds it (`pid`, `listen`,
//! `started`), so an embedded open that hits redb's `Locked` can tell the
//! user which server to point `--server` at. redb's own lock is what
//! enforces exclusivity; the sidecar is information only. A sidecar whose
//! pid is dead (a crashed server) is ignored and overwritten.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the sidecar holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    /// The listen address as bound (`127.0.0.1:7000`).
    pub listen: String,
    /// RFC 3339-ish UTC timestamp (seconds since the epoch, rendered), for
    /// humans; nothing parses it.
    pub started: String,
}

/// A written sidecar; removed on drop.
pub struct LockFile {
    path: PathBuf,
}

/// `<db>.LOCK` for a store at `db`.
pub fn lock_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".LOCK");
    PathBuf::from(p)
}

/// Read the sidecar next to `db`, if there is one and it parses.
pub fn read(db: &Path) -> Option<LockInfo> {
    let text = std::fs::read_to_string(lock_path(db)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read the sidecar and say whether its holder is still alive: `Some((info,
/// alive))`, or `None` when there is no readable sidecar.
pub fn holder(db: &Path) -> Option<(LockInfo, bool)> {
    let info = read(db)?;
    let alive = pid_alive(info.pid);
    Some((info, alive))
}

impl LockFile {
    /// Write the sidecar for the running server. A stale sidecar (its pid
    /// is dead) is overwritten silently; a live one is reported (redb's
    /// lock would already have refused the open, so this is belt and
    /// braces for a sidecar left by a server on another store copy).
    pub fn create(db: &Path, listen: &str) -> std::io::Result<LockFile> {
        let path = lock_path(db);
        if let Some((info, true)) = holder(db) {
            if info.pid != std::process::id() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "`{}` names a live memory-graph serve (pid {}, {})",
                        path.display(),
                        info.pid,
                        info.listen
                    ),
                ));
            }
        }
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        let info = LockInfo {
            pid: std::process::id(),
            listen: listen.to_string(),
            started,
        };
        let text = serde_json::to_string_pretty(&info).expect("LockInfo serializes");
        std::fs::write(&path, text)?;
        Ok(LockFile { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the sidecar now (also done on drop).
    pub fn remove(self) {
        drop(self);
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a process with this pid exists (`kill(pid, 0)` on unix,
/// `OpenProcess` on Windows). A pid we cannot query (permission) counts as
/// alive: better to name a possibly-dead holder than to overwrite a live
/// one.
pub fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 only checks for existence.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if r == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: OpenProcess with a query-only right; the handle is
        // closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trips_and_is_removed_on_drop() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let lock = LockFile::create(&db, "127.0.0.1:1").unwrap();
        let (info, alive) = holder(&db).unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.listen, "127.0.0.1:1");
        assert!(alive);
        drop(lock);
        assert!(!lock_path(&db).exists());
    }

    #[test]
    fn stale_sidecar_is_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        // A pid no live process is likely to have.
        let stale = LockInfo {
            pid: u32::MAX - 7,
            listen: "old:1".into(),
            started: String::new(),
        };
        std::fs::write(lock_path(&db), serde_json::to_string(&stale).unwrap()).unwrap();
        let lock = LockFile::create(&db, "new:2").unwrap();
        assert_eq!(read(&db).unwrap().listen, "new:2");
        drop(lock);
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
    }
}
