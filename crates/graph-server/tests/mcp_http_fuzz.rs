//! Fuzzing the MCP endpoint's HTTP request handling (ADR 0005 test plan
//! items 2 and 6, epic story 34): random bytes on the socket, and random
//! methods, paths, headers (Host, Origin, Content-Length, session and
//! protocol headers) and body sizes and contents, against one live
//! in-process server. Every request must get an HTTP answer from the
//! documented set (or a closed connection for bytes that are not HTTP),
//! never a hang, and the server must keep serving a well-formed session
//! afterwards. A panic in a handler would surface as a dropped connection
//! on a well-formed request, which fails the case.
use graph_core::Extractor;
use graph_server::mcp::McpConfig;
use graph_server::testing::mcp_http::McpHttpClient;
use graph_server::testing::TestServer;
use graph_store::open_store;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

fn server(d: &tempfile::TempDir) -> (TestServer, SocketAddr) {
    let db = d.path().join("g.redb");
    {
        let s = open_store(&db, rust()).unwrap();
        s.index_bytes("acme", "geo", "src/lib.rs", b"pub fn origin() {}\n", None)
            .unwrap();
    }
    let mut m = McpConfig::new("127.0.0.1:0".parse().unwrap());
    m.allow_origins = vec!["http://localhost:6274".into()];
    let srv = TestServer::start_with(&db, rust(), |c| c.mcp = Some(m));
    let addr = srv.running().unwrap().mcp_addr.expect("MCP listens");
    (srv, addr)
}

/// Write `raw`, read everything back. `Err` only on a read timeout (a
/// hang). With `half_close` the write half is closed after `raw`, for a
/// request that may be incomplete (hyper then closes, maybe without an
/// answer); without it the request must be complete, since `Connection:
/// close` ends it.
fn exchange(addr: SocketAddr, raw: &[u8], half_close: bool) -> Result<Vec<u8>, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    // The server may answer and close before reading everything.
    let _ = s.write_all(raw);
    if half_close {
        let _ = s.shutdown(Shutdown::Write);
    }
    let mut out = Vec::new();
    match s.read_to_end(&mut out) {
        Ok(_) => Ok(out),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Err(format!("no answer within 20 s: {e}"))
        }
        // A connection error after what was read: the answer, if any, is
        // judged by the caller (a complete request must have one; the
        // server reads the rest of a body it refused before it closes, so
        // a reset cannot destroy that answer: #224).
        Err(_) => Ok(out),
    }
}

/// `raw` with every `from` replaced by `to`.
fn replace(raw: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// The status code of the first HTTP answer in `raw`, if any.
fn status(raw: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(raw);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "HTTP/1.1").then_some(())?;
    parts.next()?.parse().ok()
}

/// Every status the endpoint documents, plus hyper's own 400/431/505 for
/// malformed HTTP.
const KNOWN: [u16; 11] = [200, 202, 400, 403, 404, 405, 408, 413, 429, 431, 505];

fn arb_header() -> impl Strategy<Value = (String, String)> {
    let token = "[ -~]{0,40}";
    prop_oneof![
        token.prop_map(|v| ("Host".to_string(), v)),
        prop_oneof![
            Just("http://localhost:6274".to_string()),
            Just("http://evil.example".to_string()),
            Just("null".to_string()),
            token.prop_map(String::from),
        ]
        .prop_map(|v| ("Origin".to_string(), v)),
        prop_oneof![
            (0u64..3_000_000).prop_map(|n| n.to_string()),
            token.prop_map(String::from),
        ]
        .prop_map(|v| ("Content-Length".to_string(), v)),
        prop_oneof![
            Just(SESSION.to_string()),
            "[0-9a-f]{0,40}".prop_map(String::from)
        ]
        .prop_map(|v| ("Mcp-Session-Id".to_string(), v)),
        prop_oneof![
            Just("2025-11-25".to_string()),
            Just("2025-06-18".to_string()),
            token.prop_map(String::from),
        ]
        .prop_map(|v| ("MCP-Protocol-Version".to_string(), v)),
        prop_oneof![
            Just("application/json".to_string()),
            token.prop_map(String::from)
        ]
        .prop_map(|v| ("Content-Type".to_string(), v)),
        prop_oneof![Just("chunked".to_string()), token.prop_map(String::from)]
            .prop_map(|v| ("Transfer-Encoding".to_string(), v)),
        ("[A-Za-z-]{1,20}", token).prop_map(|(k, v)| (k, v.to_string())),
    ]
}

