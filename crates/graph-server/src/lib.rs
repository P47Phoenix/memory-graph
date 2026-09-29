//! The memory-graph server (ADR 0004, stages A and B): the
//! `memory_graph.v1` services (`Store`, `Write`, `Admin`, `Raft`,
//! `grpc.health.v1`) over one `V2Store` that an openraft node replicates.
//!
//! Layout:
//! * [`ServeConfig`] / [`run_blocking`] / [`start`]: configuration, the
//!   blocking entry point the CLI's `serve` calls (own multi-thread runtime,
//!   Ctrl-C / SIGTERM, graceful shutdown), and the async one tests use.
//! * [`paths`]: [`NodePaths`] (`--data-dir` or `--db` layout), `node.json`
//!   ([`NodeJson`]), [`InitMode`] (bootstrap, restart, uninitialized) and
//!   the cluster id a node knows.
//! * [`slot`]: [`StoreSlot`], the store behind a read/write lock so a
//!   compact or snapshot install can swap the file under a write lock while
//!   every read and apply holds a read lock; it also owns the snapshot
//!   handle table clients page through.
//! * [`raft`]: the openraft integration, isolated so API churn stays in one
//!   place: `types` (the `RaftTypeConfig`, the log entry and its response),
//!   `log_store` (`RedbLogStore`, the Raft log), `state_machine`
//!   (`StoreStateMachine`, applying `LogCommand`s through the store's marked
//!   writes), `snapshot_dir` (snapshot files), `network` (`GrpcNetwork` over
//!   the peers' `Raft` service, and the test fault plan), `wire` (openraft
//!   RPC types on the wire) and `node` (start, initialize, snapshot policy,
//!   membership, leader info).
//! * [`services`]: the four tonic services (`raft` is the peers' side).
//! * [`forward`]: a follower forwards writes, membership changes and the
//!   linearizable read barrier to the leader (stage C, ADR 0004 D8/D9).
//! * [`observe`]: metrics (Prometheus text, `Admin.Metrics`), the per-RPC
//!   tracing/metrics layer and the `/metrics` HTTP responder (stage E).
//! * [`disk`]: the disk guard (`--min-free-disk`).
//! * [`lock`]: the LOCK sidecar naming the holder.
//! * [`powercut`]: a test-only in-memory redb storage backend that can
//!   lose power (the durability tests).
//! * [`testing`]: [`testing::TestServer`] and [`testing::ClusterTestbed`]
//!   for tests.
//!
//! Every write is a replicated log entry (D5): the service turns it into a
//! prost `LogCommand`, proposes it through `Raft::client_write`, and the
//! state machine applies it in one store transaction that also records the
//! entry's index (`RAFT_SM`), so apply is exactly-once and a crash can never
//! leave a half-applied entry (D7). Reads never go through Raft: `LOCAL`
//! reads the node's store, `LINEARIZABLE` first runs openraft's read barrier,
//! and a `snapshot_id` reads a frozen handle.

pub mod conn;
pub mod disk;
pub mod extractors;
pub mod forward;
pub mod join;
pub mod lock;
pub mod observe;
pub mod paths;
pub mod powercut;
pub mod raft;
pub mod server;
pub mod services;
pub mod slot;
pub mod snapshots;
pub mod testing;

pub use disk::{DiskGuard, FreeSpaceProbe};
pub use extractors::{extractors_hash, SharedExtractor};
pub use lock::LockFile;
pub use paths::{InitMode, JoinSpec, NodeJson, NodePaths};
pub use raft::network::FaultPlan;
pub use raft::RaftSettings;
pub use server::{
    run_blocking, run_blocking_with, start, Running, ServeConfig, ShutdownHandle, SysInfoFn,
    TestingHooks,
};
pub use slot::StoreSlot;

/// The server's own version, reported in `Hello` and `Admin.Status`.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default limit `Search` / `SearchSymbols` apply to a request without one
/// (ADR 0004 D1); the response says so with `applied_default_limit`.
pub const DEFAULT_SEARCH_LIMIT: usize = 1000;

/// Snapshot handles one connection may hold at once (ADR 0004 D1).
pub const SNAPSHOT_HANDLES_PER_CONNECTION: usize = 64;

/// Snapshot handles the whole server may hold at once (each pins a read
/// transaction, so old pages cannot be reclaimed while it lives).
pub const SNAPSHOT_HANDLES_GLOBAL: usize = 1024;

/// How often the idle reaper drops expired snapshot handles.
pub const SNAPSHOT_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The health service name that is `SERVING` while a leader is known and
/// this node's applied index is within `--ready-max-lag` entries of the
/// leader's committed index (D10).
pub const READY_SERVICE: &str = "memory-graph.ready";

/// Default `--ready-max-lag`: entries a node may be behind the leader's
/// committed index and still report ready.
pub const DEFAULT_READY_MAX_LAG: u64 = 1000;
