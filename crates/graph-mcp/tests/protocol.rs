//! Protocol tests (ADR 0005 test plan 2): framing, batches, ids,
//! notifications, malformed JSON, unknown methods, the lifecycle and
//! `initialize` version negotiation, the stdio loop, and a proptest over
//! request bytes.

use graph_mcp::{
    serve_stdio, McpBackend, McpServer, ToolCall, ToolError, INVALID_PARAMS, INVALID_REQUEST,
    METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR, SUPPORTED_PROTOCOL_VERSIONS,
};
use proptest::prelude::*;
use serde_json::{json, Value};

/// A backend that answers every call with an empty page, or fails with
/// the error it was built with.
struct Fake(Option<fn() -> ToolError>);

impl McpBackend for Fake {
    fn read(&self, _: &ToolCall) -> Result<Value, ToolError> {
        match self.0 {
            Some(e) => Err(e()),
            None => Ok(json!({"items": [], "next_offset": null, "stale_possible": false})),
        }
    }
}

fn server() -> McpServer<Fake> {
    McpServer::new(Fake(None), "memory-graph", "test")
}

fn send(s: &mut McpServer<Fake>, raw: &str) -> Option<Value> {
    s.handle_bytes(raw.as_bytes())
        .map(|r| serde_json::from_str(&r).expect("replies are JSON"))
}

fn init(s: &mut McpServer<Fake>) {
    let r = send(
        s,
        &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0], "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}})
        .to_string(),
    )
    .unwrap();
    assert!(r["result"].is_object(), "{r}");
}

fn code(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("no error: {r}"))
}

#[test]
fn initialize_negotiates_one_of_the_supported_versions() {
    for v in SUPPORTED_PROTOCOL_VERSIONS {
        let mut s = server();
        let r = send(
            &mut s,
            &json!({"jsonrpc": "2.0", "id": "a", "method": "initialize",
                    "params": {"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(r["id"], "a");
        assert_eq!(r["result"]["protocolVersion"], v);
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(r["result"]["serverInfo"]["name"], "memory-graph");
        assert_eq!(s.protocol_version(), Some(v));
        assert!(!s.client_initialized());
        assert!(send(
            &mut s,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .is_none());
        assert!(s.client_initialized());
        // A second initialize is refused.
        let r = send(
            &mut s,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": v}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(code(&r), INVALID_REQUEST);
    }
}

#[test]
fn an_unsupported_version_gets_the_latest_supported_one() {
    // MCP lifecycle: the server answers with a version it supports; the
    // client decides whether to go on.
    for v in ["2024-11-05", "1.0.0", "", "2099-01-01"] {
        let mut s = server();
        let r = send(
            &mut s,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": v}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25", "{r}");
        assert_eq!(s.protocol_version(), Some("2025-11-25"));
    }
    // A missing or non-string version is still invalid params, exactly.
    for params in [
        "{}",
        r#"{"protocolVersion":20250618}"#,
        r#"{"protocolVersion":null}"#,
    ] {
        let mut s = server();
        let r = send(
            &mut s,
            &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#),
        )
        .unwrap();
        assert_eq!(
            r,
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602,
                   "message": "initialize needs a string `protocolVersion`"}})
        );
        assert_eq!(s.protocol_version(), None);
    }
}

#[test]
fn exact_error_codes_and_objects() {
    // The constants are the JSON-RPC and MCP numbers.
    assert_eq!(
        [
            PARSE_ERROR,
            INVALID_REQUEST,
            METHOD_NOT_FOUND,
            INVALID_PARAMS,
            NOT_INITIALIZED
        ],
        [-32700, -32600, -32601, -32602, -32002]
    );
    let mut s = server();
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    assert_eq!(
        r,
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32002,
               "message": "`tools/list` before `initialize`: send initialize first"}})
    );
    init(&mut s);
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#).unwrap();
    assert_eq!(
        r,
        json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32601,
               "message": "method `nope` not found; this server has initialize, ping, tools/list and tools/call"}})
    );
    let r = send(&mut s, "[]").unwrap();
    assert_eq!(
        r,
        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600,
               "message": "JSON-RPC batches are not supported (MCP 2025-06-18 and later send one message at a time)"}})
    );
    let r = send(&mut s, "{").unwrap();
    assert_eq!(r["id"], Value::Null);
    assert_eq!(r["error"]["code"], -32700);
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("parse error: "));
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope"}}"#,
    )
    .unwrap();
    assert_eq!(r["error"]["code"], -32602);
    assert_eq!(r["id"], 3);
}

