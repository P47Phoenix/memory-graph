//! The MCP endpoint's transport and D4 guards (ADR 0005 D1/D4, epic story
//! 32): off by default, loopback only unless allowed, Origin/Host 403s, the
//! 1 MiB body limit, sessions, the in-flight limit and the call deadline.
//! Tool answers themselves are checked in `graph-mcp`'s differential test.
use graph_client::{ClientConfig, RemoteStore};
use graph_core::Extractor;
use graph_server::mcp::{McpConfig, MAX_BODY_BYTES, REQUEST_TIMEOUT};
use graph_server::testing::mcp_http::{http, HttpReply, McpHttpClient};
use graph_server::testing::TestServer;
use graph_store::open_store;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn loopback() -> McpConfig {
    McpConfig::new("127.0.0.1:0".parse().unwrap())
}

/// A server over a small store with MCP configured by `mcp`.
fn server(d: &tempfile::TempDir, mcp: McpConfig) -> (TestServer, SocketAddr) {
    let db = d.path().join("g.redb");
    {
        let s = open_store(&db, rust()).unwrap();
        s.index_bytes("acme", "geo", "src/lib.rs", b"pub fn origin() {}\n", None)
            .unwrap();
    }
    let srv = TestServer::start_with(&db, rust(), |c| c.mcp = Some(mcp));
    let addr = srv.running().unwrap().mcp_addr.expect("MCP listens");
    (srv, addr)
}

fn post(addr: SocketAddr, headers: &[(&str, &str)], body: &Value) -> HttpReply {
    let mut h = vec![("Content-Type", "application/json")];
    h.extend_from_slice(headers);
    http(addr, "POST", "/mcp", &h, body.to_string().as_bytes()).unwrap()
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
    })
}

fn status_mcp_addr(srv: &TestServer) -> String {
    RemoteStore::connect(ClientConfig::new(srv.endpoint()))
        .unwrap()
        .admin_status()
        .unwrap()
        .mcp_addr
}

#[test]
fn off_by_default() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let srv = TestServer::start(&db, rust());
    assert!(srv.running().unwrap().mcp_addr.is_none());
    assert_eq!(status_mcp_addr(&srv), "", "Admin.Status says MCP is off");
}

#[test]
fn status_reports_the_bound_address() {
    let d = tempfile::tempdir().unwrap();
    let (srv, addr) = server(&d, loopback());
    assert_ne!(addr.port(), 0);
    assert_eq!(status_mcp_addr(&srv), addr.to_string());
}

#[test]
fn a_non_loopback_bind_is_refused_without_allow_remote() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let e = TestServer::try_start_with(&db, rust(), |c| {
        c.mcp = Some(McpConfig::new("0.0.0.0:0".parse().unwrap()))
    })
    .err()
    .expect("refused");
    assert!(e.to_string().contains("--mcp-allow-remote"), "{e}");
    assert!(!db.exists(), "refused before anything is written");
    // With the flag it starts; the Host check is left to the operator's
    // proxy on a non-loopback bind.
    let mut m = McpConfig::new("0.0.0.0:0".parse().unwrap());
    m.allow_remote = true;
    let srv = TestServer::try_start_with(&db, rust(), |c| c.mcp = Some(m)).unwrap();
    let port = srv.running().unwrap().mcp_addr.unwrap().port();
    let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = post(at, &[("Host", "mcp.example.com")], &initialize());
    assert_eq!(r.status, 200, "{r:?}");
}

#[test]
fn a_foreign_origin_gets_403_and_an_allowed_one_passes() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.allow_origins = vec!["http://localhost:6274/".into()];
    let (_srv, addr) = server(&d, m);
    for bad in ["http://evil.example", "null", "http://localhost:6275"] {
        let r = post(addr, &[("Origin", bad)], &initialize());
        assert_eq!(r.status, 403, "{bad}: {r:?}");
    }
    let r = post(addr, &[("Origin", "http://LOCALHOST:6274")], &initialize());
    assert_eq!(r.status, 200, "{r:?}");
    // No Origin: not a browser; accepted.
    assert_eq!(post(addr, &[], &initialize()).status, 200);
    // The default allows no browser origin at all.
    let d2 = tempfile::tempdir().unwrap();
    let (_s2, a2) = server(&d2, loopback());
    let r = post(a2, &[("Origin", "http://localhost:6274")], &initialize());
    assert_eq!(r.status, 403, "{r:?}");
}

