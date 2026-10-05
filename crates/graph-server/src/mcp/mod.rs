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
//!   whatever names the operator's proxy uses, which this server cannot
//!   know, so it skips the Host check: DNS-rebinding protection then rests
//!   on the Origin allowlist (a browser always sends `Origin` on these
//!   requests) plus the operator's proxy;
//! - a body over [`MAX_BODY_BYTES`] gets 413 (with `Connection: close`,
//!   the rest being unread); 429 and the session and JSON answers come
//!   after the whole body was read. Every connection is closed in stages
//!   ([`linger`], RFC 9112 section 9.6), so an answer given before the body
//!   was read (413, and the 403/404/405 guards) still reaches a client that
//!   is sending it, rather than a TCP reset;
//! - past `--mcp-max-inflight` requests at once, 429 (with `Retry-After`),
//!   decided after the session checks;
//! - a call past [`CALL_DEADLINE`] answers a JSON-RPC error
//!   ([`REQUEST_TIMEOUT`]);
//! - results are cut at 4 MiB with `next_offset` and tools are read-only
//!   (both in `graph-mcp`).
//!
//! Transport: HTTP/1.1 (hyper, with axum's router), one task per
//! connection. `POST /mcp` carries one JSON-RPC message. `initialize`
//! (without a session) opens a session, whose id comes back in
//! `Mcp-Session-Id`; every later message must carry it (400 without, 404
//! for an unknown or ended one). The session holds only the negotiated
//! protocol version; a request naming another one in `MCP-Protocol-Version`
//! gets 400, and one without the header is served with the session's
//! (lenient: clients of 2025-03-26 do not send it). Sessions are bounded
//! by [`MAX_SESSIONS`] (least recently used evicted) and [`SESSION_IDLE`]. A request's answer is `application/json`; a notification or a
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
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

pub mod backend;

/// The largest request body accepted; more gets 413 (ADR 0005 D4).
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// Default `--mcp-max-inflight`.
pub const DEFAULT_MAX_INFLIGHT: usize = 16;
/// How long one call may run before it is answered with a timeout.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
/// Sessions kept at once; the least recently used is ended when a new one
/// would pass it.
pub const MAX_SESSIONS: usize = 1024;
/// A session unused for this long ends (its next request gets 404).
pub const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);
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
    /// A session unused this long ends ([`SESSION_IDLE`]; tests shorten it).
    pub session_idle: Duration,
    /// Testing only: the staged close's bounds ([`Linger::DEFAULT`]), so a
    /// test can check them without waiting the real ones out.
    #[doc(hidden)]
    pub testing_linger: Linger,
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
            session_idle: SESSION_IDLE,
            testing_linger: Linger::DEFAULT,
        }
    }
}

/// The bounds of the staged close ([`linger`]): it stops reading after
/// `idle` without a byte, after `time` in all, or after `bytes` read.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Linger {
    pub idle: Duration,
    pub time: Duration,
    pub bytes: u64,
}

impl Linger {
    /// 5 s idle, 30 s in all, 64 MiB (nginx's `lingering_timeout` and
    /// `lingering_time` defaults, plus a byte cap).
    pub const DEFAULT: Linger = Linger {
        idle: LINGER_IDLE,
        time: LINGER_TIME,
        bytes: LINGER_BYTES,
    };
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
        let at = if bound.ip().is_unspecified() {
            format!("bound on all interfaces, port {}", bound.port())
        } else {
            format!("at http://{bound}/mcp")
        };
        eprintln!(
            "memory-graph serve: WARNING: --mcp-allow-remote: the MCP endpoint ({at}) may be \
             reachable from other hosts; the Host check is off, so DNS-rebinding protection \
             rests on the Origin allowlist and your proxy. {NO_AUTH_WARNING}"
        );
        tracing::warn!(
            %bound,
            "--mcp-allow-remote: the MCP endpoint may be reachable from other hosts; it has no \
             authentication (#105)"
        );
    }
}