#[test]
fn only_initialize_and_ping_before_initialize() {
    let mut s = server();
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
    assert_eq!(r["result"], json!({}));
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
    assert_eq!(code(&r), NOT_INITIALIZED);
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"describe"}}"#,
    )
    .unwrap();
    assert_eq!(code(&r), NOT_INITIALIZED);
    // An initialized notification before initialize changes nothing.
    assert!(send(
        &mut s,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    )
    .is_none());
    assert!(!s.client_initialized());
}

#[test]
fn framing_errors() {
    let mut s = server();
    init(&mut s);
    // Malformed JSON: a parse error with a null id.
    let r = send(&mut s, "{not json").unwrap();
    assert_eq!(code(&r), PARSE_ERROR);
    assert_eq!(r["id"], Value::Null);
    let r = send(&mut s, "\u{FEFF}").unwrap();
    assert_eq!(code(&r), PARSE_ERROR);
    // Batches are refused, empty or not.
    for b in ["[]", r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#] {
        let r = send(&mut s, b).unwrap();
        assert_eq!(code(&r), INVALID_REQUEST, "{b}");
        assert!(r["error"]["message"].as_str().unwrap().contains("batch"));
    }
    // Not an object.
    for m in ["1", "\"x\"", "null", "true"] {
        assert_eq!(code(&send(&mut s, m).unwrap()), INVALID_REQUEST, "{m}");
    }
    // Ids: strings and integers are echoed; null, floats, objects refused.
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":"x-1","method":"ping"}"#).unwrap();
    assert_eq!(r["id"], "x-1");
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":-7,"method":"ping"}"#).unwrap();
    assert_eq!(r["id"], -7);
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#,
    )
    .unwrap();
    assert_eq!(r["id"], json!(18446744073709551615u64));
    for bad in ["null", "1.5", "{}", "[1]", "true"] {
        let r = send(
            &mut s,
            &format!(r#"{{"jsonrpc":"2.0","id":{bad},"method":"ping"}}"#),
        )
        .unwrap();
        assert_eq!(code(&r), INVALID_REQUEST, "{bad}");
        assert_eq!(r["id"], Value::Null);
    }
    // jsonrpc must be "2.0".
    let r = send(&mut s, r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap();
    assert_eq!(code(&r), INVALID_REQUEST);
    assert_eq!(r["id"], 1);
    let r = send(&mut s, r#"{"id":1,"method":"ping"}"#).unwrap();
    assert_eq!(code(&r), INVALID_REQUEST);
    // method must be a string; params an object.
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":5}"#).unwrap();
    assert_eq!(code(&r), INVALID_REQUEST);
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":1}"#).unwrap();
    assert_eq!(code(&r), INVALID_REQUEST);
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#,
    )
    .unwrap();
    assert_eq!(code(&r), INVALID_PARAMS);
    // Unknown method.
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#,
    )
    .unwrap();
    assert_eq!(code(&r), METHOD_NOT_FOUND);
    assert_eq!(r["id"], 9);
}