#[test]
fn a_host_that_is_not_the_bound_loopback_gets_403() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d, loopback());
    let port = addr.port();
    for bad in [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        format!("localhost.evil.example:{port}"),
        "".to_string(),
    ] {
        let r = post(addr, &[("Host", &bad)], &initialize());
        assert_eq!(r.status, 403, "Host {bad:?}: {r:?}");
    }
    for good in [
        addr.to_string(),
        format!("localhost:{port}"),
        "localhost".into(),
    ] {
        let r = post(addr, &[("Host", &good)], &initialize());
        assert_eq!(r.status, 200, "Host {good:?}: {r:?}");
    }
    // The guards run before the method, too.
    let r = http(addr, "GET", "/mcp", &[("Host", "evil.example")], b"").unwrap();
    assert_eq!(r.status, 403);
}

#[test]
fn a_body_over_1_mib_gets_413() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d, loopback());
    let big = vec![b' '; MAX_BODY_BYTES + 1];
    let r = http(addr, "POST", "/mcp", &[], &big).unwrap();
    assert_eq!(r.status, 413, "{r:?}");
    // A declared length over the limit is refused before the body is read.
    let r = http(
        addr,
        "POST",
        "/mcp",
        &[("Content-Length", &(64u64 << 20).to_string())],
        b"",
    )
    .unwrap();
    assert_eq!(r.status, 413, "{r:?}");
    // Exactly the limit is read (and is not JSON: a parse error, 400).
    let at = vec![b' '; MAX_BODY_BYTES];
    let r = http(addr, "POST", "/mcp", &[], &at).unwrap();
    assert_eq!(r.status, 400, "{r:?}");
    assert_eq!(r.json()["error"]["code"], graph_mcp::PARSE_ERROR);
}

/// #224: an answer given before the body was read (a declared length over
/// the limit, a foreign Host, another path, another method) reaches a
/// client that sends its whole body first and reads only afterwards, as a
/// simple client does. The server closes in stages (RFC 9112 section 9.6):
/// it reads and discards the rest before it closes. Closing with the body
/// unread sends a TCP reset, which on Windows destroyed the answer in the
/// client's buffer (`WSAECONNABORTED`, 10053) every time here, and on a
/// loaded host often enough to fail `a_body_over_1_mib_gets_413`.
#[test]
fn an_early_answer_survives_a_client_that_reads_after_sending() {
    use std::io::{Read, Write};
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d, loopback());
    let big = MAX_BODY_BYTES + (1 << 20);
    for (head, body_len, want) in [
        ("POST /mcp HTTP/1.1\r\nHost: {addr}\r\n", big, 413),
        (
            "POST /mcp HTTP/1.1\r\nHost: evil.example\r\n",
            512 << 10,
            403,
        ),
        ("POST /other HTTP/1.1\r\nHost: {addr}\r\n", 512 << 10, 404),
        ("PUT /mcp HTTP/1.1\r\nHost: {addr}\r\n", 512 << 10, 405),
    ] {
        let head = head.replace("{addr}", &addr.to_string());
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        // No `Connection: close`: a pooling client's request. The answer
        // must say the connection ends, since the body stays unread.
        let req =
            format!("{head}Content-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        // The whole body, which the server answers without reading: it
        // must read it after answering, or this write cannot finish.
        s.write_all(&vec![b' '; body_len])
            .unwrap_or_else(|e| panic!("{want}: sending the body: {e}"));
        // Give the server every chance to close before this reads.
        std::thread::sleep(Duration::from_millis(300));
        let mut out = Vec::new();
        s.read_to_end(&mut out)
            .unwrap_or_else(|e| panic!("{want}: reading the answer: {e}"));
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with(&format!("HTTP/1.1 {want} ")),
            "{want}: {text}"
        );
        let head = text.split("\r\n\r\n").next().unwrap().to_ascii_lowercase();
        assert!(head.contains("\r\nconnection: close"), "{want}: {text}");
    }
}

/// An early answer (a 413 for a declared length over the limit, no body
/// sent yet), then the connection is left open; `None` if the answer did
/// not come.
fn early_413(addr: SocketAddr) -> Option<std::net::TcpStream> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n",
        64u64 << 20
    );
    s.write_all(req.as_bytes()).unwrap();
    // The server answers and half-closes at once: EOF after the answer.
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok()?;
    String::from_utf8_lossy(&out)
        .starts_with("HTTP/1.1 413 ")
        .then_some(s)
}

