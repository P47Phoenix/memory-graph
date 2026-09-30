//! A minimal blocking HTTP/1.1 client for the MCP endpoint's tests
//! (`serve --mcp-listen`): one request per connection (`Connection:
//! close`), over `std::net`, no extra dependency. [`McpHttpClient`] adds
//! the MCP session (initialize, `Mcp-Session-Id`, ids).
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// One HTTP answer.
#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    /// Header names lower case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpReply {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON (panics if it is not).
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {} {}", self.status, self.body))
    }
}

/// Send one request to `addr`. `Host` defaults to `addr`; a `("Host", ..)`
/// in `headers` replaces it (an empty value leaves it out).
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<HttpReply> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| addr.to_string());
    if !host.is_empty() {
        head.push_str(&format!("Host: {host}\r\n"));
    }
    let mut has_len = false;
    for (k, v) in headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
    {
        has_len |= k.eq_ignore_ascii_case("content-length");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_len {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    s.write_all(head.as_bytes())?;
    // The server may answer (413) before reading a body it refuses.
    let _ = s.write_all(body);
    let _ = s.flush();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other(format!("no HTTP head in {text:?}")))?;
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line in {head:?}")))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    Ok(HttpReply {
        status,
        headers,
        body,
    })
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    loop {
        let Some((size, rest)) = s.split_once("\r\n") else {
            return out;
        };
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 || rest.len() < n {
            return out;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
}

/// An MCP client over the streamable HTTP endpoint: `new` initializes a
/// session (request plus `notifications/initialized`).
pub struct McpHttpClient {
    pub addr: SocketAddr,
    pub session: String,
    pub protocol: String,
    next_id: i64,
}

impl McpHttpClient {
    pub fn new(addr: SocketAddr) -> Self {
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0],
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        });
        let r = http(
            addr,
            "POST",
            "/mcp",
            &[("Content-Type", "application/json")],
            init.to_string().as_bytes(),
        )
        .expect("initialize");
        assert_eq!(r.status, 200, "{r:?}");
        let session = r
            .header("mcp-session-id")
            .expect("a session id")
            .to_string();
        let protocol = r.json()["result"]["protocolVersion"]
            .as_str()
            .unwrap()
            .to_string();
        let c = Self {
            addr,
            session,
            protocol,
            next_id: 1,
        };
        let n = c.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        assert_eq!(n.status, 202, "{n:?}");
        c
    }

    /// POST one message with this session's headers.
    pub fn post(&self, msg: &Value) -> HttpReply {
        http(
            self.addr,
            "POST",
            "/mcp",
            &[
                ("Content-Type", "application/json"),
                ("Accept", "application/json, text/event-stream"),
                ("Mcp-Session-Id", &self.session),
                ("MCP-Protocol-Version", &self.protocol),
            ],
            msg.to_string().as_bytes(),
        )
        .expect("an HTTP answer")
    }

    /// A request; the whole JSON-RPC response.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let r = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        assert_eq!(r.status, 200, "{method}: {r:?}");
        assert_eq!(r.header("content-type"), Some("application/json"));
        let v = r.json();
        assert_eq!(v["id"], id, "{v}");
        v
    }
}