#[test]
fn notifications_and_client_responses_get_no_reply() {
    let mut s = server();
    init(&mut s);
    for m in [
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"user"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/unknown"}"#,
        r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","method":"ping","params":[1]}"#,
        r#"{"jsonrpc":"2.0","id":4,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":5,"error":{"code":1,"message":"x"}}"#,
    ] {
        assert!(send(&mut s, m).is_none(), "{m}");
    }
    // Still serving after a cancellation.
    let r = send(&mut s, r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#).unwrap();
    assert_eq!(r["result"], json!({}));
}

#[test]
fn tools_call_errors() {
    let mut s = server();
    init(&mut s);
    let call = |s: &mut McpServer<Fake>, p: Value| {
        send(
            s,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": p}).to_string(),
        )
        .unwrap()
    };
    assert_eq!(code(&call(&mut s, json!({}))), INVALID_PARAMS);
    assert_eq!(code(&call(&mut s, json!({"name": 3}))), INVALID_PARAMS);
    let r = call(&mut s, json!({"name": "drop_tables"}));
    assert_eq!(code(&r), INVALID_PARAMS);
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .contains("find_symbols"));
    assert_eq!(
        code(&call(&mut s, json!({"name": "describe", "arguments": [1]}))),
        INVALID_PARAMS
    );
    // Absent arguments are an empty object.
    let r = call(&mut s, json!({"name": "list_repos"}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    // Bad arguments of a known tool are a tool execution error the model
    // can read (SEP-1303), listing the valid values.
    for args in [
        json!({"text": "x", "grain": "function"}),
        json!({"text": "x", "limit": 501}),
        json!({"text": "x", "colour": 1}),
    ] {
        let r = call(&mut s, json!({"name": "search", "arguments": args}));
        assert_eq!(r["result"]["isError"], true, "{r}");
        let body: Value =
            serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["code"], "invalid_params");
        assert_eq!(body["retryable"], false);
    }
    let r = call(
        &mut s,
        json!({"name": "file_tokens", "arguments": {"org": "o", "repo": "r", "path": "p", "start_line": 3, "end_line": 2}}),
    );
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("start_line 3 is after end_line 2"));
    // Store errors are isError results with a typed code.
    let mut s = McpServer::new(
        Fake(Some(|| {
            ToolError::Store(graph_store::StoreError::NoLeader { retry_after_ms: 50 })
        })),
        "memory-graph",
        "test",
    );
    init(&mut s);
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"describe","arguments":{}}}"#,
    )
    .unwrap();
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"].get("structuredContent").is_none());
    let body: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["code"], "no_leader");
    assert_eq!(body["retryable"], true);
    assert_eq!(body["retry_after_ms"], 50);
}

#[test]
fn tools_list_declares_both_schemas_for_all_seven() {
    let mut s = server();
    init(&mut s);
    let r = send(
        &mut s,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
    )
    .unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
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
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object");
        assert_eq!(t["outputSchema"]["type"], "object");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
    }
}

#[test]
fn the_stdio_loop_writes_one_line_per_reply_and_nothing_else() {
    let mut s = server();
    let input = format!(
        "{}\n\n   \r\n{}\r\n{}\n{}\n\u{0}\u{ff}garbage\n{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[1]}}),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_files","arguments":{"org":"o","repo":"r"}}}"#,
        // The last line has no newline: still read.
        r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
    );
    let mut bytes = input.into_bytes();
    // A line that is not UTF-8.
    bytes.extend_from_slice(b"\n\xff\xfe\n");
    let mut out = Vec::new();
    serve_stdio(&mut s, &bytes[..], &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with('\n'));
    let replies: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let ids: Vec<Value> = replies.iter().map(|r| r["id"].clone()).collect();
    assert_eq!(
        ids,
        [
            json!(1),
            json!(2),
            json!(3),
            Value::Null,
            json!(4),
            Value::Null
        ]
    );
    assert_eq!(
        replies[0]["result"]["protocolVersion"],
        SUPPORTED_PROTOCOL_VERSIONS[1]
    );
    assert_eq!(code(&replies[3]), PARSE_ERROR);
    assert_eq!(code(&replies[5]), PARSE_ERROR);
}

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| json!(f)),
        prop_oneof![
            Just("2.0".to_string()),
            Just("initialize".to_string()),
            Just("tools/call".to_string()),
            Just("search".to_string()),
            Just("2025-11-25".to_string()),
            ".{0,8}"
        ]
        .prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(
                prop_oneof![
                    Just("jsonrpc".to_string()),
                    Just("id".to_string()),
                    Just("method".to_string()),
                    Just("params".to_string()),
                    Just("name".to_string()),
                    Just("arguments".to_string()),
                    Just("protocolVersion".to_string()),
                    Just("text".to_string()),
                    Just("limit".to_string()),
                    "[a-z]{1,6}"
                ],
                inner,
                0..5
            )
            .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// Any reply is one JSON-RPC 2.0 object with exactly one of result/error,