/// Send `chunk` every `gap` until a write fails (the server has closed:
/// its reset comes back); `(elapsed, bytes sent)` then, or `None` if every
/// write still went through after `limit`.
fn closed_within(
    s: &mut std::net::TcpStream,
    chunk: &[u8],
    gap: Duration,
    limit: Duration,
) -> Option<(Duration, usize)> {
    use std::io::Write;
    let t0 = std::time::Instant::now();
    let mut sent = 0;
    while t0.elapsed() < limit {
        if s.write_all(chunk).is_err() {
            return Some((t0.elapsed(), sent));
        }
        sent += chunk.len();
        std::thread::sleep(gap);
    }
    None
}

/// #224 review: the staged close is bounded. After an early answer the
/// server keeps reading what the client sends, but closes once the client
/// has been silent for `idle`, once `time` has passed however it trickles,
/// and once `bytes` were read however fast it sends; each bound is checked
/// alone, shortened through `testing_linger`.
#[test]
fn the_staged_close_is_bounded() {
    use graph_server::mcp::Linger;
    let long = Duration::from_secs(600);
    let run = |bounds: Linger| {
        let d = tempfile::tempdir().unwrap();
        let mut m = loopback();
        m.testing_linger = bounds;
        let (srv, addr) = server(&d, m);
        (d, srv, early_413(addr).expect("the early 413"))
    };

    // Idle: a byte now is still read; then silence past `idle`, and the
    // connection is gone.
    let (_d, _srv, mut s) = run(Linger {
        idle: Duration::from_millis(300),
        time: long,
        bytes: u64::MAX,
    });
    assert!(
        closed_within(
            &mut s,
            b" ",
            Duration::from_millis(50),
            Duration::from_millis(200)
        )
        .is_none(),
        "the server still reads within the idle time"
    );
    std::thread::sleep(Duration::from_millis(1500));
    let r = closed_within(
        &mut s,
        b" ",
        Duration::from_millis(100),
        Duration::from_secs(10),
    );
    assert!(r.is_some(), "still open 1.5 s after a 300 ms idle bound");

    // Time: a byte every 50 ms is never idle, but `time` ends it.
    let (_d, _srv, mut s) = run(Linger {
        idle: long,
        time: Duration::from_secs(1),
        bytes: u64::MAX,
    });
    let r = closed_within(
        &mut s,
        b" ",
        Duration::from_millis(50),
        Duration::from_secs(20),
    );
    let (took, _) = r.expect("still open 20 s past a 1 s time bound");
    assert!(took >= Duration::from_millis(800), "closed after {took:?}");

    // Bytes: sending as fast as it can, closed after about `bytes`
    // (plus what the socket buffers hold).
    let (_d, _srv, mut s) = run(Linger {
        idle: long,
        time: long,
        bytes: 256 << 10,
    });
    let r = closed_within(
        &mut s,
        &[b' '; 64 << 10],
        Duration::ZERO,
        Duration::from_secs(20),
    );
    let (_, sent) = r.expect("still open 20 s past a 256 KiB byte bound");
    assert!(sent < 64 << 20, "{sent} bytes read past a 256 KiB bound");
}

#[test]
fn sessions_are_issued_and_checked() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d, loopback());
    let mut c = McpHttpClient::new(addr);
    assert_eq!(c.protocol, graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0]);
    let r = c.request("tools/list", json!({}));
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 7, "{r}");
    let r = c.request(
        "tools/call",
        json!({"name": "search", "arguments": {"text": "origin"}}),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(
        r["result"]["structuredContent"]["items"][0]["symbol"], "origin",
        "{r}"
    );
    // Without a session: 400; an unknown one: 404.
    let list = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"});
    assert_eq!(post(addr, &[], &list).status, 400);
    assert_eq!(post(addr, &[("Mcp-Session-Id", "nope")], &list).status, 404);
    // Another protocol version than the session's: 400.
    let r = post(
        addr,
        &[
            ("Mcp-Session-Id", &c.session),
            ("MCP-Protocol-Version", "2024-11-05"),
        ],
        &list,
    );
    assert_eq!(r.status, 400, "{r:?}");
    // Negotiation is graph-mcp's: an old revision gets the latest.
    let mut old = initialize();
    old["params"]["protocolVersion"] = json!("2024-11-05");
    let r = post(addr, &[], &old);
    assert_eq!(
        r.json()["result"]["protocolVersion"],
        graph_mcp::SUPPORTED_PROTOCOL_VERSIONS[0]
    );
    // A notification or a response gets 202 and no body.
    let r = c.post(
        &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
    );
    assert_eq!((r.status, r.body.as_str()), (202, ""));
    // GET (no server-initiated stream in v1): 405; another path: 404.
    let r = http(addr, "GET", "/mcp", &[], b"").unwrap();
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST, DELETE"));
    assert_eq!(
        http(addr, "POST", "/other", &[], b"{}").unwrap().status,
        404
    );
    // DELETE ends the session.
    let r = http(
        addr,
        "DELETE",
        "/mcp",
        &[("Mcp-Session-Id", &c.session)],
        b"",
    )
    .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(c.post(&list).status, 404);
}