struct Sessions {
    /// Session id -> (negotiated protocol version, last use).
    map: HashMap<String, (&'static str, Instant)>,
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
    session_idle: Duration,
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
        }),
        session_idle: cfg.session_idle,
    });
    let app = axum::Router::new()
        .route("/mcp", axum::routing::any(handle))
        .fallback(|req: Request| async move {
            close_if_body_unread(req.headers(), StatusCode::NOT_FOUND.into_response())
        })
        .with_state(state);
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => {
                    // A connection that failed before it was accepted is
                    // the client's; anything else (out of file handles)
                    // gets a pause, as axum::serve does.
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionRefused
                    ) {
                        tracing::warn!(error = %e, "MCP endpoint: accept failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
            _ = shutdown.wait() => break,
        };
        // Reap the finished connections as we go.
        while conns.try_join_next().is_some() {}
        conns.spawn(serve_connection(
            stream,
            app.clone(),
            shutdown.clone(),
            cfg.testing_linger,
        ));
    }
    drop(listener);
    // The connections are asked to finish the request they are on. This
    // task is spawned detached (`server::start`), so that holds only while
    // the runtime lives: a `serve` process that exits ends them with it.
    while conns.join_next().await.is_some() {}
}

/// How long a connection the server is done with keeps reading (and
/// discarding) what the client still sends, at most, before it is closed
/// ([`linger`]).
pub(crate) const LINGER_TIME: Duration = Duration::from_secs(30);
/// How long [`linger`] waits for the client's next bytes before it closes.
pub(crate) const LINGER_IDLE: Duration = Duration::from_secs(5);
/// The most [`linger`] reads and discards from one connection.
pub(crate) const LINGER_BYTES: u64 = 64 << 20;
/// [`linger`]'s read buffer: small, since thousands of connections may
/// linger at once and each holds one.
const LINGER_BUF: usize = 8 << 10;

/// One HTTP/1.1 connection: hyper serves it, and the TCP connection is then
/// closed in stages ([`linger`]) rather than at once.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    app: axum::Router,
    shutdown: ShutdownHandle,
    bounds: Linger,
) {
    use hyper_util::rt::TokioIo;
    use tower_service::Service;
    let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let mut app = app.clone();
        // Boxed: `poll_without_shutdown` wants an `Unpin` future.
        Box::pin(async move { app.call(req.map(Body::new)).await })
    });
    let mut conn =
        hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc);
    let mut stopping = false;
    let served = loop {
        let done = tokio::select! {
            r = std::future::poll_fn(|cx| conn.poll_without_shutdown(cx)) => Some(r),
            _ = shutdown.wait(), if !stopping => None,
        };
        match done {
            Some(r) => break r,
            None => {
                // Finish the request in progress, then close.
                stopping = true;
                std::pin::Pin::new(&mut conn).graceful_shutdown();
            }
        }
    };
    if let Err(e) = served {
        tracing::debug!(error = %e, "MCP connection ended with an error");
    }
    // The IO back from hyper, which neither shut it down nor closed it:
    // the response it wrote is flushed.
    let stream = conn.into_parts().io.into_inner();
    if !shutdown.is_triggered() {
        tokio::select! {
            () = linger(stream, bounds) => {}
            _ = shutdown.wait() => {}
        }
    }
}

/// Close `stream` in stages (RFC 9112 section 9.6, "Tear-down"): send FIN
/// (half-close), then read and discard whatever the client still sends
/// until it closes too, `bounds.idle` passes without a byte, `bounds.time`
/// passes, or `bounds.bytes` were read ([`Linger::DEFAULT`]: 5 s, 30 s,
/// 64 MiB); only then close.
///
/// A server that closes a socket with unread bytes in its receive buffer,
/// or that receives more after the close, sends a TCP reset, and the reset
/// can destroy its own last response in the client's buffers before the
/// client reads it (#224): a 413 for a body the server refused before
/// reading it, a 403/404/405 answered before the body was read, all arrived
/// as `ECONNRESET`/`WSAECONNABORTED` instead. Reading the rest first leaves
/// nothing unread at the close.
async fn linger(mut stream: tokio::net::TcpStream, bounds: Linger) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if stream.shutdown().await.is_err() {
        return;
    }
    let deadline = tokio::time::Instant::now() + bounds.time;
    let mut buf = vec![0u8; LINGER_BUF];
    let mut read = 0u64;
    while read < bounds.bytes {
        let idle = tokio::time::Instant::now() + bounds.idle;
        match tokio::time::timeout_at(idle.min(deadline), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => read += n as u64,
            // EOF (the client closed), an error, or out of time.
            _ => return,
        }
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

/// `r` with `Connection: close` (an early answer whose body was not read).
fn closing(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    r
}

/// `r` with `Connection: close` when the request declares a body (a
/// non-zero `Content-Length`, or any `Transfer-Encoding`) that this answer
/// leaves unread, so a pooling client does not send its next request on a
/// connection the server will not read again.
fn close_if_body_unread(headers: &HeaderMap, r: Response) -> Response {
    let declared = headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v.to_str().map_or(true, |v| v.trim() != "0"));
    if declared {
        closing(r)
    } else {
        r
    }
}

