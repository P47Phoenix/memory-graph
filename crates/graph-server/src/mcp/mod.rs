//! MCP over streamable HTTP inside `serve` (ADR 0005 D1/D3/D4, epic story
//! 32): `--mcp-listen <addr>` serves one `/mcp` endpoint, on its own port
//! (the gRPC port must reach peers; this one defaults to loopback), with
//! axum. Off unless the flag is given.
//!
//! The protocol and the tools are [`graph_mcp`]'s; this module is the
//! transport plus the D4 guards:
//! - a non-loopback bind is refused unless `--mcp-allow-remote`, which logs
//!   a warning at every start ([`check_bind`]);
//! - an `Origin` not in `--mcp-allow-origin` (default none) gets 403; a
//!   request without `Origin` (not a browser) passes;
//! - on a loopback bind, a `Host` other than `localhost` or the bound
//!   address (with the bound port, if it names one) gets 403 (DNS
//!   rebinding). A non-loopback bind (`--mcp-allow-remote`) is reached by
//!   whatever name the operator's proxy uses, so it skips the Host check;
//! - a body over [`MAX_BODY_BYTES`] gets 413;
//! - past `--mcp-max-inflight` requests at once, 429 (with `Retry-After`);
//! - a call past [`CALL_DEADLINE`] answers a JSON-RPC error
//!   ([`REQUEST_TIMEOUT`]);
//! - results are cut at 4 MiB with `next_offset` and tools are read-only
//!   (both in `graph-mcp`).
//!
//! Transport: `POST /mcp` carries one JSON-RPC message. `initialize`
//! (without a session) opens a session, whose id comes back in
//! `Mcp-Session-Id`; every later message must carry it (400 without, 404
//! for an unknown or ended one). The session holds only the negotiated
//! protocol version; a request naming another one in `MCP-Protocol-Version`
//! gets 400. A request's answer is `application/json`; a notification or a
//! response gets 202 with no body. `GET /mcp` is 405: v1 has no
//! server-initiated stream and no SSE resumability. `DELETE /mcp` ends the
//! session.
//!
//! Every tool read goes through this node's own Store service in process
//! ([`backend::InProcessStore`]), on the blocking pool.
use crate::server::ShutdownHandle;
use crate::services::store::StoreService;
use crate::services::Ctx;
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use graph_mcp::{McpServer, StoreBackend};
use graph_proto::View;
use graph_store::StoreError;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

pub mod backend;

/// The largest request body accepted; more gets 413 (ADR 0005 D4).
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// Default `--mcp-max-inflight`.
pub const DEFAULT_MAX_INFLIGHT: usize = 16;
/// How long one call may run before it is answered with a timeout.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
/// Sessions kept at once; the oldest is ended when a new one would pass it.
pub const MAX_SESSIONS: usize = 1024;
/// The JSON-RPC error code of a call past [`CALL_DEADLINE`] (the code MCP
/// SDKs use for a request timeout).
pub const REQUEST_TIMEOUT: i64 = -32001;
/// The session header of the streamable HTTP transport.
pub const SESSION_HEADER: &str = "mcp-session-id";
/// The protocol version header of the streamable HTTP transport.
pub const PROTOCOL_HEADER: &str = "mcp-protocol-version";
/// The warning `serve --help` and `docs/mcp.md` box (ADR 0005 D4 item 7).
pub const NO_AUTH_WARNING: &str = "The MCP endpoint has no authentication. Anyone who can reach \
     it can read every indexed source token. Keep it on loopback or behind an authenticating \
     proxy until #105.";

/// `--mcp-read`: how the endpoint's tools read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpRead {
    /// This node's replica as it is (a follower may lag; `stale_possible`
    /// says when it may).
    #[default]
    Local,
    /// Every read after the read barrier (forwarded to the leader from a
    /// follower); `NoLeader` without a quorum.
    Linearizable,
}

/// The `--mcp-*` flags.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// `--mcp-listen`; port 0 picks a free one (see `Running::mcp_addr`).
    pub listen: SocketAddr,
    /// `--mcp-allow-remote`: allow a non-loopback `listen`.
    pub allow_remote: bool,
    /// `--mcp-allow-origin`: the browser origins allowed (exact, such as
    /// `http://localhost:6274`); every other `Origin` gets 403.
    pub allow_origins: Vec<String>,
    /// `--mcp-read`.
    pub read: McpRead,
    /// `--mcp-max-inflight`: requests served at once; more get 429.
    pub max_inflight: usize,
    /// The per-call deadline ([`CALL_DEADLINE`]; tests shorten it).
    pub call_timeout: Duration,
    /// Testing only: every call sleeps this long on the blocking pool
    /// first, so a test can hold requests in flight or pass the deadline.
    #[doc(hidden)]
    pub testing_call_delay: Option<Duration>,
}

