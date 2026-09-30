//! Snapshot backups (ADR 0006, epic story 35): the leader copies each
//! snapshot it builds to a backup location, keeps the newest few there, and
//! a new cluster restores straight from that location with verification.
//!
//! * [`BackupSink`]: the internal object-store trait (`put`, `get`, `list`,
//!   `delete`) with [`file::FileSink`] (`file://<dir>`) and [`s3::S3Sink`]
//!   (`s3://bucket/prefix` over plain HTTP with SigV4, story 36).
//! * [`uploader::Backup`]: the async, best-effort uploader: one upload at a
//!   time on its own thread, a newer snapshot supersedes a queued one, three
//!   retries, then a log line, `mg_backup_failures_total` and
//!   `last_backup_error`. It never blocks or delays a snapshot build or a
//!   log purge (the build only hands it a job).
//! * [`restore`]: `--restore file://.../snap-T-I.redb`, `s3://.../snap-T-I.redb`
//!   or `.../latest`, the `.meta` check of a plain-path `--restore`, and the
//!   listing behind `cluster backups` (story 37).
//! * [`Backup::upload_now`]: `cluster snapshot --upload` (story 37).
//!
//! Object layout (E7): `<prefix>/<cluster_id>/snap-T-I.redb` plus
//! `snap-T-I.meta` (the unchanged snapshot sidecar JSON). The data object
//! is written first and the `.meta` last: a backup exists only once its
//! `.meta` does.
//!
//! The local snapshot being replaced mid-upload (the ADR's open question for
//! S1) is settled by an open handle, not a `<snapshot>.backup.tmp` copy:
//! the uploader opens the snapshot file once and streams from that handle.
//! Rust's `File::open` on Windows asks for `FILE_SHARE_READ | WRITE |
//! DELETE`, so (on NTFS with POSIX delete semantics, Windows 10 1809 and later) a newer build can still remove the old pair while it is
//! read (as on Unix, where an unlinked file stays readable through an open
//! handle), and a copy would cost a second snapshot's worth of disk that
//! the disk guard does not budget for. The bytes read are hashed on the way
//! and compared with the sidecar's sha256 before the `.meta` is written, so
//! a file that changed under the handle can never be committed. A snapshot
//! already removed before the open is simply superseded by the newer one.
pub mod creds;
pub mod file;
pub mod restore;
pub mod s3;
pub mod sigv4;
pub mod uploader;

pub use file::FileSink;
pub use s3::{S3Options, S3Sink};
pub use uploader::{Backup, BackupStats, Uploaded};

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// One object in a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The key, `/`-separated, relative to the sink's root.
    pub key: String,
    pub size: u64,
    /// Last modified (the orphan sweep's age).
    pub modified: SystemTime,
}

/// A backup location: an object store keyed by `/`-separated names.
///
/// A `put` is atomic from a reader's point of view: an interrupted put
/// leaves no object under `key` (a sink may leave a temporary it names
/// otherwise, which the orphan sweep removes).
pub trait BackupSink: Send + Sync {
    /// Store everything `src` yields as `key` (replacing it); the bytes
    /// written.
    fn put(&self, key: &str, src: &mut dyn Read) -> std::io::Result<u64>;
    /// Copy `key` into `dst`; the bytes copied. `NotFound` if absent.
    fn get(&self, key: &str, dst: &mut dyn Write) -> std::io::Result<u64>;
    /// Every object whose key starts with `prefix`.
    fn list(&self, prefix: &str) -> std::io::Result<Vec<ObjectInfo>>;
    /// Remove `key`; an absent key is not an error.
    fn delete(&self, key: &str) -> std::io::Result<()>;
    /// Where it writes, for logs (never a secret).
    fn describe(&self) -> String;
    /// The URL of `key` that `--restore` takes (never a secret).
    fn url_of(&self, key: &str) -> String {
        format!("{}/{key}", self.describe())
    }
    /// Clean up unfinished uploads under `prefix` begun before
    /// `older_than` (S3: abort stale multipart uploads); how many. A sink
    /// whose interrupted puts leave only listable objects (`file://`)
    /// needs nothing here: the orphan sweep removes those.
    fn sweep_incomplete(&self, _prefix: &str, _older_than: SystemTime) -> std::io::Result<usize> {
        Ok(0)
    }
}

/// `--backup-on`: which nodes upload the snapshots they build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupOn {
    /// Only the node that is the leader when the build completes (E2).
    #[default]
    Leader,
    /// Every node. The object keys carry no node id, so nodes that share a
    /// `--backup-url` may overwrite each other's copy of the same (term,
    /// index); a restore verifies the pair and refuses a mixed one. Give
    /// each node its own `--backup-url` with `all`.
    All,
    /// No uploads (restores still work).
    None,
}

impl std::str::FromStr for BackupOn {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "leader" => Ok(Self::Leader),
            "all" => Ok(Self::All),
            "none" => Ok(Self::None),
            other => Err(format!("`{other}`: expected leader, all or none")),
        }
    }
}

impl std::fmt::Display for BackupOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Leader => "leader",
            Self::All => "all",
            Self::None => "none",
        })
    }
}

/// Default `--backup-keep`.
pub const DEFAULT_KEEP: usize = 7;

/// Data objects without a `.meta` (and a sink's leftover temporaries) older
/// than this are swept (E9).
pub const ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Attempts after the first that an upload gets before it counts as failed.
pub const UPLOAD_RETRIES: u32 = 3;

