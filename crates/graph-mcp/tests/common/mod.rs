//! Minimal MCP test clients: [`Client`] over [`McpServer`] in process, and
//! [`HttpClient`] over `serve --mcp-listen`. Through [`Tools`], every
//! `tools/call` answer is checked against the tool's `outputSchema`, and
//! every input against its `inputSchema`, before a test sees it.
#![allow(dead_code)]
use graph_mcp::{schema, tools, McpBackend, McpServer};
use graph_server::testing::mcp_http::McpHttpClient;
use serde_json::{json, Value};

/// What the checks need from a client: one request, the whole response.
pub trait Tools {
    fn request(&mut self, method: &str, params: Value) -> Value;

    /// `tools/call`; the `result` (a JSON-RPC error panics).
    fn call(&mut self, name: &str, args: Value) -> Value {
        let r = self.call_raw(name, args);
        r.get("result")
            .unwrap_or_else(|| panic!("{name}: {r}"))
            .clone()
    }

    /// `tools/call`; the whole response. A successful result is checked
    /// against the output schema, its text copy against its structured
    /// content.
    fn call_raw(&mut self, name: &str, args: Value) -> Value {
        let input_ok = tools::input_schema(name).map(|s| schema::validate(&s, &args));
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        if let Some(res) = r.get("result") {
            if res["isError"] == false {
                // Only input the schema accepts may succeed.
                assert!(
                    matches!(input_ok, Some(Ok(()))),
                    "{name} {args} succeeded but fails inputSchema: {input_ok:?}"
                );
                let sc = &res["structuredContent"];
                let out = tools::output_schema(name).unwrap();
                if let Err(e) = schema::validate(&out, sc) {
                    panic!("{name}: structuredContent fails outputSchema: {e}\n{sc}");
                }
                let text: Value =
                    serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(&text, sc, "the text copy equals structuredContent");
            } else {
                let body: Value =
                    serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
                assert!(
                    body["code"].is_string() && body["message"].is_string(),
                    "{body}"
                );
            }
        }
        r
    }

    /// `structuredContent` of a successful call.
    fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], false, "{name} {args}: {r}");
        r["structuredContent"].clone()
    }

    /// The `{code, message}` of an `isError` result.
    fn tool_error(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], true, "{name} {args}: {r}");
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// The JSON-RPC error of a call.
    fn rpc_error(&mut self, name: &str, args: Value) -> Value {
        let r = self.call_raw(name, args.clone());
        r.get("error")
            .unwrap_or_else(|| panic!("{name} {args}: expected an error, got {r}"))
            .clone()
    }
}

pub struct Client<B> {
    pub server: McpServer<B>,
    next_id: i64,
}

impl<B: McpBackend> Client<B> {
    /// A server that has been initialized (request plus notification).
    pub fn new(backend: B) -> Self {
        let mut c = Self {
            server: McpServer::new(backend, "memory-graph", "test"),
            next_id: 1,
        };
        let r = c.request(
            "initialize",
            json!({
                "protocolVersion": graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0],
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        assert!(r.get("result").is_some(), "{r}");
        assert!(c
            .server
            .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        c
    }
}

impl<B: McpBackend> Tools for Client<B> {
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .unwrap();
        let reply = self.server.handle_bytes(line.as_bytes()).expect("a reply");
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], id, "{v}");
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }
}

/// The streamable HTTP endpoint of a running server.
pub struct HttpClient(pub McpHttpClient);

impl HttpClient {
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self(McpHttpClient::new(addr))
    }
}

impl Tools for HttpClient {
    fn request(&mut self, method: &str, params: Value) -> Value {
        let v = self.0.request(method, params);
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }
}
