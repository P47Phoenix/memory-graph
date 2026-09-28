//! Where a command's store lives (ADR 0004 D3/D4): a local file opened
//! embedded, or a `memory-graph serve` reached over gRPC. One resolver
//! ([`resolve`]) turns `--db`, `--server` / `MEMORY_GRAPH_SERVER` and
//! `--read` / `MEMORY_GRAPH_READ` into a [`Target`]; one opener per kind
//! ([`open_embedded`], [`connect`]) is used by every command, so the error
//! messages and exit codes are the same everywhere.
use anyhow::{anyhow, bail, Result};
use graph_client::{ClientConfig, ReadMode, RemoteStore};
use graph_store::StoreError;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Server address when `--server` is not given.
pub const ENV_SERVER: &str = "MEMORY_GRAPH_SERVER";
/// Read mode when `--read` is not given (`local` or `linearizable`).
pub const ENV_READ: &str = "MEMORY_GRAPH_READ";
/// How long an embedded open keeps retrying a locked file, in
/// milliseconds (default 5000). Tests set it low.
pub const ENV_LOCK_WAIT_MS: &str = "MEMORY_GRAPH_LOCK_WAIT_MS";
/// The database file when neither `--db` nor a server is given.
pub const DEFAULT_DB: &str = "./graph.redb";
/// Default retry window for a locked database file.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Process exit codes beyond 0 (success) and 1 (any other failure).
pub mod exit {
    /// `health`: the server (or `--ready`: a leader) is not serving.
    pub const NOT_SERVING: i32 = 1;
    /// `cluster leader`: no leader is known.
    pub const NO_LEADER: i32 = 3;
    /// A write (or linearizable read) found no leader within its deadline.
    pub const WRITE_DEADLINE: i32 = 4;
    /// The server speaks another protocol or store format version.
    pub const PROTOCOL: i32 = 5;
}

/// An error that carries its own exit code.
#[derive(Debug)]
pub struct Exit {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Exit {}

/// The exit code for a failed command: an [`Exit`] anywhere in the chain
/// wins; then `NoLeader` (4) and `Protocol` (5) store errors; else 1.
pub fn exit_code(e: &anyhow::Error) -> i32 {
    if let Some(x) = e.chain().find_map(|c| c.downcast_ref::<Exit>()) {
        return x.code;
    }
    match e.chain().find_map(|c| c.downcast_ref::<StoreError>()) {
        Some(StoreError::NoLeader { .. }) => exit::WRITE_DEADLINE,
        Some(StoreError::Protocol(_)) => exit::PROTOCOL,
        _ => 1,
    }
}

/// Where the store is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A local file, opened in this process.
    Embedded(PathBuf),
    /// A `memory-graph serve` at `host:port`.
    Remote { addr: String, read: ReadMode },
}

/// The raw inputs [`resolve`] decides from (flags as parsed, environment
/// as read), so the decision is testable without touching the process
/// environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct TargetArgs<'a> {
    pub db: Option<&'a Path>,
    pub server_flag: Option<&'a str>,
    pub server_env: Option<&'a str>,
    pub read_flag: Option<ReadMode>,
    pub read_env: Option<&'a str>,
}

impl<'a> TargetArgs<'a> {
    /// The environment variables as the process sees them.
    pub fn env() -> (Option<String>, Option<String>) {
        let get = |k| {
            std::env::var(k)
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        (get(ENV_SERVER), get(ENV_READ))
    }
}

/// Decide the target. `--db` together with a server (from `--server` or
/// `MEMORY_GRAPH_SERVER`) is an error that names both: the env var is not a
/// clap flag, so clap's own conflict check cannot see it. `--server` beats
/// the env var. `--read` without a server is an error; `MEMORY_GRAPH_READ`
/// without one is ignored (it is meant to be set once for a shell).
pub fn resolve(a: TargetArgs<'_>) -> Result<Target> {
    let env_server = a.server_env.map(str::trim).filter(|s| !s.is_empty());
    let server_flag = a.server_flag.map(str::trim);
    if server_flag == Some("") {
        bail!("--server must not be empty (give host:port)");
    }
    match (a.db, server_flag, env_server) {
        (Some(db), Some(s), _) => bail!(
            "--db `{}` and --server {s} were both given: use one (a local database file, or a server)",
            db.display()
        ),
        (Some(db), None, Some(s)) => bail!(
            "--db `{}` and {ENV_SERVER}={s} were both given: use one (unset {ENV_SERVER} to open the file, or drop --db)",
            db.display()
        ),
        (None, Some(s), _) | (None, None, Some(s)) => {
            let read = match (a.read_flag, a.read_env) {
                (Some(r), _) => r,
                (None, Some(v)) if !v.trim().is_empty() => v
                    .trim()
                    .parse::<ReadMode>()
                    .map_err(|e| anyhow!("{ENV_READ}: {e}"))?,
                _ => ReadMode::Local,
            };
            Ok(Target::Remote {
                addr: s.to_string(),
                read,
            })
        }
        (db, None, None) => {
            if a.read_flag.is_some() {
                bail!("--read applies only with --server (or {ENV_SERVER})");
            }
            Ok(Target::Embedded(
                db.map_or_else(|| PathBuf::from(DEFAULT_DB), Path::to_path_buf),
            ))
        }
    }
}

/// `--read` values.
pub fn parse_read_mode(s: &str) -> std::result::Result<ReadMode, String> {
    s.parse()
}

/// The retry window for a locked file: `MEMORY_GRAPH_LOCK_WAIT_MS`, or 5 s.
pub fn lock_wait() -> Duration {
    std::env::var(ENV_LOCK_WAIT_MS)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_LOCK_WAIT, Duration::from_millis)
}

