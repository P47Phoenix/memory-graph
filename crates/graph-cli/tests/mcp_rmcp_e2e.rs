//! A real MCP client against the real binary (ADR 0005 test plan item 4,
//! epic story 34): the official Rust SDK, `rmcp`, drives `memory-graph mcp`
//! over stdio (as a child process) and `serve --mcp-listen 127.0.0.1:0` over
//! streamable HTTP. Each session runs initialize, initialized, `tools/list`
//! and `tools/call search` on the vendored corpus, and every span is checked
//! against the source bytes.
//!
//! Why `rmcp` and not a recorded MCP Inspector transcript: ADR 0005 allows
//! `rmcp` as a dev-dependency if it passes `scripts/check-no-c-deps.py`.
//! It does with `default-features = false` and the
//! `transport-streamable-http-client-reqwest` feature, which pulls reqwest
//! without TLS (see the comment in Cargo.toml). A live SDK client also
//! catches what a byte-for-byte replay cannot: it negotiates the version
//! itself, sends the headers it really sends (`Accept`, `Mcp-Session-Id`,
//! `MCP-Protocol-Version`) and parses our answers with its own types.
mod common;

use common::readiness::{start_serve, StartOptions};
use rmcp::model::{CallToolRequestParams, ClientConfig, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");

fn corpus_repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus/anyhow")
}

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_CONFIG")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn indexed_db(dir: &Path) -> PathBuf {
    let db = dir.join("g.redb");
    let o = cmd()
        .arg("--db")
        .arg(&db)
        .args(["index", "--org", "corpus", "--repo", "anyhow"])
        .arg(corpus_repo())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    db
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// The client's `initialize` parameters, asking for `version`.
fn client_info(version: ProtocolVersion) -> ClientConfig {
    ClientConfig::default().with_protocol_version(version)
}

/// The byte offset of 1-based `line`, `col` (in Unicode scalar values).
fn offset_of(src: &str, line: u64, col: u64) -> usize {
    let start = src
        .split_inclusive('\n')
        .take(line as usize - 1)
        .map(str::len)
        .sum::<usize>();
    start
        + src[start..]
            .chars()
            .take(col as usize - 1)
            .map(char::len_utf8)
            .sum::<usize>()
}

fn args(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

/// tools/list and `search` through an initialized rmcp client; the
/// negotiated version must be `expect_version`.
async fn exercise(client: RunningService<RoleClient, ClientConfig>, expect_version: &str) {
    let info = client.peer_info().expect("initialize answered");
    assert_eq!(info.protocol_version.as_str(), expect_version);
    assert_eq!(info.server_info.as_ref().unwrap().name, "memory-graph");
    assert!(info.capabilities.tools.is_some(), "{info:?}");

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "describe",
            "list_repos",
            "search",
            "find_symbols",
            "file_outline",
            "file_tokens",
            "list_files"
        ]
    );
    for t in &tools {
        assert!(
            t.output_schema.is_some(),
            "{} declares outputSchema",
            t.name
        );
        let ro = t.annotations.as_ref().and_then(|a| a.read_only_hint);
        assert_eq!(ro, Some(true), "{} is read-only", t.name);
    }

    let r = client
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(args(json!({
                "text": "Error", "grain": "token", "org": "corpus", "repo": "anyhow", "limit": 500
            }))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(false), "{r:?}");
    let sc = r.structured_content.clone().expect("structuredContent");
    graph_mcp::schema::validate(&graph_mcp::tools::output_schema("search").unwrap(), &sc).unwrap();
    // The text copy is the same object.
    let text = r.content[0].as_text().expect("a text block").text.clone();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), sc);
    let items = sc["items"].as_array().unwrap();
    assert!(items.len() > 20, "{} hits", items.len());
    for h in items {
        let src = std::fs::read_to_string(corpus_repo().join(h["file"].as_str().unwrap())).unwrap();
        let sp = &h["span"];
        let n = |k: &str| sp[k].as_u64().unwrap();
        let (s, e) = (n("start") as usize, n("end") as usize);
        assert_eq!(&src[s..e], "Error", "{h}");
        assert_eq!(offset_of(&src, n("start_line"), n("start_col")), s, "{h}");
        assert_eq!(offset_of(&src, n("end_line"), n("end_col")), e, "{h}");
    }
    // A symbol-grain hit spans its whole definition.
    let r = client
        .call_tool(
            CallToolRequestParams::new("find_symbols").with_arguments(args(
                json!({"pattern": "Error", "kind": "type", "limit": 5}),
            )),
        )
        .await
        .unwrap();
    let sc = r.structured_content.unwrap();
    let hit = &sc["items"][0];
    let src = std::fs::read_to_string(corpus_repo().join(hit["file"].as_str().unwrap())).unwrap();
    let def = &src[hit["span"]["start"].as_u64().unwrap() as usize
        ..hit["span"]["end"].as_u64().unwrap() as usize];
    assert!(def.contains("Error"), "{def}");

    // An unknown tool is a JSON-RPC error the SDK surfaces as one.
    let e = client
        .call_tool(CallToolRequestParams::new("drop_tables"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("drop_tables"), "{e}");
    // A store-level refusal (an unknown language) is an isError result.
    let r = client
        .call_tool(
            CallToolRequestParams::new("search")
                .with_arguments(args(json!({"text": "x", "language": "klingon"}))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true), "{r:?}");
    client.cancel().await.unwrap();
}

#[test]
fn rmcp_over_stdio() {
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(d.path());
    let rt = runtime();
    rt.block_on(async {
        for (asked, got) in [
            (ProtocolVersion::V_2025_06_18, "2025-06-18"),
            (ProtocolVersion::V_2025_11_25, "2025-11-25"),
            // Newer than we speak: answered with our latest, which the
            // SDK accepts.
            (ProtocolVersion::LATEST, "2025-11-25"),
        ] {
            let mut c = tokio::process::Command::new(BIN);
            c.env_remove("MEMORY_GRAPH_SERVER")
                .env_remove("MEMORY_GRAPH_READ")
                .arg("--db")
                .arg(&db)
                .arg("mcp");
            let (transport, _stderr) = TokioChildProcess::builder(c)
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let client = client_info(asked).serve(transport).await.unwrap();
            exercise(client, got).await;
        }
    });
}

/// `serve --db <db> --listen 127.0.0.1:0 --mcp-listen 127.0.0.1:0`; the
/// MCP address once printed.
struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(db: &Path) -> (Serve, SocketAddr) {
    let mut c = cmd();
    c.args(["serve", "--db"]).arg(db).args([
        "--listen",
        "127.0.0.1:0",
        "--mcp-listen",
        "127.0.0.1:0",
    ]);
    let options = StartOptions {
        echo_stderr: false,
        ..StartOptions::default()
    };
    let s = start_serve(c, options);
    let mcp = s.mcp.expect("an `mcp on` line before `listening on`");
    (Serve(s.child), mcp)
}

#[test]
fn rmcp_over_streamable_http() {
    let d = tempfile::tempdir().unwrap();
    let db = indexed_db(d.path());
    let (_serve, addr) = serve(&db);
    let rt = runtime();
    rt.block_on(async {
        for (asked, got) in [
            (ProtocolVersion::V_2025_06_18, "2025-06-18"),
            (ProtocolVersion::V_2025_11_25, "2025-11-25"),
        ] {
            let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
            let client = client_info(asked).serve(transport).await.unwrap();
            exercise(client, got).await;
        }
    });
}
