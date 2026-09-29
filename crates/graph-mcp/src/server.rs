//! JSON-RPC 2.0 framing and the MCP lifecycle (ADR 0005 D1), independent
//! of the transport: [`McpServer::handle`] takes one message and returns
//! the reply, if any; [`serve_stdio`] runs it over newline-delimited
//! messages (the stdio transport).
//!
//! - One message per line; batches (JSON arrays) are refused with
//!   `-32600`: MCP dropped JSON-RPC batching in 2025-06-18.
//! - A request id is a string or an integer (MCP forbids `null`).
//! - Before `initialize` only `initialize` and `ping` are answered.
//! - `notifications/cancelled` is accepted and ignored: the stdio loop
//!   answers one request at a time, so a request is finished before a later
//!   cancellation can be read (the spec lets a server ignore it).
//! - Messages from the client that are responses (we send no requests) and
//!   unknown notifications are ignored.
use crate::tools::{error_body, parse_call, tool_definitions, McpBackend, ToolError};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};

/// MCP revisions this server speaks, newest first: the latest stable one
/// at build time and the one before it (ADR 0005 D1).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2025-11-25", "2025-06-18"];

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// A request other than `initialize` or `ping` before `initialize`.
pub const NOT_INITIALIZED: i64 = -32002;

/// The protocol state machine over one backend.
pub struct McpServer<B> {
    backend: B,
    name: String,
    version: String,
    /// The negotiated revision, once `initialize` succeeded.
    protocol: Option<&'static str>,
    /// `notifications/initialized` was received.
    initialized: bool,
}

fn error(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message.into()});
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": e})
}

fn result(id: Value, r: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": r})
}