/// The backup settings of a server (`--backup-*`).
#[derive(Clone)]
pub struct BackupConfig {
    /// `--backup-url` (`file://<dir>` or `s3://bucket/prefix`).
    pub url: String,
    /// `--backup-keep`: committed backups kept (0 = all).
    pub keep: usize,
    /// `--backup-on`.
    pub on: BackupOn,
    /// The first retry's delay; each further one doubles it.
    pub retry_backoff: Duration,
    /// [`ORPHAN_AGE`] (tests shorten it).
    pub orphan_age: Duration,
    /// The `s3://` settings (`--backup-endpoint`, `--backup-region`,
    /// `--backup-virtual-host`, `--backup-credentials-file`,
    /// `--backup-profile`); refused with a `file://` URL unless default.
    pub s3: S3Options,
    /// Tests: this sink instead of the one `url` names.
    pub sink: Option<Arc<dyn BackupSink>>,
}

impl BackupConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            keep: DEFAULT_KEEP,
            on: BackupOn::Leader,
            retry_backoff: Duration::from_secs(1),
            orphan_age: ORPHAN_AGE,
            s3: S3Options::default(),
            sink: None,
        }
    }

    /// The sink this configuration writes to.
    pub fn open_sink(&self) -> Result<Arc<dyn BackupSink>, String> {
        if let Some(s) = &self.sink {
            return Ok(Arc::clone(s));
        }
        match parse_url(&self.url)? {
            Location::File(dir) => {
                let d = S3Options::default();
                let s = &self.s3;
                if s.endpoint.is_some()
                    || s.region != d.region
                    || s.virtual_host
                    || s.credentials_file.is_some()
                    || s.profile.is_some()
                {
                    return Err(format!(
                        "`{}`: --backup-endpoint, --backup-region, --backup-virtual-host, \
                         --backup-credentials-file and --backup-profile apply to s3:// only",
                        self.url
                    ));
                }
                Ok(Arc::new(FileSink::new(dir)))
            }
            Location::S3(_) => Ok(Arc::new(S3Sink::new(&self.url, self.s3.clone())?)),
        }
    }
}

impl std::fmt::Debug for BackupConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupConfig")
            .field("url", &self.url)
            .field("keep", &self.keep)
            .field("on", &self.on)
            .field("retry_backoff", &self.retry_backoff)
            .field("orphan_age", &self.orphan_age)
            .field("s3", &self.s3)
            .field("sink", &self.sink.as_ref().map(|s| s.describe()))
            .finish()
    }
}

/// Run `f` on a plain thread of its own and await it: the sinks are
/// synchronous, and the `s3://` one refuses a thread inside a tokio
/// runtime's context (which `spawn_blocking` threads are).
pub async fn off_runtime<T: Send + 'static>(
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("backup-call".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| format!("{what}: starting a thread: {e}"))?;
    rx.await.map_err(|_| format!("{what}: the thread panicked"))
}

/// A parsed backup URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    File(std::path::PathBuf),
    S3(s3::S3Url),
}

/// Whether `s` names a backup URL (rather than a plain path).
pub fn is_url(s: &str) -> bool {
    s.contains("://")
}

/// Parse `file://<path>` (`file:///srv/b`, `file:///C:/b` or `file://C:/b`
/// on Windows, or a relative `file://backups`) or `s3://bucket[/prefix]`;
/// anything else is refused.
pub fn parse_url(url: &str) -> Result<Location, String> {
    if let Some(rest) = url.strip_prefix("file://") {
        // `file:///C:/x`: drop the slash before a drive letter.
        let b = rest.as_bytes();
        let rest = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            &rest[1..]
        } else {
            rest
        };
        if rest.is_empty() {
            return Err(format!("`{url}`: file:// needs a directory"));
        }
        return Ok(Location::File(std::path::PathBuf::from(rest)));
    }
    if url.starts_with("s3://") {
        return s3::parse_s3_url(url).map(Location::S3);
    }
    Err(format!(
        "`{url}`: not a supported backup URL (expected file://<dir> or s3://bucket/prefix)"
    ))
}

/// `snap-<term>-<index>` of a key or file name ending `.redb` or `.meta`.
pub fn parse_stem(name: &str) -> Option<(u64, u64)> {
    let base = name.rsplit('/').next()?;
    let stem = base
        .strip_suffix(".meta")
        .or_else(|| base.strip_suffix(".redb"))?;
    let rest = stem.strip_prefix("snap-")?;
    let (t, i) = rest.split_once('-')?;
    Some((t.parse().ok()?, i.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse() {
        assert_eq!(
            parse_url("file:///srv/b").unwrap(),
            Location::File("/srv/b".into())
        );
        assert_eq!(
            parse_url("file:///C:/b").unwrap(),
            Location::File("C:/b".into())
        );
        assert_eq!(
            parse_url("file://rel/dir").unwrap(),
            Location::File("rel/dir".into())
        );
        assert!(parse_url("file://").is_err());
        assert_eq!(
            parse_url("s3://bucket/x").unwrap(),
            Location::S3(s3::S3Url {
                bucket: "bucket".into(),
                prefix: "x".into()
            })
        );
        assert!(parse_url("s3://B").is_err());
        assert!(parse_url("http://x").is_err());
        assert!(is_url("file:///x") && !is_url("/x/snap-1-2.redb"));
    }

    #[test]
    fn stems_parse() {
        assert_eq!(parse_stem("c/snap-3-120.meta"), Some((3, 120)));
        assert_eq!(parse_stem("snap-3-120.redb"), Some((3, 120)));
        assert_eq!(parse_stem("c/snap-3-120.redb.part-1"), None);
        assert_eq!(parse_stem("c/other.meta"), None);
        assert_eq!("all".parse::<BackupOn>().unwrap(), BackupOn::All);
        assert!("most".parse::<BackupOn>().is_err());
    }
}