fn arb_body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..512),
        // JSON-RPC-shaped messages with random parts.
        (
            prop_oneof![
                Just("initialize"),
                Just("ping"),
                Just("tools/list"),
                Just("tools/call"),
                Just("notifications/initialized"),
                Just("nope"),
            ],
            prop_oneof![
                Just(json!(1)),
                Just(json!("a")),
                Just(json!(null)),
                Just(json!(1.5))
            ],
            prop_oneof![
                Just(json!({})),
                Just(json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "f", "version": "0"}})),
                Just(json!({"name": "search", "arguments": {"text": "origin"}})),
                Just(json!({"name": "file_tokens", "arguments": {"org": "acme", "repo": "geo", "path": "src/lib.rs", "limit": 0}})),
                Just(json!({"name": "search", "arguments": [1]})),
                Just(json!([])),
            ],
            any::<bool>(),
        )
            .prop_map(|(m, id, params, with_id)| {
                let mut v = json!({"jsonrpc": "2.0", "method": m, "params": params});
                if with_id {
                    v["id"] = id;
                }
                v.to_string().into_bytes()
            }),
        // Near and past the 1 MiB limit.
        (1_048_000usize..1_049_000).prop_map(|n| vec![b' '; n]),
    ]
}

/// A well-formed session request (POST /mcp, loopback Host, the session's
/// headers, a matching Content-Length) most of the time, with each part
/// replaced by a random one now and then, plus 0-3 random headers.
fn arb_request() -> impl Strategy<Value = (Vec<u8>, bool)> {
    (
        prop_oneof![
            8 => Just("POST"),
            1 => prop_oneof![
                Just("GET"),
                Just("DELETE"),
                Just("PUT"),
                Just("OPTIONS"),
                Just("HEAD")
            ],
        ],
        prop_oneof![
            8 => Just("/mcp"),
            1 => prop_oneof![Just("/"), Just("/mcp/"), Just("/MCP"), Just("/mcp?x=1")],
        ],
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.8),
        prop::collection::vec(arb_header(), 0..4),
        arb_body(),
    )
        .prop_map(|(method, path, host_ok, in_session, headers, body)| {
            let mut head = format!("{method} {path} HTTP/1.1\r\n");
            if host_ok {
                head.push_str("Host: localhost\r\n");
            }
            if in_session {
                head.push_str(&format!(
                    "Mcp-Session-Id: {SESSION}\r\nMCP-Protocol-Version: 2025-11-25\r\n\
                     Content-Type: application/json\r\n"
                ));
            }
            let mut has_len = false;
            for (k, v) in headers {
                has_len |= k.eq_ignore_ascii_case("content-length")
                    || k.eq_ignore_ascii_case("transfer-encoding");
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            if !has_len {
                head.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            head.push_str("Connection: close\r\n\r\n");
            let mut raw = head.into_bytes();
            raw.extend(body);
            // Complete: the body is exactly what Content-Length says.
            (raw, !has_len)
        })
}
/// Stands for the live session id in generated requests; replaced before
/// sending (a generated `DELETE` may end the session, and the next case
/// then runs in a new one).
const SESSION: &str = "@@SESSION@@";

/// A live session still works (the server survived); a new one replaces
/// it if the case ended it.
fn alive(c: &RefCell<McpHttpClient>) -> Result<(), TestCaseError> {
    let ended = c
        .borrow()
        .post(&json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}))
        .status
        == 404;
    if ended {
        let addr = c.borrow().addr;
        *c.borrow_mut() = McpHttpClient::new(addr);
    }
    let r = c.borrow_mut().request(
        "tools/call",
        json!({"name": "search", "arguments": {"text": "origin"}}),
    );
    prop_assert_eq!(&r["result"]["isError"], &Value::Bool(false), "{}", r);
    Ok(())
}

