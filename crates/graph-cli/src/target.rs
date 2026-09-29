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
/// The client write deadline when `--write-deadline` is not given
/// (`--write-deadline` also reads it).
pub const ENV_WRITE_DEADLINE: &str = "MEMORY_GRAPH_WRITE_DEADLINE";
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
    /// A write (or linearizable read) found no leader within its deadline
    /// (`--write-deadline`; `--read-deadline` for a read).
    pub const WRITE_DEADLINE: i32 = 4;
    /// The server speaks another protocol or store format version.
    pub const PROTOCOL: i32 = 5;
    /// A data directory (or a node named in a membership change) belongs to
    /// another cluster (`WrongCluster`): `serve --join` a peer of another
    /// cluster, say.
    pub const WRONG_CLUSTER: i32 = 6;
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
/// wins; then `NoLeader` (4), `Protocol` (5) and `WrongCluster` (6) store
/// errors; else 1.
pub fn exit_code(e: &anyhow::Error) -> i32 {
    if let Some(x) = e.chain().find_map(|c| c.downcast_ref::<Exit>()) {
        return x.code;
    }
    match e.chain().find_map(|c| c.downcast_ref::<StoreError>()) {
        Some(StoreError::NoLeader { .. }) => exit::WRITE_DEADLINE,
        Some(StoreError::Protocol(_)) => exit::PROTOCOL,
        Some(StoreError::WrongCluster { .. }) => exit::WRONG_CLUSTER,
        _ => 1,
    }
}

/// Add what a remote exit-4 failure means to its message. A write
/// (`read = false`) never got an answer from a leader within the write
/// deadline, whether the server was unreachable, the connection was lost
/// mid-call, or no leader was elected. A read (`read = true`, a
/// linearizable one) found no leader within the read deadline and answered
/// nothing rather than possibly stale data. Other failures are returned
/// unchanged.
pub fn explain_remote_failure(
    e: anyhow::Error,
    addr: &str,
    code: i32,
    read: bool,
) -> anyhow::Error {
    if code != exit::WRITE_DEADLINE {
        return e;
    }
    if read {
        e.context(format!(
            "server {addr}: the linearizable read found no leader within the read deadline \
             (--read-deadline: an election, the node cut off from the majority, or the leader \
             unreachable); it answers nothing rather than possibly stale data. Retry later, or \
             use --read local for the node's own, possibly stale, answer"
        ))
    } else {
        e.context(format!(
            "server {addr}: the write was not acknowledged within the write deadline \
             (connection lost, server unreachable, or no leader); it may or may not have \
             been applied, and rerunning it is safe"
        ))
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
            Ok(v) => {
                // The open succeeded, so no server holds the file: a
                // sidecar left by a crashed server is stale.
                graph_server::lock::remove_stale(db);
                return Ok(v);
            }
            Err(e) => return Err(anyhow::Error::from(e)),
        }
    }
}