/// Back-off before retry `attempt`: between half and all of
/// `min(1 s, 50 ms * 2^attempt)`.
fn backoff(attempt: u32) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let cap = Duration::from_millis(50)
        .saturating_mul(1u32 << attempt.min(10))
        .min(Duration::from_secs(1));
    let half = cap / 2;
    let span = (cap - half).as_millis().max(1) as u64;
    // A per-call random seed from std (no rand dependency here).
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    half + Duration::from_millis(r % (span + 1))
}

/// Run `open` (an embedded open of `db`), retrying while the file is
/// locked by another process, with jittered back-off (base 50 ms, cap 1 s)
/// for up to [`lock_wait`]; then fail with [`locked_error`].
pub fn open_embedded<T>(db: &Path, mut open: impl FnMut() -> Result<T, StoreError>) -> Result<T> {
    let wait = lock_wait();
    let start = Instant::now();
    let mut attempt = 0u32;
    loop {
        match open() {
            Err(StoreError::Locked(why)) => {
                let spent = start.elapsed();
                if spent >= wait {
                    return Err(locked_error(db, &why));
                }
                std::thread::sleep(backoff(attempt).min(wait - spent));
                attempt += 1;
            }
            r => return r.map_err(anyhow::Error::from),
        }
    }
}

/// The message for a database file another process holds: names the
/// `memory-graph serve` from the `<db>.LOCK` sidecar when its pid is alive,
/// else says what else could hold it.
pub fn locked_error(db: &Path, why: &str) -> anyhow::Error {
    match graph_server::lock::holder(db) {
        Some((info, true)) => {
            let addr = connectable(&info.listen);
            anyhow!(
                "database {} is locked by pid {} (memory-graph serve on {}); use --server {addr} or stop it",
                db.display(),
                info.pid,
                info.listen
            )
        }
        _ => anyhow!(
            "database {} is locked by another process ({why}): another memory-graph command is using it, \
             or a `memory-graph serve` owns it (then use --server <host:port> instead)",
            db.display()
        ),
    }
}

/// A listen address a local client can dial: an unspecified bind address
/// (`0.0.0.0:7000`, `[::]:7000`) becomes loopback.
fn connectable(listen: &str) -> String {
    match listen.parse::<std::net::SocketAddr>() {
        Ok(a) if a.ip().is_unspecified() => {
            let ip = if a.is_ipv4() { "127.0.0.1" } else { "[::1]" };
            format!("{ip}:{}", a.port())
        }
        _ => listen.to_string(),
    }
}

/// Connect to a server. A protocol or store-format mismatch is exit code 5.
pub fn connect(addr: &str, read: ReadMode) -> Result<RemoteStore> {
    connect_with(addr, read, None)
}