#[test]
fn random_bytes_never_hang_or_kill_the_endpoint() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d);
    let c = RefCell::new(McpHttpClient::new(addr));
    let mut runner = TestRunner::new(Config {
        cases: 300,
        ..Config::default()
    });
    runner
        .run(
            &prop_oneof![
                prop::collection::vec(any::<u8>(), 0..2048),
                // A valid request line, then garbage.
                prop::collection::vec(any::<u8>(), 0..512).prop_map(|g| {
                    let mut v = b"POST /mcp HTTP/1.1\r\nHost: localhost\r\n".to_vec();
                    v.extend(g);
                    v
                }),
            ],
            |raw| {
                let out = exchange(addr, &raw, true).map_err(TestCaseError::fail)?;
                if let Some(s) = status(&out) {
                    prop_assert!(KNOWN.contains(&s), "status {} for {:?}", s, raw);
                }
                alive(&c)
            },
        )
        .unwrap_or_else(|e| match e {
            // The input can be a 1 MiB body: report only the reason.
            proptest::test_runner::TestError::Fail(why, _) => panic!("{why}"),
            e => panic!("{e:?}"),
        });
}

#[test]
fn random_requests_get_a_documented_answer() {
    let d = tempfile::tempdir().unwrap();
    let (_srv, addr) = server(&d);
    let c = RefCell::new(McpHttpClient::new(addr));
    // Status -> count (0: no answer), to check the generator reaches
    // every guard rather than bouncing off the first.
    let seen = RefCell::new(std::collections::BTreeMap::<u16, u32>::new());
    let mut runner = TestRunner::new(Config {
        cases: 400,
        ..Config::default()
    });
    runner
        .run(&arb_request(), |(raw, complete)| {
            let raw = replace(&raw, SESSION.as_bytes(), c.borrow().session.as_bytes());
            let out = exchange(addr, &raw, !complete).map_err(TestCaseError::fail)?;
            let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
            let s = status(&out);
            // Only a request with a random Content-Length or
            // Transfer-Encoding may end without an answer.
            let Some(s) = s else {
                *seen.borrow_mut().entry(0).or_insert(0) += 1;
                prop_assert!(!complete, "no answer to a complete request: {}", head);
                return alive(&c);
            };
            *seen.borrow_mut().entry(s).or_insert(0) += 1;
            prop_assert!(KNOWN.contains(&s), "status {} for {}", s, head);
            if s == 200 && head.starts_with("POST ") {
                let text = String::from_utf8_lossy(&out);
                let (h, body) = text.split_once("\r\n\r\n").unwrap();
                prop_assert!(
                    h.to_ascii_lowercase()
                        .contains("content-type: application/json"),
                    "{}",
                    h
                );
                // A 200 carries one JSON-RPC response (plain or chunked).
                let body = body.trim();
                if !body.is_empty() && !h.to_ascii_lowercase().contains("chunked") {
                    let v: Value = serde_json::from_str(body)
                        .map_err(|e| TestCaseError::fail(format!("{e}: {body}")))?;
                    prop_assert_eq!(&v["jsonrpc"], "2.0", "{}", v);
                }
            }
            alive(&c)
        })
        .unwrap_or_else(|e| match e {
            // The input can be a 1 MiB body: report only the reason.
            proptest::test_runner::TestError::Fail(why, _) => panic!("{why}"),
            e => panic!("{e:?}"),
        });
    let seen = seen.into_inner();
    eprintln!("statuses: {seen:?}");
    for s in [200, 202, 400, 403, 404, 405, 413] {
        assert!(seen.contains_key(&s), "no {s} in {seen:?}");
    }
}