/// The message for a database file another process holds: names the
/// `memory-graph serve` from the `<db>.LOCK` sidecar when its pid is alive
/// **and** its listen address answers `Hello` (a short probe: on Windows a
/// dead server's pid can be reused by an unrelated process), else says
/// what else could hold it. A dead or unanswering holder is never named.
pub fn locked_error(db: &Path, why: &str) -> anyhow::Error {
    match graph_server::lock::holder(db) {
        Some((info, true)) if answers_hello(&connectable(&info.listen)) => {
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

/// How long [`answers_hello`] waits for a server.
const HOLDER_PROBE: Duration = Duration::from_millis(500);

/// Whether a memory-graph server answers `Hello` at `addr` (a server of
/// another protocol version counts: it is a server).
fn answers_hello(addr: &str) -> bool {
    let mut cfg = ClientConfig::new(addr);
    cfg.retry.budget = HOLDER_PROBE;
    cfg.connect_timeout = HOLDER_PROBE;
    match RemoteStore::connect(cfg) {
        Ok(_) => true,
        Err(StoreError::Protocol(_) | StoreError::SchemaMismatch { .. }) => true,
        Err(_) => false,
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

/// The `--read-deadline` of this process: the client's retry budget
/// (`ClientConfig::retry.budget`), which bounds every read and the first
/// `Hello`, applied to every [`connect`] that sets no budget of its own.
static READ_DEADLINE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Set the read deadline every later [`connect`] uses (once per process).
/// Unset, [`ClientConfig`]'s default retry budget (5 s).
pub fn set_read_deadline(d: Duration) {
    let _ = READ_DEADLINE.set(d);
}

/// The `--write-deadline` of this process, applied to every [`connect`].
static WRITE_DEADLINE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Set the write deadline every later [`connect`] uses (once per process;
/// a second call is ignored). Unset, [`ClientConfig`]'s default (10 s).
pub fn set_write_deadline(d: Duration) {
    let _ = WRITE_DEADLINE.set(d);
}

/// Parse a `--write-deadline`: `<n>ms`, `<n>s`, `<n>m`, or bare seconds;
/// positive.
pub fn parse_write_deadline(s: &str) -> std::result::Result<Duration, String> {
    let t = s.trim();
    let (num, unit_ms) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000)
    } else {
        (t, 1000)
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a duration (e.g. 500ms, 10s, 2m)"))?;
    if n == 0 {
        return Err("the write deadline must be positive".into());
    }
    Ok(Duration::from_millis(n.saturating_mul(unit_ms)))
}

/// Connect to a server. A protocol or store-format mismatch is exit code 5.
pub fn connect(addr: &str, read: ReadMode) -> Result<RemoteStore> {
    connect_with(addr, read, None)
}

/// [`connect`] with a custom retry budget for the first `Hello` (e.g. a
/// short one for `health`).
pub fn connect_with(addr: &str, read: ReadMode, budget: Option<Duration>) -> Result<RemoteStore> {
    let endpoints = endpoints(addr)?;
    let mut cfg = ClientConfig::new(endpoints[0].clone());
    cfg.endpoints = endpoints;
    cfg.read_mode = read;
    if let Some(d) = WRITE_DEADLINE.get() {
        cfg.write_deadline = *d;
    }
    if let Some(b) = budget.or_else(|| READ_DEADLINE.get().copied()) {
        cfg.retry.budget = b;
    }
    let store = RemoteStore::connect(cfg).map_err(|e| match e {
        StoreError::Protocol(_) | StoreError::SchemaMismatch { .. } => anyhow::Error::new(Exit {
            code: exit::PROTOCOL,
            message: format!("server {addr} is not compatible with this client: {e}"),
        }),
        e => anyhow::Error::new(e).context(format!("cannot connect to server {addr}")),
    })?;
    *READ_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store.read_log());
    Ok(store)
}

/// `--server` / `MEMORY_GRAPH_SERVER`: one `host:port`, or several
/// separated by commas (the client uses the first that answers and moves
/// on to the next when one is unreachable or has no leader).
pub fn endpoints(addr: &str) -> Result<Vec<String>> {
    let eps: Vec<String> = addr
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    if eps.is_empty() {
        bail!("--server `{addr}` names no endpoint (give host:port[,host:port...])");
    }
    Ok(eps)
}

/// The read log of the last server connection this process made.
static READ_LOG: std::sync::Mutex<Option<std::sync::Arc<graph_client::ReadLog>>> =
    std::sync::Mutex::new(None);

/// Whether a read of this process's server connection may have missed
/// acknowledged writes (ADR 0004 D8): `None` with no server (embedded
/// output is unchanged) or before any read.
pub fn stale_possible() -> Option<bool> {
    READ_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|l| l.stale_possible())
}

/// Add `"stale_possible"` to a JSON output document when talking to a
/// server (unchanged embedded).
pub fn with_read_meta(mut doc: serde_json::Value) -> serde_json::Value {
    if let (Some(stale), Some(o)) = (stale_possible(), doc.as_object_mut()) {
        o.insert("stale_possible".into(), serde_json::Value::Bool(stale));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_takes_a_comma_separated_endpoint_list() {
        assert_eq!(endpoints("h:1").unwrap(), ["h:1"]);
        assert_eq!(
            endpoints(" a:1 , b:2,,c:3 ").unwrap(),
            ["a:1", "b:2", "c:3"]
        );
        assert!(endpoints(" , ").is_err());
    }

    #[test]
    fn embedded_json_output_gets_no_stale_possible() {
        let doc = with_read_meta(serde_json::json!({ "repos": [] }));
        assert!(doc.get("stale_possible").is_none());
    }

    #[test]
    fn write_deadline_parses_units_and_refuses_zero() {
        assert_eq!(
            parse_write_deadline("500ms"),
            Ok(Duration::from_millis(500))
        );
        assert_eq!(parse_write_deadline("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_write_deadline("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_write_deadline(" 3 "), Ok(Duration::from_secs(3)));
        assert!(parse_write_deadline("0ms").is_err());
        assert!(parse_write_deadline("soon").is_err());
        assert!(parse_write_deadline("").is_err());
    }

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
        let e = anyhow::Error::from(StoreError::WrongCluster {
            expected: "a".into(),
            found: "b".into(),
        })
        .context("serving `dir`");
        assert_eq!(exit_code(&e), exit::WRONG_CLUSTER);
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
    fn a_remote_write_deadline_names_the_server_and_the_lost_connection() {
        let e = anyhow::Error::from(StoreError::NoLeader { retry_after_ms: 5 });
        let code = exit_code(&e);
        let e = explain_remote_failure(e, "h:7", code, false);
        assert_eq!(exit_code(&e), exit::WRITE_DEADLINE, "the code survives");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("server h:7") && msg.contains("connection lost"),
            "{msg}"
        );
        let e = explain_remote_failure(anyhow!("plain"), "h:7", 1, false);
        assert_eq!(e.to_string(), "plain");
        // A read gets a read's explanation, never the write's.
        let e = anyhow::Error::from(StoreError::NoLeader { retry_after_ms: 5 });
        let e = explain_remote_failure(e, "h:7", exit::WRITE_DEADLINE, true);
        assert_eq!(exit_code(&e), exit::WRITE_DEADLINE);
        let msg = format!("{e:#}");
        assert!(
            msg.contains("linearizable read found no leader")
                && msg.contains("--read-deadline")
                && !msg.contains("write"),
            "{msg}"
        );
    }

    fn write_sidecar(db: &Path, pid: u32, listen: &str) {
        let info = graph_server::lock::LockInfo {
            pid,
            listen: listen.into(),
            started: String::new(),
        };
        std::fs::write(
            graph_server::lock::lock_path(db),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
    }

    /// A pid that exited: a child spawned and waited for.
    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// An address nothing listens on (bound, then released).
    fn dead_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    }

    #[test]
    fn a_dead_holder_is_never_named_and_its_sidecar_goes_on_open() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        write_sidecar(&db, dead_pid(), "127.0.0.1:7000");
        let msg = locked_error(&db, "already open").to_string();
        assert!(
            msg.contains("locked by another process") && !msg.contains("pid"),
            "{msg}"
        );
        open_embedded(&db, || Ok::<_, StoreError>(())).unwrap();
        assert!(
            !graph_server::lock::lock_path(&db).exists(),
            "the stale sidecar is removed after a successful open"
        );
    }

    /// Windows pid reuse: the pid is alive (here: our own), but nothing
    /// answers at the sidecar's address, so it is not named.
    #[test]
    fn a_live_pid_whose_address_does_not_answer_is_not_named() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("g.redb");
        write_sidecar(&db, std::process::id(), &dead_addr());
        let msg = locked_error(&db, "already open").to_string();
        assert!(
            msg.contains("locked by another process") && !msg.contains("pid"),
            "{msg}"
        );
        // A live sidecar is never removed by an open.
        open_embedded(&db, || Ok::<_, StoreError>(())).unwrap();
        assert!(graph_server::lock::lock_path(&db).exists());
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
