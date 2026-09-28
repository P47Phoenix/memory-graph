//! The openraft integration (ADR 0004 D5/D7), kept in one place so API
//! churn in `openraft` (pinned `=0.9.25`) touches nothing else.
//!
//! * [`types`]: the `RaftTypeConfig` ([`types::TypeConfig`]), the log entry
//!   payload ([`types::LogRequest`], a prost-encoded `LogCommand`) and the
//!   per-entry response ([`types::LogResponse`]).
//! * [`log_store`]: [`log_store::RedbLogStore`], the Raft log and hard state
//!   in `<db>.raft.redb` (`Durability::Immediate`; `append` commits before
//!   it reports the entries flushed).
//! * [`state_machine`]: [`state_machine::StoreStateMachine`], which applies
//!   entries through the store's marked writes (one transaction per entry,
//!   exactly-once by the `RAFT_SM` marker) and builds / installs snapshots
//!   with `export_snapshot` / `install_snapshot`.
//! * [`network`]: [`network::GrpcNetwork`], the `RaftNetworkFactory` over
//!   the peers' `Raft` gRPC service, and the test fault-injection wrapper
//!   ([`network::FaultPlan`]); [`wire`] converts openraft's RPC types.
//! * [`snapshot_dir`]: [`snapshot_dir::SnapshotDir`], the snapshot files
//!   (`snap-<term>-<index>.redb` + `.meta`).
//! * [`node`]: [`node::RaftNode`], start-up (`Raft::initialize` on first
//!   start, resume from the log after) and the leader lookups the services
//!   use.

// `openraft::StorageError` is 224 bytes and the trait signatures are its;
// boxing it is not ours to decide.
#![allow(clippy::result_large_err)]

pub mod log_store;
pub mod network;
pub mod node;
pub mod snapshot_dir;
pub mod state_machine;
pub mod types;
pub mod wire;

pub use node::{RaftNode, RaftSettings};
pub use types::{LogRequest, LogResponse, NodeId, TypeConfig};