#[test]
fn requests_past_max_inflight_get_429() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.max_inflight = 1;
    m.testing_call_delay = Some(Duration::from_millis(1500));
    let (_srv, addr) = server(&d, m);
    let c = McpHttpClient::new(addr);
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let slow = {
        let (s, p) = (c.session.clone(), c.protocol.clone());
        let msg = msg.clone();
        std::thread::spawn(move || {
            post(
                addr,
                &[("Mcp-Session-Id", &s), ("MCP-Protocol-Version", &p)],
                &msg,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let r = c.post(&msg);
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.header("retry-after"), Some("1"));
    // A refused tools/call is counted as `refused` for its tool.
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "search", "arguments": {"text": "x"}}});
    assert_eq!(c.post(&call).status, 429);
    assert_eq!(slow.join().unwrap().status, 200);
    assert_eq!(c.post(&msg).status, 200, "the slot is free again");
    let metrics = metrics(&_srv);
    assert!(
        metrics.contains("mg_mcp_tool_calls_total{tool=\"search\",outcome=\"refused\"} 1"),
        "{metrics}"
    );
}

/// Mutation survivors (epic story 34): `initialize` over HTTP names the
/// server and its version; a chunked body (no Content-Length) past 1 MiB
/// gets 413 and one cut short gets 400, not the same answer.
#[test]
fn server_info_and_chunked_body_errors() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d, loopback());
    let r = post(addr, &[], &initialize());
    let info = &r.json()["result"]["serverInfo"];
    assert_eq!(info["name"], "memory-graph", "{info}");
    assert_eq!(info["version"], graph_server::SERVER_VERSION, "{info}");

    let raw = |body: &[u8]| {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        s.write_all(head.as_bytes()).unwrap();
        let _ = s.write_all(body);
        let _ = s.shutdown(std::net::Shutdown::Write);
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    };
    // Over the limit in 64 KiB chunks.
    let chunk = vec![b' '; 64 * 1024];
    let mut big = Vec::new();
    for _ in 0..17 {
        big.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
        big.extend(&chunk);
        big.extend(b"\r\n");
    }
    big.extend(b"0\r\n\r\n");
    let out = raw(&big);
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    // A chunk size line that is not hex: the body read fails.
    let out = raw(b"zz\r\n{}\r\n0\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    assert!(out.contains("reading the request body failed"), "{out}");
}

fn metrics(srv: &TestServer) -> String {
    RemoteStore::connect(ClientConfig::new(srv.endpoint()))
        .unwrap()
        .admin_metrics()
        .unwrap()
}

