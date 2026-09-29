//! MCP access to the memory graph (ADR 0005): the transport-independent
//! core. [`McpServer`] is JSON-RPC 2.0 plus the MCP lifecycle
//! (`initialize` with version negotiation, `notifications/initialized`,
//! `ping`, `tools/list`, `tools/call`, cancellation); [`tools`] holds the
//! seven read-only tools and [`StoreBackend`], which answers them from any
//! [`graph_store::StoreRead`]; [`serve_stdio`] is the stdio transport
//! behind `memory-graph mcp`.
//!
//! Hand-rolled on `serde_json`, with no MCP SDK at run time, so the
//! pure-Rust gate stays clean. Depends on graph-store types only: no
//! redb, tonic or language crate.
pub mod schema;
mod server;
pub mod tools;

pub use server::{
    serve_stdio, McpServer, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
    NOT_INITIALIZED, PARSE_ERROR, SUPPORTED_PROTOCOL_VERSIONS,
};
pub use tools::{
    tool_definitions, McpBackend, PageArgs, StaleCounter, StoreBackend, ToolCall, ToolError,
    DEFAULT_LIMIT, MAX_LIMIT,
};
