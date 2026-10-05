//! The memory-graph client (ADR 0004 D2): [`RemoteStore`], a
//! `graph_store::Store` over a gRPC connection to `memory-graph serve`, so
//! the CLI and any embedder use a server exactly as they use a file.
//!
//! * Synchronous facade: the store owns a small tokio runtime and every
//!   trait method is `block_on` of one or a few RPCs. Calling it from
//!   inside a tokio runtime panics with a message that says so (see
//!   [`block_on`]); the CLI has no runtime of its own.
//! * [`connect`](RemoteStore::connect) does `Store.Hello` and refuses a
//!   server with another `PROTOCOL_VERSION`.
//! * Reads carry the configured [`ReadMode`] (`Local` or `Linearizable`);
//!   [`Store::snapshot`] opens a server-side handle and returns a
//!   [`RemoteSnapshot`] whose reads carry `View.snapshot_id`. A `search` /
//!   `search_symbols` without a limit pages to completion under a snapshot
//!   handle when the server applied its default limit, so the pages come
//!   from one frozen view.
//! * Every read answer carries a [`ReadMeta`] (the `mg-read-meta` response
//!   header: applied index, the leader's committed index as last seen,
//!   `stale_possible`); [`RemoteStore::read_log`] collects them.
//! * Several endpoints (`ClientConfig::endpoints`): the first that answers
//!   `Hello` is used; an unreachable or leaderless node moves the
//!   connection to the next one (a `NotLeader` naming the leader moves it
//!   there).
//! * Indexing ships raw source bytes: `prepare` returns
//!   `PreparedFile::remote`, `index_prepared` streams one `Write.Index`
//!   RPC (a header, then one `FileBytes` per file); the server parses.
//! * Retries: `UNAVAILABLE` is retried with jittered back-off within the
//!   [`RetryConfig`] budget (writes: [`ClientConfig::write_deadline`], and
//!   a write also retries a connection lost mid-call, since every write is
//!   idempotent on a retry); a `NotLeader` that names the leader switches
//!   the connection there (with back-off from the second switch in a
//!   row). A write that runs out of deadline fails with `NoLeader` (a
//!   leader that lost its quorum answers a pending write `NoLeader` too,
//!   after its `--quorum-loss-timeout`, so the deadline also bounds a
//!   write sent to a minority leader). `NoLeader` does not mean the write
//!   was not applied; retries are idempotent. A write refused
//!   *transiently* (a typed flag on the refusal, issue #225: `Admin.Remove`'s
//!   quorum guard right after a leader change) is resent until the write
//!   deadline, which then reports that refusal, not `NoLeader`. A read
//!   whose connection was lost fails with a `Storage` error naming the
//!   server. Every call carries the `mg-protocol-version` header.
//!
//! [`Store::snapshot`]: graph_store::Store::snapshot

mod conn;
mod reads;
mod remote;
mod snapshot;

pub use conn::{block_on, HelloInfo, ReadLog, QUICK_CONNECT_TIMEOUT};
pub use graph_proto::ReadMeta;
pub use remote::{RemoteStore, RemoveOutcome, ENCODING_STORE_FORMAT};
pub use snapshot::RemoteSnapshot;

pub use graph_proto::{PROTOCOL_VERSION, RAFT_ENTRY_MAX_BYTES};

use std::time::Duration;

/// How reads are served (ADR 0004 D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadMode {
    /// The node's own store as it is; may lag the leader.
    #[default]
    Local,
    /// Through the leader's read barrier: sees every acknowledged write.
    Linearizable,
}

impl std::str::FromStr for ReadMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "linearizable" => Ok(Self::Linearizable),
            other => Err(format!("unknown read mode `{other}` (local|linearizable)")),
        }
    }
}

/// Back-off for `UNAVAILABLE` answers: full jitter between half and all of
/// `min(cap, base * 2^attempt)`, until `budget` has elapsed since the
/// first attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    pub base: Duration,
    pub cap: Duration,
    pub budget: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(50),
            cap: Duration::from_secs(2),
            budget: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `host:port` of one or more nodes; the first that answers `Hello`
    /// is used, and a `NotLeader` moves the connection to the leader.
    pub endpoints: Vec<String>,
    pub read_mode: ReadMode,
    pub retry: RetryConfig,
    /// How long a write keeps retrying while no leader is known (or the
    /// server is unreachable, or the leader lost its quorum); past it the
    /// write fails with `NoLeader`. `NoLeader` does not mean the write was
    /// not applied; retries are idempotent.
    pub write_deadline: Duration,
    /// TCP connect timeout per attempt (default 5 s).
    pub connect_timeout: Duration,
    /// The deadline (`grpc-timeout`) of each membership-change attempt
    /// (`AddLearner`, `Promote`, `Remove`, `TransferLeader`; default 180 s),
    /// so a change that can never commit fails instead of hanging.
    pub admin_deadline: Duration,
    /// Reported in `Hello` for the server's logs.
    pub client_version: String,
}

impl ClientConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoints: vec![endpoint.into()],
            read_mode: ReadMode::Local,
            retry: RetryConfig::default(),
            write_deadline: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
            admin_deadline: Duration::from_secs(180),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_mode_parses() {
        assert_eq!("local".parse::<ReadMode>().unwrap(), ReadMode::Local);
        assert_eq!(
            "LINEARIZABLE".parse::<ReadMode>().unwrap(),
            ReadMode::Linearizable
        );
        assert!("eventual".parse::<ReadMode>().is_err());
    }

    /// The synchronous facade must never be driven from inside a runtime
    /// (it would deadlock a single-threaded one and starve a worker of a
    /// multi-threaded one): it panics with a message that says so.
    #[test]
    fn block_on_inside_a_runtime_panics() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let inner = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // Outside any runtime: fine.
        assert_eq!(block_on(&inner, async { 41 + 1 }), 42);
        // Inside one: refused.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async { block_on(&inner, async { 1 }) })
        }));
        let msg = r.unwrap_err();
        let msg = msg
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| msg.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            msg.contains("inside a tokio runtime"),
            "panic message names the cause: {msg:?}"
        );
    }
}