/// Epic story 34: per-tool call counters with every outcome a call can
/// have short of a timeout or a 429 (both tested above): `ok`, `error`
/// (an isError result), `rejected` (a JSON-RPC error), and an unknown tool
/// name counted as `unknown`.
#[test]
fn per_tool_counters_cover_each_outcome() {
    let d = tempfile::tempdir().unwrap();
    let (srv, addr) = server(&d, loopback());
    let mut c = McpHttpClient::new(addr);
    let call = |c: &mut McpHttpClient, name: &str, args: Value| {
        c.request("tools/call", json!({"name": name, "arguments": args}))
    };
    for _ in 0..2 {
        let r = call(&mut c, "search", json!({"text": "origin"}));
        assert_eq!(r["result"]["isError"], false, "{r}");
    }
    let r = call(
        &mut c,
        "search",
        json!({"text": "x", "language": "klingon"}),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    // Invalid arguments are a tool error too (an isError result).
    let r = call(&mut c, "search", json!({"text": "x", "limit": 501}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    // `arguments` that are not an object: a JSON-RPC error.
    let r = c.request("tools/call", json!({"name": "search", "arguments": [1]}));
    assert!(r.get("error").is_some(), "{r}");
    let r = call(&mut c, "drop_tables", json!({}));
    assert!(r.get("error").is_some(), "{r}");
    let r = call(&mut c, "describe", json!({}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    // Not a tool call: not counted.
    c.request("tools/list", json!({}));
    let m = metrics(&srv);
    let lines: Vec<&str> = m
        .lines()
        .filter(|l| l.starts_with("mg_mcp_tool_calls_total{"))
        .collect();
    let mut want = vec![
        "mg_mcp_tool_calls_total{tool=\"describe\",outcome=\"ok\"} 1",
        "mg_mcp_tool_calls_total{tool=\"search\",outcome=\"error\"} 2",
        "mg_mcp_tool_calls_total{tool=\"search\",outcome=\"ok\"} 2",
        "mg_mcp_tool_calls_total{tool=\"search\",outcome=\"rejected\"} 1",
        "mg_mcp_tool_calls_total{tool=\"unknown\",outcome=\"rejected\"} 1",
    ];
    let mut got = lines.clone();
    got.sort();
    want.sort();
    assert_eq!(got, want, "{m}");
}

#[test]
fn a_call_past_the_deadline_times_out() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.call_timeout = Duration::from_millis(200);
    m.testing_call_delay = Some(Duration::from_millis(1000));
    let (srv, addr) = server(&d, m);
    let mut c = McpHttpClient::new(addr);
    let r = c.request("tools/call", json!({"name": "describe", "arguments": {}}));
    // The wire contract: -32001 (REQUEST_TIMEOUT).
    assert_eq!(r["error"]["code"], -32001, "{r}");
    assert_eq!(REQUEST_TIMEOUT, -32001);
    let metrics = RemoteStore::connect(ClientConfig::new(srv.endpoint()))
        .unwrap()
        .admin_metrics()
        .unwrap();
    assert!(
        metrics.contains("mg_mcp_tool_calls_total{tool=\"describe\",outcome=\"timeout\"} 1"),
        "{metrics}"
    );
}

/// Refusals under load arrive as HTTP answers, not connection resets:
/// 40 concurrent calls with a real (padded) body against one slot each get
/// 200 or 429, and a bad session still gets its own 404, not 429.
#[test]
fn refusals_under_load_are_received_with_a_real_body() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.max_inflight = 1;
    m.testing_call_delay = Some(Duration::from_millis(300));
    let (_srv, addr) = server(&d, m);
    let c = McpHttpClient::new(addr);
    // A valid request padded to ~256 KiB with whitespace.
    let mut body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    body.push_str(&" ".repeat(256 << 10));
    let body = std::sync::Arc::new(body);
    let calls: Vec<_> = (0..40)
        .map(|_| {
            let (s, p, b) = (c.session.clone(), c.protocol.clone(), body.clone());
            std::thread::spawn(move || {
                http(
                    addr,
                    "POST",
                    "/mcp",
                    &[
                        ("Content-Type", "application/json"),
                        ("Mcp-Session-Id", &s),
                        ("MCP-Protocol-Version", &p),
                    ],
                    b.as_bytes(),
                )
            })
        })
        .collect();
    let bad = http(
        addr,
        "POST",
        "/mcp",
        &[("Mcp-Session-Id", "nope")],
        body.as_bytes(),
    )
    .unwrap();
    assert_eq!(bad.status, 404, "{bad:?}");
    let (mut ok, mut busy) = (0, 0);
    for t in calls {
        let r = t.join().unwrap().expect("an HTTP answer, not a reset");
        match r.status {
            200 => ok += 1,
            429 => busy += 1,
            s => panic!("unexpected {s}: {r:?}"),
        }
    }
    assert!(ok >= 1 && busy >= 1, "ok {ok}, 429 {busy}");
    // A body over the limit, really sent, is answered 413 too.
    let big = vec![b' '; MAX_BODY_BYTES + 4096];
    let r = http(addr, "POST", "/mcp", &[], &big).unwrap();
    assert_eq!(r.status, 413, "{r:?}");
    assert_eq!(r.header("connection"), Some("close"));
}

/// An idle session ends after the idle timeout (404: initialize again).
#[test]
fn idle_sessions_expire() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.session_idle = Duration::from_millis(300);
    let (_srv, addr) = server(&d, m);
    let c = McpHttpClient::new(addr);
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    assert_eq!(c.post(&list).status, 200);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(c.post(&list).status, 404);
}

/// SESSION_IDLE counts from the last use, not from initialize: a session
/// used more often than the idle timeout lives on; left alone it ends,
/// and a new initialize works.
#[test]
fn session_idle_counts_from_the_last_use() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.session_idle = Duration::from_millis(1500);
    let (_srv, addr) = server(&d, m);
    let c = McpHttpClient::new(addr);
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(c.post(&list).status, 200, "used within the idle time");
    }
    std::thread::sleep(Duration::from_millis(2000));
    assert_eq!(c.post(&list).status, 404, "idle past session_idle");
    let fresh = McpHttpClient::new(addr);
    assert_eq!(fresh.post(&list).status, 200);
}