impl McpConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            allow_remote: false,
            allow_origins: Vec::new(),
            read: McpRead::Local,
            max_inflight: DEFAULT_MAX_INFLIGHT,
            call_timeout: CALL_DEADLINE,
            testing_call_delay: None,
        }
    }
}

/// Refuse a non-loopback bind without `--mcp-allow-remote` (ADR 0005 D4),
/// and a zero `--mcp-max-inflight`.
pub fn check_bind(cfg: &McpConfig) -> Result<(), StoreError> {
    if cfg.max_inflight == 0 {
        return Err(StoreError::Rejected(
            "--mcp-max-inflight must be at least 1".into(),
        ));
    }
    if !cfg.listen.ip().is_loopback() && !cfg.allow_remote {
        return Err(StoreError::Rejected(format!(
            "--mcp-listen {} is not a loopback address; the MCP endpoint has no authentication \
             (#105), so it listens on loopback only (127.0.0.1, ::1). Pass --mcp-allow-remote \
             to listen there anyway, behind an authenticating proxy",
            cfg.listen
        )));
    }
    Ok(())
}

/// Log the D4 warnings of a start: always the no-auth one when remote
/// listening is allowed (at every start).
pub fn warn_at_start(cfg: &McpConfig, bound: SocketAddr) {
    if cfg.allow_remote {
        eprintln!(
            "memory-graph serve: WARNING: --mcp-allow-remote: the MCP endpoint at \
             http://{bound}/mcp may be reachable from other hosts. {NO_AUTH_WARNING}"
        );
        tracing::warn!(
            %bound,
            "--mcp-allow-remote: the MCP endpoint may be reachable from other hosts; it has no \
             authentication (#105)"
        );
    }
}

struct Sessions {
    /// Session id -> negotiated protocol version.
    map: HashMap<String, &'static str>,
    /// Creation order, for the [`MAX_SESSIONS`] bound.
    order: VecDeque<String>,
}

struct McpState {
    ctx: Arc<Ctx>,
    svc: Arc<StoreService>,
    view: View,
    bound: SocketAddr,
    allow_origins: Vec<String>,
    inflight: Arc<Semaphore>,
    call_timeout: Duration,
    delay: Option<Duration>,
    sessions: Mutex<Sessions>,
}

/// Serve `/mcp` on `listener` until `shutdown`.
pub async fn serve(listener: TcpListener, cfg: McpConfig, ctx: Arc<Ctx>, shutdown: ShutdownHandle) {
    let bound = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "MCP listener address");
            return;
        }
    };
    let state = Arc::new(McpState {
        svc: Arc::new(StoreService {
            ctx: Arc::clone(&ctx),
        }),
        ctx,
        view: match cfg.read {
            McpRead::Local => View::Local,
            McpRead::Linearizable => View::Linearizable,
        },
        bound,
        allow_origins: cfg
            .allow_origins
            .iter()
            .map(|o| normalize_origin(o))
            .collect(),
        inflight: Arc::new(Semaphore::new(cfg.max_inflight)),
        call_timeout: cfg.call_timeout,
        delay: cfg.testing_call_delay,
        sessions: Mutex::new(Sessions {
            map: HashMap::new(),
            order: VecDeque::new(),
        }),
    });
    let app = axum::Router::new()
        .route("/mcp", axum::routing::any(handle))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state);
    let r = axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "MCP endpoint stopped");
    }
}

fn normalize_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn plain(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{msg}\n"),
    )
        .into_response()
}

