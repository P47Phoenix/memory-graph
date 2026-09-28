//! Dev tooling for memory-graph. Not a workspace member (root `Cargo.toml`
//! excludes it), so its build-time dependencies never reach the shipped
//! binary or the pure-Rust gate.
//!
//! ```text
//! cargo run --manifest-path xtask/Cargo.toml -- proto
//! ```
//!
//! regenerates `crates/graph-proto/src/gen/memory_graph.v1.rs` from the
//! `.proto` files under `crates/graph-proto/proto` with `protox` (a pure-Rust
//! protobuf compiler: no `protoc` binary) and `tonic-prost-build`. The output
//! is checked in; CI's `proto-regen` job reruns this and fails on any diff.
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const PROTO_FILES: &[&str] = &[
    "memory_graph/v1/common.proto",
    "memory_graph/v1/store.proto",
    "memory_graph/v1/write.proto",
    "memory_graph/v1/admin.proto",
    "memory_graph/v1/raft.proto",
];

fn repo_root() -> PathBuf {
    // xtask/ sits directly under the repository root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf()
}

fn proto() -> Result<()> {
    let root = repo_root();
    let include = root.join("crates/graph-proto/proto");
    let out_dir = root.join("crates/graph-proto/src/gen");
    let files: Vec<PathBuf> = PROTO_FILES.iter().map(|f| include.join(f)).collect();
    for f in &files {
        if !f.is_file() {
            bail!("missing proto file {}", f.display());
        }
    }

    let fds = protox::compile(&files, [&include])
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("protox failed to compile the .proto files")?;

    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // The generated file is `include!`d by graph-proto, so rustfmt never sees
    // it: prost's prettyplease formatting is what keeps it readable and
    // stable between runs. `BTreeMap` for every map so the Rust side matches
    // graph-store's `BTreeMap` fields and the output is deterministic.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&out_dir)
        .btree_map(".")
        .emit_rerun_if_changed(false)
        .compile_fds(fds)
        .context("tonic-prost-build codegen failed")?;

    let generated = out_dir.join("memory_graph.v1.rs");
    if !generated.is_file() {
        bail!("codegen produced no {}", generated.display());
    }
    // Line endings must not depend on the host: normalise to LF so a Windows
    // regen equals a Linux one.
    let text = std::fs::read_to_string(&generated)?;
    let normalised = text.replace("\r\n", "\n");
    if normalised != text {
        std::fs::write(&generated, normalised)?;
    }
    println!("wrote {}", generated.display());
    Ok(())
}

fn main() -> Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "proto" => proto(),
        _ => bail!("usage: cargo run --manifest-path xtask/Cargo.toml -- proto"),
    }
}