/// Whether the server closes `s` (EOF or an error on read) within `limit`.
fn server_closes_within(s: &mut std::net::TcpStream, limit: Duration) -> bool {
    use std::io::Read;
    let t0 = std::time::Instant::now();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut buf = [0u8; 4096];
    while t0.elapsed() < limit {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return true,
        }
    }
    false
}

/// One keep-alive request on `s`, its response read through (the body is
/// small, so one read past the headers is enough for the status line).
fn keep_alive_initialize(s: &mut std::net::TcpStream, addr: SocketAddr) -> String {
    use std::io::{Read, Write};
    let body = initialize().to_string();
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 8192];
    let n = s.read(&mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// #234: a client that trickles a request's headers is cut off once the
/// header timeout passes.
#[test]
fn a_slow_header_sender_is_closed_after_the_header_timeout() {
    use std::io::Write;
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.header_timeout = Duration::from_millis(400);
    let (_srv, addr) = server(&d, m);
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(b"POST /mcp HTTP/1.1\r\nHost: ").unwrap();
    let t0 = std::time::Instant::now();
    assert!(
        server_closes_within(&mut s, Duration::from_secs(10)),
        "never closed"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    // A normal request still works.
    assert_eq!(post(addr, &[], &initialize()).status, 200);
}

/// #234: past `max_connections` a new connection is closed at once; once a
/// held one closes, connections are served again.
#[test]
fn connections_past_the_cap_are_closed_at_once() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.max_connections = 2;
    let (_srv, addr) = server(&d, m);
    let mut a = std::net::TcpStream::connect(addr).unwrap();
    let mut b = std::net::TcpStream::connect(addr).unwrap();
    // Both are being served (a keep-alive request each answers).
    assert!(keep_alive_initialize(&mut a, addr).starts_with("HTTP/1.1 200"));
    assert!(keep_alive_initialize(&mut b, addr).starts_with("HTTP/1.1 200"));
    let mut c = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        server_closes_within(&mut c, Duration::from_secs(5)),
        "a third connection is closed"
    );
    drop(a);
    // The slot comes back once the server has seen the close.
    let t0 = std::time::Instant::now();
    loop {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        use std::io::Write;
        let body = initialize().to_string();
        let ok = write!(
            s,
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .is_ok()
            && {
                use std::io::Read;
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = [0u8; 64];
                matches!(s.read(&mut buf), Ok(n) if n > 0 && buf[..n].starts_with(b"HTTP/1.1 200"))
            };
        if ok {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "slot never freed");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(b);
}

/// #234: a keep-alive connection with no request in flight is closed after
/// the idle timeout, counted from its last response.
#[test]
fn an_idle_keep_alive_connection_is_closed() {
    let d = tempfile::tempdir().unwrap();
    let mut m = loopback();
    m.idle_timeout = Duration::from_millis(400);
    let (_srv, addr) = server(&d, m);
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    assert!(keep_alive_initialize(&mut s, addr).starts_with("HTTP/1.1 200"));
    let t0 = std::time::Instant::now();
    assert!(
        server_closes_within(&mut s, Duration::from_secs(10)),
        "never closed"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
}

#[test]
fn zero_connection_limits_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("g.redb");
    let mut m = loopback();
    m.max_connections = 0;
    let e = TestServer::try_start_with(&db, rust(), |c| c.mcp = Some(m))
        .err()
        .expect("refused");
    assert!(e.to_string().contains("--mcp-max-connections"), "{e}");
}
