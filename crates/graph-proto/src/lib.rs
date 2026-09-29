//! The memory-graph wire contract (ADR 0004 D1): protobuf package
//! `memory_graph.v1`, its four gRPC services (`Store`, `Write`, `Admin`,
//! `Raft`), and the conversions between the generated messages and the
//! public types of `graph-core` / `graph-store`.
//!
//! * [`pb`] is the generated code (`src/gen/memory_graph.v1.rs`, checked in;
//!   regenerate with `cargo run --manifest-path xtask/Cargo.toml -- proto`,
//!   which needs no `protoc`: `protox` compiles the `.proto` files).
//! * [`convert`] holds `From` / `TryFrom` between every mirrored type and its
//!   message, both directions. Proto -> Rust is fallible (`ConvertError`):
//!   an UNSPECIFIED enum, a missing required sub-message or a `u64` that
//!   does not fit `usize` is a protocol error, never a default value.
//! * [`error`] maps `StoreError` to and from `tonic::Status`: the gRPC code
//!   from the ADR's table, and the typed variant prost-encoded in
//!   `Status::details` so it round-trips.
//!
//! This crate contains no server or client logic; `graph-server` and
//! `graph-client` (stage A package 3) build on it.

pub mod convert;
pub mod error;
pub mod version;

/// The generated `memory_graph.v1` package.
pub mod memory_graph {
    #[allow(clippy::all, clippy::pedantic, rustdoc::all, missing_docs)]
    pub mod v1 {
        include!("gen/memory_graph.v1.rs");
    }
}

pub use convert::{ConvertError, View};
pub use error::{status_to_store_error, store_error_to_status, WireError};
pub use memory_graph::v1 as pb;
pub use version::{check_version, CheckVersion, SendVersion, PROTOCOL_VERSION_HEADER};

/// The protocol version exchanged in `Store.Hello`. A server refuses any
/// other value with `FAILED_PRECONDITION` and a `Protocol` detail (ADR 0004
/// D1); a breaking change bumps this and the package (`v2`).
pub const PROTOCOL_VERSION: u32 = 1;

/// The number of nodes per `NodeBatch` on a server stream (`Descendants`,
/// `FileTokens`).
pub const STREAM_BATCH_NODES: usize = 4096;

/// Largest replicated log entry the leader produces: an `IndexChunk` is cut
/// when its files' bytes reach this (a single larger file is an entry of its
/// own), ADR 0004 D5.
pub const RAFT_ENTRY_MAX_BYTES: usize = 8 << 20;

/// Every gRPC method path this package defines (`/memory_graph.v1.Store/Search`,
/// ...), read from the generated code, so a new RPC is listed without an
/// edit here. A server labels metrics by these (and the standard health
/// methods) only: a client-supplied path is not a label value.
pub fn rpc_paths() -> &'static std::collections::BTreeSet<&'static str> {
    static PATHS: std::sync::OnceLock<std::collections::BTreeSet<&'static str>> =
        std::sync::OnceLock::new();
    PATHS.get_or_init(|| {
        const GEN: &str = include_str!("gen/memory_graph.v1.rs");
        GEN.split('"')
            .skip(1)
            .step_by(2)
            .filter(|s| {
                s.strip_prefix("/memory_graph.v1.")
                    .and_then(|r| r.split_once('/'))
                    .is_some_and(|(svc, m)| {
                        !svc.is_empty()
                            && !m.is_empty()
                            && svc.bytes().all(|b| b.is_ascii_alphanumeric())
                            && m.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests;