fn json_reply(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// `(name, port)` of a `Host` header value; `None` if malformed.
fn split_host(v: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = v.strip_prefix('[') {
        let (name, after) = rest.split_once(']')?;
        return match after {
            "" => Some((name, None)),
            p => Some((name, Some(p.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match v.rsplit_once(':') {
        Some((name, port)) => Some((name, Some(port.parse().ok()?))),
        None => Some((v, None)),
    }
}

/// Whether `host` is the bound loopback name or address (ADR 0005 D4).
pub fn host_allowed(host: &str, bound: SocketAddr) -> bool {
    let Some((name, port)) = split_host(host.trim()) else {
        return false;
    };
    if port.is_some_and(|p| p != bound.port()) {
        return false;
    }
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    name.parse::<IpAddr>().is_ok_and(|ip| ip == bound.ip())
}

impl McpState {
    /// The D4 header checks; `Some` is the refusal.
    fn guard(&self, headers: &HeaderMap) -> Option<Response> {
        if self.bound.ip().is_loopback() {
            let ok = headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| host_allowed(h, self.bound));
            if !ok {
                return Some(plain(
                    StatusCode::FORBIDDEN,
                    "forbidden: the Host header must name this loopback endpoint (localhost or its address)",
                ));
            }
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            let o = origin.to_str().map(normalize_origin).unwrap_or_default();
            if !self.allow_origins.contains(&o) {
                return Some(plain(
                    StatusCode::FORBIDDEN,
                    "forbidden: this Origin is not allowed (serve --mcp-allow-origin)",
                ));
            }
        }
        None
    }

    fn new_session(&self, protocol: &'static str) -> String {
        let id = format!("{:032x}", rand::random::<u128>());
        let mut s = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        while s.map.len() >= MAX_SESSIONS {
            match s.order.pop_front() {
                Some(old) => {
                    s.map.remove(&old);
                }
                None => break,
            }
        }
        s.map.insert(id.clone(), protocol);
        s.order.push_back(id.clone());
        id
    }

    fn session(&self, id: &str) -> Option<&'static str> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(id)
            .copied()
    }

    fn end_session(&self, id: &str) -> bool {
        let mut s = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let found = s.map.remove(id).is_some();
        if found {
            s.order.retain(|x| x != id);
        }
        found
    }
}

fn server_name() -> (&'static str, &'static str) {
    ("memory-graph", crate::SERVER_VERSION)
}

async fn handle(State(st): State<Arc<McpState>>, req: Request) -> Response {
    if let Some(refused) = st.guard(req.headers()) {
        return refused;
    }
    let session = req
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match *req.method() {
        Method::POST => {}
        Method::DELETE => {
            return match session {
                None => plain(StatusCode::BAD_REQUEST, "DELETE needs an Mcp-Session-Id"),
                Some(id) if st.end_session(&id) => StatusCode::OK.into_response(),
                Some(_) => plain(StatusCode::NOT_FOUND, "no such session"),
            }
        }
        _ => {
            // GET (a server-initiated SSE stream) is not offered in v1.
            let mut r = plain(
                StatusCode::METHOD_NOT_ALLOWED,
                "this endpoint takes POST (and DELETE to end a session)",
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            return r;
        }
    }
    let Ok(permit) = Arc::clone(&st.inflight).try_acquire_owned() else {
        let mut r = plain(
            StatusCode::TOO_MANY_REQUESTS,
            "too many MCP requests in flight (serve --mcp-max-inflight); retry",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return r;
    };
    let too_long = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_BODY_BYTES as u64);
    let protocol_header = req
        .headers()
        .get(PROTOCOL_HEADER)
        .map(|v| v.to_str().unwrap_or("").to_string());
    if too_long {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body over 1 MiB");
    }
    let body: Body = req.into_body();
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body over 1 MiB");
    };
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    let method = parsed
        .as_ref()
        .and_then(|v| v.get("method"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let has_id = parsed.as_ref().is_some_and(|v| v.get("id").is_some());
    let (name, version) = server_name();

    if parsed.is_none() || method.as_deref() == Some("initialize") {
        // Parse errors and `initialize` need no store and no session.
        let mut server = McpServer::new(NoBackend, name, version);
        let reply = server.handle_bytes(&bytes);
        drop(permit);
        let Some(reply) = reply else {
            return StatusCode::ACCEPTED.into_response();
        };
        if parsed.is_none() {
            return json_reply(StatusCode::BAD_REQUEST, reply);
        }
        let mut r = json_reply(StatusCode::OK, reply);
        if let Some(v) = server.protocol_version() {
            let id = st.new_session(v);
            r.headers_mut().insert(
                SESSION_HEADER,
                HeaderValue::from_str(&id).expect("hex is a valid header value"),
            );
        }
        return r;
    }

    let Some(id) = session else {
        return plain(
            StatusCode::BAD_REQUEST,
            "missing Mcp-Session-Id: send initialize first",
        );
    };
    let Some(protocol) = st.session(&id) else {
        return plain(
            StatusCode::NOT_FOUND,
            "unknown or ended MCP session: initialize again",
        );
    };
    if let Some(h) = protocol_header {
        if h != protocol {
            return plain(
                StatusCode::BAD_REQUEST,
                &format!("MCP-Protocol-Version {h:?} is not this session's ({protocol})"),
            );
        }
    }
    let tool = (method.as_deref() == Some("tools/call")).then(|| {
        let n = parsed
            .as_ref()
            .and_then(|v| v.pointer("/params/name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        tool_label(n)
    });
    let rt = tokio::runtime::Handle::current();
    let svc = Arc::clone(&st.svc);
    let view = st.view;
    let delay = st.delay;
    let work = tokio::task::spawn_blocking(move || {
        // Held until the read really ends, past a timed-out answer too, so
        // `--mcp-max-inflight` bounds the blocking work.
        let _permit = permit;
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        let store = backend::InProcessStore::new(svc, rt, view);
        let stale = store.stale_counter();
        let backend = StoreBackend::remote(
            Box::new(store),
            Box::new(move || stale.load(Ordering::SeqCst)),
        );
        let mut server = McpServer::resumed(backend, name, version, protocol)
            .expect("a session holds a supported version");
        server.handle_bytes(&bytes)
    });
    let outcome = tokio::time::timeout(st.call_timeout, work).await;
    let reply = match outcome {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "MCP call failed");
            if let Some(t) = &tool {
                st.ctx.raft.obs.observe_mcp_call(t, "error");
            }
            let id = parsed.as_ref().and_then(|v| v.get("id")).cloned();
            return json_reply(
                StatusCode::OK,
                rpc_error(id, graph_mcp::INTERNAL_ERROR, "internal error"),
            );
        }
        Err(_) => {
            if let Some(t) = &tool {
                st.ctx.raft.obs.observe_mcp_call(t, "timeout");
            }
            if !has_id {
                return StatusCode::ACCEPTED.into_response();
            }
            let id = parsed.as_ref().and_then(|v| v.get("id")).cloned();
            return json_reply(
                StatusCode::OK,
                rpc_error(
                    id,
                    REQUEST_TIMEOUT,
                    &format!(
                        "the call did not finish within {} s (the endpoint's per-call deadline)",
                        st.call_timeout.as_secs_f64()
                    ),
                ),
            );
        }
    };
    let Some(reply) = reply else {
        return StatusCode::ACCEPTED.into_response();
    };
    if let Some(t) = &tool {
        st.ctx.raft.obs.observe_mcp_call(t, call_outcome(&reply));
    }
    json_reply(StatusCode::OK, reply)
}

fn rpc_error(id: Option<Value>, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {"code": code, "message": message}
    })
    .to_string()
}

/// The metrics label of a tool name: a known tool, or `unknown` (a client
/// cannot grow the label set).
fn tool_label(name: &str) -> String {
    let known = graph_mcp::tool_definitions()
        .iter()
        .any(|t| t["name"] == name);
    if known {
        name.to_string()
    } else {
        "unknown".into()
    }
}

/// `ok`, `error` (an `isError` result) or `rejected` (a JSON-RPC error).
fn call_outcome(reply: &str) -> &'static str {
    match serde_json::from_str::<Value>(reply) {
        Ok(v) if v.get("error").is_some() => "rejected",
        Ok(v) if v.pointer("/result/isError") == Some(&Value::Bool(true)) => "error",
        Ok(_) => "ok",
        Err(_) => "rejected",
    }
}

/// `initialize` and parse errors touch no store.
struct NoBackend;

impl graph_mcp::McpBackend for NoBackend {
    fn read(&self, _: &graph_mcp::ToolCall) -> Result<Value, graph_mcp::ToolError> {
        Err(graph_mcp::ToolError::Store(StoreError::Protocol(
            "no session".into(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn host_check_accepts_only_the_bound_loopback_name_or_address() {
        let b = at("127.0.0.1:7071");
        for ok in ["127.0.0.1:7071", "127.0.0.1", "localhost:7071", "LOCALHOST"] {
            assert!(host_allowed(ok, b), "{ok}");
        }
        for bad in [
            "evil.example:7071",
            "127.0.0.1:7072",
            "127.0.0.2:7071",
            "localhost:80",
            "localhost.evil.example",
            "",
            "127.0.0.1:x",
            "[::1]:7071",
        ] {
            assert!(!host_allowed(bad, b), "{bad}");
        }
        let v6 = at("[::1]:9000");
        assert!(host_allowed("[::1]:9000", v6));
        assert!(host_allowed("[::1]", v6));
        assert!(host_allowed("localhost:9000", v6));
        assert!(!host_allowed("127.0.0.1:9000", v6));
        assert!(!host_allowed("[::1]x", v6));
    }

    #[test]
    fn a_non_loopback_bind_needs_allow_remote() {
        for a in ["0.0.0.0:0", "10.1.2.3:7071", "[::]:0"] {
            let mut c = McpConfig::new(at(a));
            let e = check_bind(&c).unwrap_err().to_string();
            assert!(e.contains("--mcp-allow-remote"), "{e}");
            c.allow_remote = true;
            check_bind(&c).unwrap();
        }
        for a in ["127.0.0.1:0", "127.0.0.9:7071", "[::1]:0"] {
            check_bind(&McpConfig::new(at(a))).unwrap();
        }
        let mut c = McpConfig::new(at("127.0.0.1:0"));
        c.max_inflight = 0;
        assert!(check_bind(&c).is_err());
    }

    #[test]
    fn outcomes_and_labels() {
        assert_eq!(call_outcome(r#"{"error":{"code":-32602}}"#), "rejected");
        assert_eq!(call_outcome(r#"{"result":{"isError":true}}"#), "error");
        assert_eq!(call_outcome(r#"{"result":{"isError":false}}"#), "ok");
        assert_eq!(tool_label("search"), "search");
        assert_eq!(tool_label("drop_tables"), "unknown");
    }
}