/// Whether a body read failed on the size limit (not on the transport).
fn is_length_limit(e: &axum::Error) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(x) = cur {
        if x.to_string().contains("length limit exceeded") {
            return true;
        }
        cur = x.source();
    }
    false
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
        let now = Instant::now();
        let idle = self.session_idle;
        let mut s = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        s.map
            .retain(|_, (_, used)| now.duration_since(*used) < idle);
        while s.map.len() >= MAX_SESSIONS {
            // The least recently used goes first.
            let Some(lru) = s
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            s.map.remove(&lru);
        }
        s.map.insert(id.clone(), (protocol, now));
        id
    }

    /// The session's protocol version, marking it used now; `None` for an
    /// unknown, ended or idle-expired one.
    fn session(&self, id: &str) -> Option<&'static str> {
        let now = Instant::now();
        let mut s = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let (protocol, used) = s.map.get_mut(id)?;
        if now.duration_since(*used) >= self.session_idle {
            s.map.remove(id);
            return None;
        }
        *used = now;
        Some(*protocol)
    }

    fn end_session(&self, id: &str) -> bool {
        let mut s = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        s.map.remove(id).is_some()
    }
}

fn server_name() -> (&'static str, &'static str) {
    ("memory-graph", crate::SERVER_VERSION)
}

async fn handle(State(st): State<Arc<McpState>>, req: Request) -> Response {
    if let Some(refused) = st.guard(req.headers()) {
        return close_if_body_unread(req.headers(), refused);
    }
    let session = req
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match *req.method() {
        Method::POST => {}
        Method::DELETE => {
            let r = match session {
                None => plain(StatusCode::BAD_REQUEST, "DELETE needs an Mcp-Session-Id"),
                Some(id) if st.end_session(&id) => StatusCode::OK.into_response(),
                Some(_) => plain(StatusCode::NOT_FOUND, "no such session"),
            };
            return close_if_body_unread(req.headers(), r);
        }
        _ => {
            // GET (a server-initiated SSE stream) is not offered in v1.
            let mut r = plain(
                StatusCode::METHOD_NOT_ALLOWED,
                "this endpoint takes POST (and DELETE to end a session)",
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            return close_if_body_unread(req.headers(), r);
        }
    }
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
        // The body is left unread: the connection must not be reused.
        return closing(plain(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body over 1 MiB",
        ));
    }
    // The whole body is read before any other answer (429 included), so a
    // refusal reaches a client still sending instead of a reset.
    let body: Body = req.into_body();
    let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) if is_length_limit(&e) => {
            return closing(plain(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body over 1 MiB",
            ))
        }
        Err(e) => {
            return closing(plain(
                StatusCode::BAD_REQUEST,
                &format!("reading the request body failed: {e}"),
            ))
        }
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
    // A missing MCP-Protocol-Version is accepted (the session's version
    // applies); one naming another version is refused.
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
    // Taken only now: a request with a bad session gets its own error.
    let Ok(permit) = Arc::clone(&st.inflight).try_acquire_owned() else {
        if let Some(t) = &tool {
            st.ctx.raft.obs.observe_mcp_call(t, "refused");
        }
        let mut r = plain(
            StatusCode::TOO_MANY_REQUESTS,
            "too many MCP requests in flight (serve --mcp-max-inflight); retry",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return r;
    };
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
                st.ctx.raft.obs.observe_mcp_call(t, "internal");
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