impl<B: McpBackend> McpServer<B> {
    /// `name` and `version` are what `initialize` reports as `serverInfo`.
    pub fn new(backend: B, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            backend,
            name: name.into(),
            version: version.into(),
            protocol: None,
            initialized: false,
        }
    }

    /// The negotiated protocol revision, once initialized.
    pub fn protocol_version(&self) -> Option<&'static str> {
        self.protocol
    }

    /// Whether the client sent `notifications/initialized`.
    pub fn client_initialized(&self) -> bool {
        self.initialized
    }

    /// One raw message (a line, without its newline) in, the serialized
    /// reply out (`None` for a notification or a client response).
    pub fn handle_bytes(&mut self, bytes: &[u8]) -> Option<String> {
        let reply = match serde_json::from_slice::<Value>(bytes) {
            Ok(v) => self.handle(v),
            Err(e) => Some(error(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            )),
        };
        reply.map(|r| serde_json::to_string(&r).expect("a Value serializes"))
    }

    /// One parsed message in, the reply out.
    pub fn handle(&mut self, msg: Value) -> Option<Value> {
        let obj = match msg {
            Value::Object(o) => o,
            Value::Array(_) => {
                return Some(error(
                    Value::Null,
                    INVALID_REQUEST,
                    "JSON-RPC batches are not supported (MCP 2025-06-18 and later send one message at a time)",
                    None,
                ))
            }
            _ => {
                return Some(error(
                    Value::Null,
                    INVALID_REQUEST,
                    "a JSON-RPC message must be an object",
                    None,
                ))
            }
        };
        let id = obj.get("id").cloned();
        let id_ok = match &id {
            None => true,
            Some(Value::String(_)) => true,
            Some(Value::Number(n)) => n.is_i64() || n.is_u64(),
            _ => false,
        };
        if !id_ok {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "the id must be a string or an integer",
                None,
            ));
        }
        let reply_id = id.clone().unwrap_or(Value::Null);
        if obj.get("jsonrpc") != Some(&json!("2.0")) {
            return id.map(|_| error(reply_id, INVALID_REQUEST, "`jsonrpc` must be \"2.0\"", None));
        }
        let method = match obj.get("method") {
            Some(Value::String(m)) => m.clone(),
            Some(_) => {
                return Some(error(
                    reply_id,
                    INVALID_REQUEST,
                    "`method` must be a string",
                    None,
                ))
            }
            // A response to a request of ours (we send none): ignored.
            None if obj.contains_key("result") || obj.contains_key("error") => return None,
            None => {
                return Some(error(
                    reply_id,
                    INVALID_REQUEST,
                    "a request needs a `method`",
                    None,
                ))
            }
        };
        let empty = Map::new();
        let params = match obj.get("params") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(p)) => p,
            Some(_) => {
                return id
                    .map(|_| error(reply_id, INVALID_PARAMS, "`params` must be an object", None))
            }
        };
        let Some(id) = id else {
            self.notification(&method);
            return None;
        };
        Some(match self.request(&method, params) {
            Ok(r) => result(id, r),
            Err((code, msg, data)) => error(id, code, msg, data),
        })
    }

    fn notification(&mut self, method: &str) {
        if method == "notifications/initialized" && self.protocol.is_some() {
            self.initialized = true;
        }
        // notifications/cancelled and anything else: nothing to do.
    }

    fn request(
        &mut self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, (i64, String, Option<Value>)> {
        match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            _ if self.protocol.is_none() => Err((
                NOT_INITIALIZED,
                format!("`{method}` before `initialize`: send initialize first"),
                None,
            )),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.call(params),
            other => Err((
                METHOD_NOT_FOUND,
                format!("method `{other}` not found; this server has initialize, ping, tools/list and tools/call"),
                None,
            )),
        }
    }

    fn initialize(
        &mut self,
        params: &Map<String, Value>,
    ) -> Result<Value, (i64, String, Option<Value>)> {
        if self.protocol.is_some() {
            return Err((INVALID_REQUEST, "already initialized".into(), None));
        }
        let requested = match params.get("protocolVersion") {
            Some(Value::String(v)) => v.clone(),
            _ => {
                return Err((
                    INVALID_PARAMS,
                    "initialize needs a string `protocolVersion`".into(),
                    None,
                ))
            }
        };
        let Some(v) = SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == requested)
        else {
            return Err((
                INVALID_PARAMS,
                "Unsupported protocol version".into(),
                Some(json!({
                    "supported": SUPPORTED_PROTOCOL_VERSIONS,
                    "requested": requested,
                })),
            ));
        };
        self.protocol = Some(v);
        Ok(json!({
            "protocolVersion": v,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": self.name, "title": "memory-graph", "version": self.version },
            "instructions": "Read-only access to a code memory graph (org -> repo -> file -> symbol -> token). Call describe first to learn the orgs, repos, languages and symbol kinds present; then search (token text rolled up to symbols), find_symbols (definitions by name), file_outline, file_tokens and list_files. List tools page with limit/offset; pass next_offset to continue."
        }))
    }

    fn call(&mut self, params: &Map<String, Value>) -> Result<Value, (i64, String, Option<Value>)> {
        let name = match params.get("name") {
            Some(Value::String(n)) => n.as_str(),
            _ => {
                return Err((
                    INVALID_PARAMS,
                    "tools/call needs a string `name`".into(),
                    None,
                ))
            }
        };
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => {
                return Err((
                    INVALID_PARAMS,
                    "tools/call `arguments` must be an object".into(),
                    None,
                ))
            }
        };
        let outcome = parse_call(name, args).and_then(|c| self.backend.read(&c));
        match outcome {
            Ok(structured) => {
                let text = serde_json::to_string(&structured).expect("a Value serializes");
                Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": structured,
                    "isError": false
                }))
            }
            Err(ToolError::InvalidParams(m)) => Err((INVALID_PARAMS, m, None)),
            Err(e) => {
                let body = error_body(&e).expect("not invalid params");
                let text = serde_json::to_string(&body).expect("a Value serializes");
                Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": true
                }))
            }
        }
    }
}

/// Serve MCP over newline-delimited messages until `input` ends: read a
/// line, write the reply (if any) followed by `\n`, flush. Blank lines are
/// skipped; a line that is not UTF-8 JSON gets a parse error. Nothing else
/// is ever written to `output`, so it can be the process's stdout.
pub fn serve_stdio<B: McpBackend>(
    server: &mut McpServer<B>,
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_bytes(trimmed) {
            output.write_all(reply.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}