/// [`connect`] with a custom retry budget for the first `Hello` (e.g. a
/// short one for `health`).
pub fn connect_with(addr: &str, read: ReadMode, budget: Option<Duration>) -> Result<RemoteStore> {
    let mut cfg = ClientConfig::new(addr);
    cfg.read_mode = read;
    if let Some(b) = budget {
        cfg.retry.budget = b;
    }
    RemoteStore::connect(cfg).map_err(|e| match e {
        StoreError::Protocol(_) | StoreError::SchemaMismatch { .. } => anyhow::Error::new(Exit {
            code: exit::PROTOCOL,
            message: format!("server {addr} is not compatible with this client: {e}"),
        }),
        e => anyhow::Error::new(e).context(format!("cannot connect to server {addr}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>() -> TargetArgs<'a> {
        TargetArgs::default()
    }

    #[test]
    fn neither_is_the_default_file() {
        assert_eq!(
            resolve(args()).unwrap(),
            Target::Embedded(PathBuf::from(DEFAULT_DB))
        );
        let db = Path::new("x.redb");
        assert_eq!(
            resolve(TargetArgs {
                db: Some(db),
                ..args()
            })
            .unwrap(),
            Target::Embedded(db.into())
        );
    }

    #[test]
    fn both_given_is_an_error_naming_both() {
        let db = Path::new("x.redb");
        let e = resolve(TargetArgs {
            db: Some(db),
            server_flag: Some("h:1"),
            ..args()
        })
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("--db `x.redb`") && e.contains("--server h:1"),
            "{e}"
        );
        let e = resolve(TargetArgs {
            db: Some(db),
            server_env: Some("h:2"),
            ..args()
        })
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("--db `x.redb`") && e.contains("MEMORY_GRAPH_SERVER=h:2"),
            "{e}"
        );
    }

    #[test]
    fn server_from_flag_or_env_flag_wins() {
        let t = resolve(TargetArgs {
            server_env: Some("env:1"),
            ..args()
        })
        .unwrap();
        assert_eq!(
            t,
            Target::Remote {
                addr: "env:1".into(),
                read: ReadMode::Local
            }
        );
        let t = resolve(TargetArgs {
            server_flag: Some("flag:1"),
            server_env: Some("env:1"),
            read_env: Some("linearizable"),
            ..args()
        })
        .unwrap();
        assert_eq!(
            t,
            Target::Remote {
                addr: "flag:1".into(),
                read: ReadMode::Linearizable
            }
        );
        // An empty env var is unset.
        assert!(matches!(
            resolve(TargetArgs {
                server_env: Some("  "),
                ..args()
            })
            .unwrap(),
            Target::Embedded(_)
        ));
        assert!(resolve(TargetArgs {
            server_flag: Some(""),
            ..args()
        })
        .is_err());
    }

    #[test]
    fn read_mode_rules() {
        // --read beats the env var.
        let t = resolve(TargetArgs {
            server_flag: Some("h:1"),
            read_flag: Some(ReadMode::Local),
            read_env: Some("linearizable"),
            ..args()
        })
        .unwrap();
        assert!(matches!(
            t,
            Target::Remote {
                read: ReadMode::Local,
                ..
            }
        ));
        // A bad env value is an error naming the variable.
        let e = resolve(TargetArgs {
            server_flag: Some("h:1"),
            read_env: Some("eventual"),
            ..args()
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains(ENV_READ), "{e}");
        // --read without a server is refused; the env var alone is ignored.
        assert!(resolve(TargetArgs {
            read_flag: Some(ReadMode::Linearizable),
            ..args()
        })
        .is_err());
        assert!(resolve(TargetArgs {
            read_env: Some("linearizable"),
            ..args()
        })
        .is_ok());
    }

    #[test]
    fn exit_codes_map_store_errors() {
        let e = anyhow::Error::from(StoreError::NoLeader { retry_after_ms: 5 });
        assert_eq!(exit_code(&e), exit::WRITE_DEADLINE);
        let e = anyhow::Error::from(StoreError::NoLeader { retry_after_ms: 5 })
            .context("database error while indexing");
        assert_eq!(exit_code(&e), exit::WRITE_DEADLINE);
        let e = anyhow::Error::from(StoreError::Protocol("v2".into()));
        assert_eq!(exit_code(&e), exit::PROTOCOL);
        let e = anyhow::Error::new(Exit {
            code: 3,
            message: "x".into(),
        });
        assert_eq!(exit_code(&e), 3);
        assert_eq!(exit_code(&anyhow!("plain")), 1);
        assert_eq!(
            exit_code(&anyhow::Error::from(StoreError::Locked("x".into()))),
            1
        );
    }

    #[test]
    fn backoff_is_bounded() {
        for a in 0..40 {
            let d = backoff(a);
            assert!(
                d >= Duration::from_millis(25) && d <= Duration::from_secs(1),
                "{d:?}"
            );
        }
    }

    #[test]
    fn unspecified_listen_is_dialled_on_loopback() {
        assert_eq!(connectable("0.0.0.0:7000"), "127.0.0.1:7000");
        assert_eq!(connectable("[::]:7000"), "[::1]:7000");
        assert_eq!(connectable("10.0.0.5:7000"), "10.0.0.5:7000");
        assert_eq!(connectable("host:7000"), "host:7000");
    }

    #[test]
    fn locked_retry_gives_up_with_a_generic_message_without_a_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        let mut n = 0;
        // Env var not set in unit tests: use a closure that is locked twice
        // then opens, so the retry path is exercised without waiting 5 s.
        let r = open_embedded(&db, || {
            n += 1;
            if n < 3 {
                Err(StoreError::Locked("already open".into()))
            } else {
                Ok(n)
            }
        })
        .unwrap();
        assert_eq!(r, 3);
        let msg = locked_error(&db, "already open").to_string();
        assert!(
            msg.contains("memory-graph serve") && msg.contains("--server"),
            "{msg}"
        );
        // Other errors are not retried.
        let mut calls = 0;
        let e = open_embedded(&db, || -> Result<(), StoreError> {
            calls += 1;
            Err(StoreError::Corrupt("x".into()))
        });
        assert!(e.is_err());
        assert_eq!(calls, 1);
    }
}