/// and a request with a valid id gets exactly that id back.
fn check_reply(input: Option<&Value>, reply: Option<String>) {
    let Some(reply) = reply else {
        // No reply only for a message without an id (a notification or a
        // client response) that parsed as an object.
        let v = input.expect("unparseable input always gets a parse error");
        let o = v.as_object().expect("a non-object always gets an error");
        assert!(!o.contains_key("id") || !o.contains_key("method"), "{v}");
        return;
    };
    assert!(!reply.contains('\n'), "one line");
    let r: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(r["jsonrpc"], "2.0");
    assert!(r.get("result").is_some() != r.get("error").is_some(), "{r}");
    if let Some(e) = r.get("error") {
        assert!(e["code"].is_i64() && e["message"].is_string(), "{r}");
    }
    if let Some(id) = input.and_then(|v| v.get("id")) {
        if (id.is_string() || id.is_i64() || id.is_u64())
            && input.unwrap().get("jsonrpc") == Some(&json!("2.0"))
        {
            assert_eq!(&r["id"], id, "{r}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn arbitrary_bytes_never_panic_and_always_frame(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let mut s = server();
        init(&mut s);
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        let reply = s.handle_bytes(&bytes);
        check_reply(parsed.as_ref(), reply);
    }

    #[test]
    fn arbitrary_json_messages_never_panic_and_always_frame(v in arb_json(), initialized in any::<bool>()) {
        let mut s = server();
        if initialized {
            init(&mut s);
        }
        let raw = serde_json::to_vec(&v).unwrap();
        let reply = s.handle_bytes(&raw);
        check_reply(Some(&v), reply);
    }

    /// The stdio framing (epic story 34): any stream of lines, JSON-RPC
    /// messages mixed with random bytes, `\n` and `\r\n` endings, blank
    /// lines and a last line without a newline, gets one JSON-RPC line per
    /// reply and nothing else, never more replies than non-blank lines.
    #[test]
    fn arbitrary_stdio_streams_frame_one_reply_per_line(
        lines in prop::collection::vec(
            prop_oneof![
                arb_json().prop_map(|v| serde_json::to_vec(&v).unwrap()),
                Just(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#.to_vec()),
                Just(br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"describe"}}"#.to_vec()),
                prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 0..64),
                Just(Vec::new()),
            ],
            0..24,
        ),
        crlf in any::<bool>(),
        trailing_newline in any::<bool>(),
    ) {
        let mut input = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            input.extend_from_slice(l);
            if i + 1 < lines.len() || trailing_newline {
                input.extend_from_slice(if crlf { b"\r\n" } else { b"\n" });
            }
        }
        let mut s = server();
        let mut out = Vec::new();
        serve_stdio(&mut s, &input[..], &mut out).unwrap();
        let text = String::from_utf8(out).expect("stdout is UTF-8");
        prop_assert!(text.is_empty() || text.ends_with('\n'));
        let non_blank = input
            .split(|b| *b == b'\n')
            .filter(|l| !l.trim_ascii().is_empty())
            .count();
        let replies: Vec<&str> = text.lines().collect();
        prop_assert!(replies.len() <= non_blank, "{} replies to {} lines", replies.len(), non_blank);
        for r in replies {
            let v: Value = serde_json::from_str(r).expect("each line is JSON");
            prop_assert_eq!(&v["jsonrpc"], &json!("2.0"));
            prop_assert!(v.get("result").is_some() != v.get("error").is_some(), "{}", v);
        }
    }

    #[test]
    fn arbitrary_tool_arguments_are_answered_or_refused(
        name in prop_oneof![Just("describe"), Just("list_repos"), Just("search"), Just("find_symbols"),
                            Just("file_outline"), Just("file_tokens"), Just("list_files"), Just("x")],
        args in arb_json(),
    ) {
        let mut s = server();
        init(&mut s);
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": name, "arguments": args}});
        let reply: Value = serde_json::from_str(&s.handle_bytes(msg.to_string().as_bytes()).unwrap()).unwrap();
        prop_assert_eq!(&reply["id"], &json!(1));
        let known = name != "x";
        let obj = args.is_null() || args.is_object();
        if let Some(res) = reply.get("result") {
            prop_assert!(known && obj, "{}", reply);
            if res["isError"] == json!(false) {
                // Accepted input fits the tool's inputSchema.
                let schema = graph_mcp::tools::input_schema(name).unwrap();
                let a = if args.is_null() { json!({}) } else { args.clone() };
                prop_assert!(graph_mcp::schema::validate(&schema, &a).is_ok(), "{} accepted {}", name, args);
            } else {
                let body: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
                prop_assert_eq!(&body["code"], &json!("invalid_params"));
                prop_assert_eq!(&body["retryable"], &json!(false));
            }
        } else {
            prop_assert!(!known || !obj, "{}", reply);
            prop_assert!(reply["error"]["code"] == json!(INVALID_PARAMS), "{}", reply);
        }
    }
}
