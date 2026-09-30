//! The seven read-only tools (ADR 0005 D2): their definitions (with
//! `inputSchema` and `outputSchema`), argument parsing into a [`ToolCall`],
//! and [`StoreBackend`], which answers a call from any
//! [`StoreRead`](graph_store::StoreRead).
//!
//! Every refusal of a known tool is a tool result with `isError: true`
//! (a tool execution error, SEP-1303), so the model sees the valid values
//! and can correct itself:
//! - arguments that do not fit the `inputSchema` (a wrong type, an unknown
//!   argument, a `limit` out of range, an unknown grain or token class) are
//!   [`ToolError::InvalidParams`], code `invalid_params`;
//! - arguments that fit the schema but name nothing indexed (an org, repo,
//!   language, symbol kind or file that is not there) are
//!   [`ToolError::InvalidArgument`], code `invalid_argument`;
//! - a [`StoreError`] has its own code.
//!
//! Only an unknown tool ([`ToolError::UnknownTool`]) is a JSON-RPC `-32602`.
use graph_core::{NodeKind, SymbolKind, TokenClass};
use graph_store::{Grain, Query, StoreError, StoreRead, SymbolQuery};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// `limit` when a list tool is called without one.
pub const DEFAULT_LIMIT: usize = 50;
/// The largest `limit` a list tool accepts; more is invalid params.
pub const MAX_LIMIT: usize = 500;
/// The largest `limit` of `file_tokens` alone: a slice of at most 20k
/// tokens (ADR 0005 D4), with [`MAX_RESULT_BYTES`] as the backstop.
pub const FILE_TOKENS_MAX_LIMIT: usize = 20_000;
/// The largest `offset` (and line number) accepted.
pub const MAX_OFFSET: u64 = u32::MAX as u64;
/// A list answer whose items serialize to more than this is cut short, with
/// `next_offset` pointing at the first item left out (ADR 0005 D4). At
/// least one item is always kept, so paging always makes progress.
pub const MAX_RESULT_BYTES: usize = 4 << 20;

const GRAINS: &str = "token, symbol, method, class, file, repo, org";
const TOKEN_CLASSES: &str = "identifier, keyword, literal, operator, punctuation, comment, other";

/// `limit` and `offset` of a list tool, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageArgs {
    pub limit: usize,
    pub offset: usize,
}

/// A validated tool call: the tool and its typed arguments.
#[derive(Debug, Clone)]
pub enum ToolCall {
    Describe {
        org: Option<String>,
        repo: Option<String>,
    },
    ListRepos {
        org: Option<String>,
        page: PageArgs,
    },
    /// `query.limit` / `query.offset` are unset; paging is in `page`.
    Search {
        query: Query,
        page: PageArgs,
    },
    /// `query.limit` / `query.offset` are unset; paging is in `page`.
    FindSymbols {
        query: SymbolQuery,
        page: PageArgs,
    },
    FileOutline {
        org: String,
        repo: String,
        path: String,
        page: PageArgs,
    },
    FileTokens {
        org: String,
        repo: String,
        path: String,
        start_line: Option<u32>,
        end_line: Option<u32>,
        page: PageArgs,
    },
    ListFiles {
        org: String,
        repo: String,
        prefix: Option<String>,
        page: PageArgs,
    },
}

/// Why a tool call did not produce a normal result.
#[derive(Debug)]
pub enum ToolError {
    /// The tool does not exist: a JSON-RPC `-32602`.
    UnknownTool(String),
    /// The arguments do not fit the tool's `inputSchema`: an `isError`
    /// result with code `invalid_params`.
    InvalidParams(String),
    /// The arguments are well-formed but name something that is not
    /// indexed: an `isError` result with code `invalid_argument`.
    InvalidArgument(String),
    /// The store failed: an `isError` result with the error's code.
    Store(StoreError),
}

impl From<StoreError> for ToolError {
    fn from(e: StoreError) -> Self {
        ToolError::Store(e)
    }
}

/// What answers tool calls (ADR 0005 D1). `read` returns the tool's
/// `structuredContent`.
pub trait McpBackend {
    fn read(&self, call: &ToolCall) -> Result<Value, ToolError>;
}

/// The typed code of a store error in an `isError` result: the
/// `StoreErrorDetail` kind names of `graph-proto` (`error.rs`), snake case.
pub fn store_error_code(e: &StoreError) -> &'static str {
    match e {
        StoreError::Locked(_) => "locked",
        StoreError::SchemaMismatch { .. } => "schema_mismatch",
        StoreError::LegacyFormat { .. } => "legacy_format",
        StoreError::OpenFailed { .. } => "open_failed",
        StoreError::Rejected(_) => "rejected",
        StoreError::NotUtf8(_) => "not_utf8",
        StoreError::TooLarge(_) => "too_large",
        StoreError::InvalidSpan(_) => "invalid_span",
        StoreError::Corrupt(_) => "corrupt",
        StoreError::Schema(_) => "schema",
        StoreError::Storage(_) => "storage",
        StoreError::SnapshotExpired { .. } => "snapshot_expired",
        StoreError::NotLeader { .. } => "not_leader",
        StoreError::NoLeader { .. } => "no_leader",
        StoreError::Protocol(_) => "protocol",
        StoreError::WrongCluster { .. } => "wrong_cluster",
        StoreError::AlreadyApplied { .. } => "already_applied",
    }
}

/// Whether a store error is a lost connection to a server (a transport
/// error, a deadline, the server unreachable): `RemoteStore` reports those
/// as `Storage` errors naming the lost connection. Retrying may succeed.
fn is_connection_loss(e: &StoreError) -> bool {
    matches!(e, StoreError::Storage(m)
        if m.contains("connection lost") || m.contains(" unavailable: "))
}

/// The `{code, message, retryable[, retry_after_ms]}` body of an `isError`
/// result, or `None` for an unknown tool (a JSON-RPC error instead).
pub fn error_body(e: &ToolError) -> Option<Value> {
    match e {
        ToolError::UnknownTool(_) => None,
        ToolError::InvalidParams(m) => Some(json!({
            "code": "invalid_params",
            "message": m,
            "retryable": false,
        })),
        ToolError::InvalidArgument(m) => Some(json!({
            "code": "invalid_argument",
            "message": m,
            "retryable": false,
        })),
        ToolError::Store(se) => {
            let lost = is_connection_loss(se);
            let mut b = json!({
                "code": if lost { "unavailable" } else { store_error_code(se) },
                "message": se.to_string(),
                "retryable": lost || matches!(se, StoreError::NoLeader { .. } | StoreError::NotLeader { .. }),
            });
            if let StoreError::NoLeader { retry_after_ms } = se {
                b["retry_after_ms"] = json!(retry_after_ms);
            }
            Some(b)
        }
    }
}

// ---------------------------------------------------------------- schemas

fn span_schema() -> Value {
    let n = json!({"type": "integer", "minimum": 0});
    json!({
        "type": ["object", "null"],
        "properties": {
            "start": n, "end": n, "start_line": n, "start_col": n, "end_line": n, "end_col": n
        },
        "required": ["start", "end", "start_line", "start_col", "end_line", "end_col"],
        "additionalProperties": false
    })
}

fn opt_str() -> Value {
    json!({"type": ["string", "null"]})
}

fn symbol_kind_schema(nullable: bool) -> Value {
    let mut e: Vec<Value> = [
        "module", "type", "function", "method", "variable", "constant", "other",
    ]
    .iter()
    .map(|s| json!(s))
    .collect();
    let t = if nullable {
        e.push(Value::Null);
        json!(["string", "null"])
    } else {
        json!("string")
    };
    json!({"type": t, "enum": e})
}

fn token_class_schema(nullable: bool) -> Value {
    let mut e: Vec<Value> = TOKEN_CLASSES.split(", ").map(|s| json!(s)).collect();
    let t = if nullable {
        e.push(Value::Null);
        json!(["string", "null"])
    } else {
        json!("string")
    };
    json!({"type": t, "enum": e})
}

fn hit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "grain": {"type": "string", "enum": GRAINS.split(", ").collect::<Vec<_>>()},
            "org": {"type": "string"},
            "repo": opt_str(),
            "file": opt_str(),
            "language": opt_str(),
            "symbol": opt_str(),
            "symbol_kind": symbol_kind_schema(true),
            "lang_kind": opt_str(),
            "token_class": token_class_schema(true),
            "span": span_schema(),
            "count": {"type": "integer", "minimum": 0},
            "no_symbols": {"type": "boolean"},
            "no_matching_symbol": {"type": "boolean"}
        },
        "required": ["grain", "org", "repo", "file", "language", "symbol", "symbol_kind",
                     "lang_kind", "token_class", "span", "count", "no_symbols", "no_matching_symbol"],
        "additionalProperties": false
    })
}

fn symbol_hit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "org": {"type": "string"},
            "repo": {"type": "string"},
            "file": {"type": "string"},
            "language": opt_str(),
            "name": {"type": "string"},
            "qualified": {"type": "string"},
            "kind": symbol_kind_schema(false),
            "lang_kind": opt_str(),
            "span": span_schema(),
            "owner": {"type": "string"}
        },
        "required": ["org", "repo", "file", "language", "name", "qualified", "kind", "lang_kind", "span"],
        "additionalProperties": false
    })
}

fn repo_info_schema() -> Value {
    let count = json!({"type": "integer", "minimum": 0});
    let counts = json!({"type": "object", "additionalProperties": count});
    json!({
        "type": "object",
        "properties": {
            "org": {"type": "string"},
            "repo": {"type": "string"},
            "files": count,
            "languages": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "files": count, "symbols": count, "tokens": count, "symbol_kinds": counts
                    },
                    "required": ["files", "symbols", "tokens", "symbol_kinds"],
                    "additionalProperties": false
                }
            },
            "token_classes": counts,
            "open_batch": {"type": "boolean"}
        },
        "required": ["org", "repo", "files", "languages", "token_classes", "open_batch"],
        "additionalProperties": false
    })
}

/// `{items, next_offset, stale_possible}` with `items` of `item`.
fn page_schema(item: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {"type": "array", "items": item},
            "next_offset": {"type": ["integer", "null"], "minimum": 0},
            "stale_possible": {"type": "boolean"}
        },
        "required": ["items", "next_offset", "stale_possible"],
        "additionalProperties": false
    })
}

/// An input schema. An optional argument may also be `null` (the same as
/// leaving it out), so its `type` (and `enum`) admit `null`.
fn input(mut props: Value, required: &[&str]) -> Value {
    for (k, p) in props.as_object_mut().expect("properties").iter_mut() {
        if required.contains(&k.as_str()) {
            continue;
        }
        if let Some(t) = p.get("type").and_then(Value::as_str).map(str::to_string) {
            p["type"] = json!([t, "null"]);
        }
        if let Some(e) = p.get_mut("enum").and_then(Value::as_array_mut) {
            e.push(Value::Null);
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false
    })
}

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn paging(props: &mut Value, max: usize) {
    props["limit"] = json!({
        "type": "integer", "minimum": 1, "maximum": max, "default": DEFAULT_LIMIT,
        "description": format!("Return at most this many items (default {DEFAULT_LIMIT}, at most {max})")
    });
    props["offset"] = json!({
        "type": "integer", "minimum": 0, "maximum": MAX_OFFSET, "default": 0,
        "description": "Skip this many items; pass the previous answer's next_offset to get the next page"
    });
}

/// The tool definitions `tools/list` returns, in a fixed order.
pub fn tool_definitions() -> Vec<Value> {
    let org = s("Organisation name (exact)");
    let repo = s("Repository name (exact)");
    let language = s("Only files of this language (case-insensitive, e.g. rust); describe lists the languages present");

    let describe_in = input(json!({"org": org, "repo": repo}), &[]);
    let describe_out = json!({
        "type": "object",
        "properties": {
            "repos": {"type": "array", "items": repo_info_schema()},
            "stale_possible": {"type": "boolean"}
        },
        "required": ["repos", "stale_possible"],
        "additionalProperties": false
    });

    let mut list_repos_p = json!({"org": org});
    paging(&mut list_repos_p, MAX_LIMIT);
    let list_repos_out = page_schema(json!({
        "type": "object",
        "properties": {"org": {"type": "string"}, "repo": {"type": "string"}},
        "required": ["org", "repo"],
        "additionalProperties": false
    }));

    let mut search_p = json!({
        "text": s("Exact token text to find"),
        "grain": {
            "type": "string", "enum": GRAINS.split(", ").collect::<Vec<_>>(), "default": "symbol",
            "description": "Level results are rolled up to: token, symbol (nearest enclosing symbol), method (nearest enclosing method or function), class (nearest enclosing type or impl block), file, repo or org"
        },
        "language": language,
        "org": org,
        "repo": repo,
        "token_class": {
            "type": "string", "enum": TOKEN_CLASSES.split(", ").collect::<Vec<_>>(),
            "description": "Only tokens of this class"
        },
        "symbol_kind": s("With grain symbol, method or class: only symbols of this kind, generic (function, type, ...) or language-specific (struct, trait, ...); describe lists the kinds present")
    });
    paging(&mut search_p, MAX_LIMIT);

    let mut find_p = json!({
        "pattern": s("Exact symbol name, `prefix*` for a prefix, or `*` for every symbol"),
        "kind": s("Only symbols of this kind, generic (function, type, ...) or language-specific (struct, trait, ...)"),
        "language": language,
        "org": org,
        "repo": repo,
        "file": s("Only symbols of this file path (as indexed, relative to the repo)")
    });
    paging(&mut find_p, MAX_LIMIT);

    let file_p = || json!({"org": org, "repo": repo, "path": s("File path as indexed (relative to the repo)")});
    let mut outline_p = file_p();
    paging(&mut outline_p, MAX_LIMIT);
    let mut tokens_p = file_p();
    tokens_p["start_line"] = json!({"type": "integer", "minimum": 1, "maximum": MAX_OFFSET, "description": "First line (1-based) whose tokens are returned"});
    tokens_p["end_line"] = json!({"type": "integer", "minimum": 1, "maximum": MAX_OFFSET, "description": "Last line (1-based, inclusive) whose tokens are returned"});
    paging(&mut tokens_p, FILE_TOKENS_MAX_LIMIT);
    let token_item = json!({
        "type": "object",
        "properties": {
            "text": {"type": "string"},
            "token_class": token_class_schema(true),
            "span": span_schema()
        },
        "required": ["text", "token_class", "span"],
        "additionalProperties": false
    });

    let mut files_p =
        json!({"org": org, "repo": repo, "prefix": s("Only paths starting with this prefix")});
    paging(&mut files_p, MAX_LIMIT);
    let file_item = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "language": opt_str(),
            "has_errors": {"type": "boolean"}
        },
        "required": ["path", "language", "has_errors"],
        "additionalProperties": false
    });

    let def = |name: &str, title: &str, desc: &str, i: Value, o: Value| {
        json!({
            "name": name,
            "title": title,
            "description": desc,
            "inputSchema": i,
            "outputSchema": o,
            "annotations": {
                "title": title,
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        })
    };
    vec![
        def("describe", "Describe the index",
            "What is indexed: per repo, its files, languages, symbol kinds and token classes. Start here to learn the valid org, repo, language and kind values.",
            describe_in, describe_out),
        def("list_repos", "List repositories",
            "The indexed repositories (org and repo names), optionally of one org.",
            input(list_repos_p, &[]), list_repos_out),
        def("search", "Search tokens",
            "Find a token by its exact text and roll the matches up to a grain: the enclosing symbol (default), method, class, file, repo or org, or each token. Symbol, method and class rows carry that symbol's full span.",
            input(search_p, &["text"]), page_schema(hit_schema())),
        def("find_symbols", "Find symbols",
            "Find symbol definitions (functions, types, methods, ...) by name: exact, `prefix*`, or `*` for all.",
            input(find_p, &["pattern"]), page_schema(symbol_hit_schema())),
        def("file_outline", "Outline a file",
            "The symbols defined in one file, in source order, with their kinds and spans.",
            input(outline_p, &["org", "repo", "path"]), page_schema(symbol_hit_schema())),
        def("file_tokens", "Read a file's tokens",
            "The tokens of one file in source order (text, class and exact span), optionally only lines start_line..=end_line.",
            input(tokens_p, &["org", "repo", "path"]), page_schema(token_item)),
        def("list_files", "List files",
            "The indexed files of one repository, optionally under a path prefix.",
            input(files_p, &["org", "repo"]), page_schema(file_item)),
    ]
}

/// The `outputSchema` of `tool`, if it exists.
pub fn output_schema(tool: &str) -> Option<Value> {
    tool_definitions()
        .into_iter()
        .find(|d| d["name"] == tool)
        .map(|d| d["outputSchema"].clone())
}

/// The `inputSchema` of `tool`, if it exists.
pub fn input_schema(tool: &str) -> Option<Value> {
    tool_definitions()
        .into_iter()
        .find(|d| d["name"] == tool)
        .map(|d| d["inputSchema"].clone())
}

// ---------------------------------------------------------------- parsing

struct Args<'a> {
    tool: &'a str,
    map: &'a Map<String, Value>,
}

impl Args<'_> {
    fn bad(&self, msg: String) -> ToolError {
        ToolError::InvalidParams(format!("{}: {msg}", self.tool))
    }

    fn only(&self, allowed: &[&str]) -> Result<(), ToolError> {
        if let Some(k) = self.map.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(self.bad(format!(
                "unknown argument `{k}`; valid arguments: {}",
                allowed.join(", ")
            )));
        }
        Ok(())
    }

    /// A string argument; an empty one is refused like a missing filter
    /// would be meaningless (`--org ""` in the CLI).
    fn opt_str(&self, name: &str) -> Result<Option<String>, ToolError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.is_empty() => {
                Err(self.bad(format!("`{name}` must not be empty")))
            }
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(v) => Err(self.bad(format!("`{name}` must be a string, got {v}"))),
        }
    }

    fn str(&self, name: &str) -> Result<String, ToolError> {
        self.opt_str(name)?
            .ok_or_else(|| self.bad(format!("`{name}` is required")))
    }

    fn opt_uint(&self, name: &str, min: u64, max: u64) -> Result<Option<u64>, ToolError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            // An integral float (5.0) is an integer, as in JSON Schema.
            Some(v) => match v.as_u64().or_else(|| {
                v.as_f64()
                    .filter(|f| f.fract() == 0.0 && (0.0..=max as f64).contains(f))
                    .map(|f| f as u64)
            }) {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                _ => Err(self.bad(format!(
                    "`{name}` must be an integer from {min} to {max}, got {v}"
                ))),
            },
        }
    }

    fn page(&self, max_limit: usize) -> Result<PageArgs, ToolError> {
        let limit = self
            .opt_uint("limit", 1, max_limit as u64)?
            .map_or(DEFAULT_LIMIT, |n| n as usize);
        let offset = self
            .opt_uint("offset", 0, MAX_OFFSET)?
            .map_or(0, |n| n as usize);
        Ok(PageArgs { limit, offset })
    }
}

/// Parse `tools/call`'s `name` and `arguments` into a [`ToolCall`]. An
/// unknown tool, an unknown argument, a wrong type, a `limit` out of
/// range, or an unknown grain or token class is
/// [`ToolError::InvalidParams`], naming the valid values.
pub fn parse_call(name: &str, arguments: &Map<String, Value>) -> Result<ToolCall, ToolError> {
    let a = Args {
        tool: name,
        map: arguments,
    };
    const PAGE: [&str; 2] = ["limit", "offset"];
    let with_page = |fields: &[&'static str]| -> Vec<&'static str> {
        fields.iter().copied().chain(PAGE).collect()
    };
    Ok(match name {
        "describe" => {
            a.only(&["org", "repo"])?;
            ToolCall::Describe {
                org: a.opt_str("org")?,
                repo: a.opt_str("repo")?,
            }
        }
        "list_repos" => {
            a.only(&with_page(&["org"]))?;
            ToolCall::ListRepos {
                org: a.opt_str("org")?,
                page: a.page(MAX_LIMIT)?,
            }
        }
        "search" => {
            a.only(&with_page(&[
                "text",
                "grain",
                "language",
                "org",
                "repo",
                "token_class",
                "symbol_kind",
            ]))?;
            let text = match arguments.get("text") {
                Some(Value::String(t)) => t.clone(),
                Some(v) => return Err(a.bad(format!("`text` must be a string, got {v}"))),
                None => return Err(a.bad("`text` is required".into())),
            };
            let grain = match a.opt_str("grain")? {
                None => Grain::Symbol,
                Some(g) => g
                    .parse::<Grain>()
                    .map_err(|_| a.bad(format!("unknown grain `{g}`; valid values: {GRAINS}")))?,
            };
            let class = match a.opt_str("token_class")? {
                None => None,
                Some(c) => Some(c.parse::<TokenClass>().map_err(|_| {
                    a.bad(format!(
                        "unknown token_class `{c}`; valid values: {TOKEN_CLASSES}"
                    ))
                })?),
            };
            let symbol_kind = a.opt_str("symbol_kind")?;
            if let Some(k) = &symbol_kind {
                if !grain.is_symbolic() {
                    return Err(a.bad(format!(
                        "`symbol_kind` requires grain symbol, method or class (got `{}`)",
                        grain_name(grain)
                    )));
                }
                // A generic kind the grain can never accept would only give
                // `no_matching_symbol` rows: refused, as in the CLI.
                if let Ok(g) = k.to_ascii_lowercase().parse::<SymbolKind>() {
                    let (fits, allowed) = match grain {
                        Grain::Method => (
                            matches!(g, SymbolKind::Method | SymbolKind::Function),
                            "method, function",
                        ),
                        Grain::Class => (
                            matches!(g, SymbolKind::Type | SymbolKind::Other),
                            "type, other",
                        ),
                        _ => (true, ""),
                    };
                    if !fits {
                        return Err(a.bad(format!(
                            "symbol_kind `{k}` can never be a grain {} row (generic kinds there: {allowed}); use grain symbol for any kind",
                            grain_name(grain)
                        )));
                    }
                }
            }
            let mut query = Query::new(text);
            query.grain = grain;
            query.class = class;
            query.language = a.opt_str("language")?;
            query.org = a.opt_str("org")?;
            query.repo = a.opt_str("repo")?;
            query.symbol_kind = symbol_kind;
            ToolCall::Search {
                query,
                page: a.page(MAX_LIMIT)?,
            }
        }
        "find_symbols" => {
            a.only(&with_page(&[
                "pattern", "kind", "language", "org", "repo", "file",
            ]))?;
            let pattern = match arguments.get("pattern") {
                Some(Value::String(p)) => p.clone(),
                Some(v) => return Err(a.bad(format!("`pattern` must be a string, got {v}"))),
                None => return Err(a.bad("`pattern` is required".into())),
            };
            let mut query = SymbolQuery::new(pattern);
            query.kind = a.opt_str("kind")?;
            query.language = a.opt_str("language")?;
            query.org = a.opt_str("org")?;
            query.repo = a.opt_str("repo")?;
            query.file = a.opt_str("file")?;
            ToolCall::FindSymbols {
                query,
                page: a.page(MAX_LIMIT)?,
            }
        }
        "file_outline" => {
            a.only(&with_page(&["org", "repo", "path"]))?;
            ToolCall::FileOutline {
                org: a.str("org")?,
                repo: a.str("repo")?,
                path: a.str("path")?,
                page: a.page(MAX_LIMIT)?,
            }
        }
        "file_tokens" => {
            a.only(&with_page(&[
                "org",
                "repo",
                "path",
                "start_line",
                "end_line",
            ]))?;
            let start_line = a.opt_uint("start_line", 1, MAX_OFFSET)?.map(|n| n as u32);
            let end_line = a.opt_uint("end_line", 1, MAX_OFFSET)?.map(|n| n as u32);
            if let (Some(s), Some(e)) = (start_line, end_line) {
                if s > e {
                    return Err(a.bad(format!("start_line {s} is after end_line {e}")));
                }
            }
            ToolCall::FileTokens {
                org: a.str("org")?,
                repo: a.str("repo")?,
                path: a.str("path")?,
                start_line,
                end_line,
                page: a.page(FILE_TOKENS_MAX_LIMIT)?,
            }
        }
        "list_files" => {
            a.only(&with_page(&["org", "repo", "prefix"]))?;
            ToolCall::ListFiles {
                org: a.str("org")?,
                repo: a.str("repo")?,
                prefix: a.opt_str("prefix")?,
                page: a.page(MAX_LIMIT)?,
            }
        }
        other => {
            return Err(ToolError::UnknownTool(format!(
                "unknown tool `{other}`; tools: {}",
                TOOL_NAMES.join(", ")
            )))
        }
    })
}

/// The tool names, in `tools/list` order.
pub const TOOL_NAMES: [&str; 7] = [
    "describe",
    "list_repos",
    "search",
    "find_symbols",
    "file_outline",
    "file_tokens",
    "list_files",
];

fn grain_name(g: Grain) -> &'static str {
    match g {
        Grain::Token => "token",
        Grain::Symbol => "symbol",
        Grain::Method => "method",
        Grain::Class => "class",
        Grain::File => "file",
        Grain::Repo => "repo",
        Grain::Org => "org",
    }
}

// ---------------------------------------------------------------- backend

/// Counts the reads so far that may have missed acknowledged writes (ADR
/// 0004 D8); `None` for an embedded store, whose answers never do.
pub type StaleCounter = Box<dyn Fn() -> u64 + Send + Sync>;

/// Answers tool calls from a [`StoreRead`]: an embedded store, or a
/// `RemoteStore` (then with a [`StaleCounter`] from its read log).
pub struct StoreBackend {
    store: Box<dyn StoreRead + Send>,
    stale_reads: Option<StaleCounter>,
}

impl StoreBackend {
    /// An embedded store: `stale_possible` is always false.
    pub fn embedded(store: Box<dyn StoreRead + Send>) -> Self {
        Self {
            store,
            stale_reads: None,
        }
    }

    /// A store behind a server: a call is `stale_possible` when the counter
    /// grew while it ran.
    pub fn remote(store: Box<dyn StoreRead + Send>, stale_reads: StaleCounter) -> Self {
        Self {
            store,
            stale_reads: Some(stale_reads),
        }
    }

    /// Check filters against what is indexed in the org/repo scope, like
    /// the CLI does, so a typo lists the valid values instead of answering
    /// nothing.
    fn validate_filters(
        &self,
        org: Option<&str>,
        repo: Option<&str>,
        language: Option<&str>,
        kind: Option<(&str, &str)>,
    ) -> Result<(), ToolError> {
        let infos = self.store.describe(org, repo)?;
        if infos.is_empty() {
            if org.is_some() || repo.is_some() {
                let all = self.store.describe(None, None)?;
                let names: BTreeSet<String> = all
                    .iter()
                    .map(|i| format!("{}/{}", i.org, i.repo))
                    .collect();
                return Err(ToolError::InvalidArgument(format!(
                    "no indexed repo matches org {} repo {}; repos present: {}",
                    org.map_or("(any)".into(), |o| format!("`{o}`")),
                    repo.map_or("(any)".into(), |r| format!("`{r}`")),
                    join(names.iter())
                )));
            }
            return Ok(());
        }
        if let Some(l) = language {
            let langs: BTreeSet<&String> = infos.iter().flat_map(|i| i.languages.keys()).collect();
            if !langs.iter().any(|x| x.eq_ignore_ascii_case(l)) {
                return Err(ToolError::InvalidArgument(format!(
                    "no files of language `{l}` in scope; languages present: {}",
                    join(langs.iter().copied())
                )));
            }
        }
        if let Some((arg, k)) = kind {
            let kinds: BTreeSet<String> =
                infos.iter().flat_map(|i| i.kind_names(language)).collect();
            let known = kinds.iter().any(|x| x.eq_ignore_ascii_case(k));
            if !known && k.to_ascii_lowercase().parse::<SymbolKind>().is_err() {
                return Err(ToolError::InvalidArgument(format!(
                    "no symbols of {arg} `{k}` in scope; kinds present: {}",
                    join(kinds.iter())
                )));
            }
        }
        Ok(())
    }

    /// The repo node of `org`/`repo`, or an `invalid_argument` naming what
    /// is there.
    fn repo_node(&self, org: &str, repo: &str) -> Result<graph_core::Node, ToolError> {
        let orgs = self.store.roots()?;
        let Some(o) = orgs
            .iter()
            .find(|n| n.kind == NodeKind::Org && n.name == org)
        else {
            let names: BTreeSet<&String> = orgs.iter().map(|n| &n.name).collect();
            return Err(ToolError::InvalidArgument(format!(
                "no org `{org}` is indexed; orgs present: {}",
                join(names.into_iter())
            )));
        };
        let repos = self.store.children(o.id)?;
        match repos
            .into_iter()
            .find(|n| n.kind == NodeKind::Repo && n.name == repo)
        {
            Some(r) => Ok(r),
            None => {
                let names: BTreeSet<String> = self
                    .store
                    .children(o.id)?
                    .into_iter()
                    .map(|n| n.name)
                    .collect();
                Err(ToolError::InvalidArgument(format!(
                    "no repo `{repo}` in org `{org}`; repos present: {}",
                    join(names.iter())
                )))
            }
        }
    }

    fn answer(&self, call: &ToolCall) -> Result<Value, ToolError> {
        Ok(match call {
            ToolCall::Describe { org, repo } => {
                self.validate_filters(org.as_deref(), repo.as_deref(), None, None)?;
                let infos = self.store.describe(org.as_deref(), repo.as_deref())?;
                json!({ "repos": infos })
            }
            ToolCall::ListRepos { org, page } => {
                let mut items = Vec::new();
                let mut org_found = false;
                for o in self.store.roots()? {
                    if o.kind != NodeKind::Org || org.as_ref().is_some_and(|x| *x != o.name) {
                        continue;
                    }
                    org_found = true;
                    for r in self.store.children(o.id)? {
                        if r.kind == NodeKind::Repo {
                            items.push(json!({"org": o.name, "repo": r.name}));
                        }
                    }
                }
                if !org_found {
                    if let Some(org) = org {
                        let names: BTreeSet<String> =
                            self.store.roots()?.into_iter().map(|n| n.name).collect();
                        return Err(ToolError::InvalidArgument(format!(
                            "no org `{org}` is indexed; orgs present: {}",
                            join(names.iter())
                        )));
                    }
                }
                page_of(items, *page)
            }
            ToolCall::Search { query, page } => {
                self.validate_filters(
                    query.org.as_deref(),
                    query.repo.as_deref(),
                    query.language.as_deref(),
                    query.symbol_kind.as_deref().map(|k| ("symbol_kind", k)),
                )?;
                let mut q = query.clone();
                q.limit = Some(page.limit + 1);
                q.offset = Some(page.offset);
                let hits = self.store.search(&q)?;
                fetched_page(to_values(&hits), *page)
            }
            ToolCall::FindSymbols { query, page } => {
                self.validate_filters(
                    query.org.as_deref(),
                    query.repo.as_deref(),
                    query.language.as_deref(),
                    query.kind.as_deref().map(|k| ("kind", k)),
                )?;
                let mut q = query.clone();
                q.limit = Some(page.limit + 1);
                q.offset = Some(page.offset);
                let hits = self.store.search_symbols(&q)?;
                fetched_page(to_values(&hits), *page)
            }
            ToolCall::FileOutline {
                org,
                repo,
                path,
                page,
            } => {
                let r = self.repo_node(org, repo)?;
                if !self
                    .store
                    .children(r.id)?
                    .iter()
                    .any(|n| n.kind == NodeKind::File && n.name == *path)
                {
                    return Err(no_file(org, repo, path));
                }
                let mut q = SymbolQuery::new("*");
                q.org = Some(org.clone());
                q.repo = Some(repo.clone());
                q.file = Some(path.clone());
                let mut hits = self.store.search_symbols(&q)?;
                // Source order: by start, the outer symbol before the inner.
                hits.sort_by_key(|h| h.span.map(|s| (s.start, std::cmp::Reverse(s.end))));
                page_of(to_values(&hits), *page)
            }
            ToolCall::FileTokens {
                org,
                repo,
                path,
                start_line,
                end_line,
                page,
            } => {
                self.repo_node(org, repo)?;
                let Some(tokens) = self.store.file_tokens(org, repo, path)? else {
                    return Err(no_file(org, repo, path));
                };
                let items: Vec<Value> = tokens
                    .into_iter()
                    .filter(|t| {
                        let line = t.span.map_or(0, |s| s.start_line);
                        start_line.is_none_or(|s| line >= s) && end_line.is_none_or(|e| line <= e)
                    })
                    .map(|t| json!({"text": t.name, "token_class": t.token_class, "span": t.span}))
                    .collect();
                page_of(items, *page)
            }
            ToolCall::ListFiles {
                org,
                repo,
                prefix,
                page,
            } => {
                let r = self.repo_node(org, repo)?;
                let items: Vec<Value> = self
                    .store
                    .children(r.id)?
                    .into_iter()
                    .filter(|n| n.kind == NodeKind::File)
                    .filter(|n| prefix.as_ref().is_none_or(|p| n.name.starts_with(p.as_str())))
                    .map(|n| json!({"path": n.name, "language": n.language, "has_errors": n.has_errors}))
                    .collect();
                page_of(items, *page)
            }
        })
    }
}

impl McpBackend for StoreBackend {
    fn read(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let before = self.stale_reads.as_ref().map(|c| c());
        let mut out = self.answer(call)?;
        let stale = match (&self.stale_reads, before) {
            (Some(c), Some(b)) => c() > b,
            _ => false,
        };
        out["stale_possible"] = Value::Bool(stale);
        Ok(out)
    }
}

fn no_file(org: &str, repo: &str, path: &str) -> ToolError {
    ToolError::InvalidArgument(format!(
        "file `{path}` is not indexed in {org}/{repo}; list_files lists the files"
    ))
}

fn join<'a>(it: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = it.map(String::as_str).collect();
    if v.is_empty() {
        "(none)".into()
    } else {
        v.join(", ")
    }
}

fn to_values<T: serde::Serialize>(v: &[T]) -> Vec<Value> {
    v.iter()
        .map(|x| serde_json::to_value(x).expect("store types serialize"))
        .collect()
}

/// A page cut from every item (the tool loaded them all).
fn page_of(all: Vec<Value>, page: PageArgs) -> Value {
    let total = all.len();
    let items: Vec<Value> = all.into_iter().skip(page.offset).take(page.limit).collect();
    let has_more = page.offset.saturating_add(items.len()) < total;
    finish(items, page.offset, has_more)
}

/// A page the store already cut, fetched with `limit + 1` rows so one
/// extra row says whether there is more.
fn fetched_page(mut rows: Vec<Value>, page: PageArgs) -> Value {
    let has_more = rows.len() > page.limit;
    rows.truncate(page.limit);
    finish(rows, page.offset, has_more)
}

/// `{items, next_offset}`, cut to [`MAX_RESULT_BYTES`] (keeping at least
/// one item).
fn finish(mut items: Vec<Value>, offset: usize, mut has_more: bool) -> Value {
    let mut bytes = 0usize;
    let mut keep = items.len();
    for (i, it) in items.iter().enumerate() {
        bytes += serde_json::to_string(it).map_or(0, |s| s.len()) + 1;
        if bytes > MAX_RESULT_BYTES && i > 0 {
            keep = i;
            break;
        }
    }
    if keep < items.len() {
        items.truncate(keep);
        has_more = true;
    }
    let next = has_more.then(|| offset + items.len());
    json!({"items": items, "next_offset": next})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_marks_the_next_offset() {
        let all: Vec<Value> = (0..5).map(|i| json!(i)).collect();
        let p = |limit, offset| page_of(all.clone(), PageArgs { limit, offset });
        assert_eq!(p(2, 0), json!({"items": [0, 1], "next_offset": 2}));
        assert_eq!(p(2, 4), json!({"items": [4], "next_offset": null}));
        assert_eq!(
            p(5, 0),
            json!({"items": [0, 1, 2, 3, 4], "next_offset": null})
        );
        assert_eq!(p(2, 9), json!({"items": [], "next_offset": null}));
        let f = fetched_page(
            vec![json!(1), json!(2), json!(3)],
            PageArgs {
                limit: 2,
                offset: 7,
            },
        );
        assert_eq!(f, json!({"items": [1, 2], "next_offset": 9}));
        let f = fetched_page(
            vec![json!(1), json!(2)],
            PageArgs {
                limit: 2,
                offset: 7,
            },
        );
        assert_eq!(f, json!({"items": [1, 2], "next_offset": null}));
    }

    #[test]
    fn an_oversize_page_is_cut_but_keeps_one_item() {
        let big = json!("x".repeat(MAX_RESULT_BYTES / 2 + 10));
        let v = finish(vec![big.clone(), big.clone(), big.clone()], 10, false);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["next_offset"], 11);
        // One item alone over the cap, at index 0, is kept; the next one
        // is cut.
        let huge = json!("x".repeat(MAX_RESULT_BYTES + 10));
        let v = finish(vec![huge.clone(), json!(1)], 4, false);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["next_offset"], 5);
        // Exactly at the cap is kept whole; one byte more is cut.
        let at = |n: usize| json!("x".repeat(n));
        // Each item costs its JSON length (len + 2 quotes) + 1.
        let half = MAX_RESULT_BYTES / 2 - 3;
        let v = finish(vec![at(half), at(half)], 0, false);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["next_offset"], Value::Null);
        let v = finish(vec![at(half), at(half + 1)], 0, false);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["next_offset"], 1);
        let v = finish(vec![huge], 0, false);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["next_offset"], Value::Null);
    }

    fn call(name: &str, args: Value) -> Result<ToolCall, ToolError> {
        parse_call(name, args.as_object().unwrap())
    }

    fn invalid(r: Result<ToolCall, ToolError>) -> String {
        match r {
            Err(ToolError::InvalidParams(m)) => m,
            other => panic!("expected invalid params, got {other:?}"),
        }
    }

    #[test]
    fn arguments_are_validated_with_the_valid_values_listed() {
        let m = invalid(call("search", json!({"text": "x", "grain": "function"})));
        assert!(m.contains(GRAINS), "{m}");
        let m = invalid(call("search", json!({"text": "x", "token_class": "word"})));
        assert!(m.contains(TOKEN_CLASSES), "{m}");
        let m = invalid(call("search", json!({"text": "x", "colour": 1})));
        assert!(m.contains("valid arguments") && m.contains("grain"), "{m}");
        let m = invalid(call("search", json!({"text": "x", "limit": MAX_LIMIT + 1})));
        assert!(m.contains("limit"), "{m}");
        invalid(call("search", json!({"text": "x", "limit": 0})));
        invalid(call("search", json!({"text": "x", "limit": "5"})));
        invalid(call("search", json!({"text": "x", "offset": -1})));
        invalid(call("search", json!({})));
        invalid(call("search", json!({"text": 3})));
        invalid(call("search", json!({"text": "x", "org": ""})));
        invalid(call(
            "search",
            json!({"text": "x", "grain": "file", "symbol_kind": "fn"}),
        ));
        invalid(call(
            "search",
            json!({"text": "x", "grain": "method", "symbol_kind": "type"}),
        ));
        invalid(call(
            "file_tokens",
            json!({"org": "o", "repo": "r", "path": "p", "start_line": 5, "end_line": 4}),
        ));
        invalid(call("file_tokens", json!({"org": "o", "repo": "r"})));
        match call("nope", json!({})) {
            Err(ToolError::UnknownTool(m)) => {
                assert!(m.contains("describe") && m.contains("list_files"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        // The exact refusals of symbol_kind, naming the grain.
        let m = invalid(call(
            "search",
            json!({"text": "x", "grain": "file", "symbol_kind": "fn"}),
        ));
        assert_eq!(
            m,
            "search: `symbol_kind` requires grain symbol, method or class (got `file`)"
        );
        let m = invalid(call(
            "search",
            json!({"text": "x", "grain": "method", "symbol_kind": "Type"}),
        ));
        assert!(
            m.contains("can never be a grain method row (generic kinds there: method, function)"),
            "{m}"
        );
        let m = invalid(call(
            "search",
            json!({"text": "x", "grain": "class", "symbol_kind": "function"}),
        ));
        assert!(
            m.contains("can never be a grain class row (generic kinds there: type, other)"),
            "{m}"
        );
        for (g, k) in [
            ("method", "method"),
            ("method", "function"),
            ("class", "type"),
            ("class", "other"),
            ("symbol", "variable"),
            ("class", "struct"),
        ] {
            call("search", json!({"text": "x", "grain": g, "symbol_kind": k})).unwrap();
        }
        // file_tokens alone takes a limit up to 20k.
        call(
            "file_tokens",
            json!({"org": "o", "repo": "r", "path": "p", "limit": FILE_TOKENS_MAX_LIMIT}),
        )
        .unwrap();
        invalid(call(
            "file_tokens",
            json!({"org": "o", "repo": "r", "path": "p", "limit": FILE_TOKENS_MAX_LIMIT + 1}),
        ));
        invalid(call(
            "list_files",
            json!({"org": "o", "repo": "r", "limit": MAX_LIMIT + 1}),
        ));
        // Integral floats are integers; null is an absent optional.
        match call(
            "list_repos",
            json!({"limit": 5.0, "offset": 2.0, "org": null}),
        )
        .unwrap()
        {
            ToolCall::ListRepos { org, page } => {
                assert_eq!(org, None);
                assert_eq!(
                    page,
                    PageArgs {
                        limit: 5,
                        offset: 2
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        invalid(call("list_repos", json!({"limit": 5.5})));
        invalid(call("list_repos", json!({"offset": MAX_OFFSET + 1})));
        match call("search", json!({"text": "x", "limit": MAX_LIMIT})).unwrap() {
            ToolCall::Search { query, page } => {
                assert_eq!(query.grain, Grain::Symbol, "symbol is the default grain");
                assert_eq!(
                    page,
                    PageArgs {
                        limit: MAX_LIMIT,
                        offset: 0
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        match call("list_repos", json!({})).unwrap() {
            ToolCall::ListRepos { page, .. } => assert_eq!(page.limit, DEFAULT_LIMIT),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_definition_is_read_only_and_every_schema_is_checkable() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            assert_eq!(d["annotations"]["readOnlyHint"], true, "{}", d["name"]);
            // An empty object is valid input for a tool without required
            // arguments; the schema checker accepts every keyword used.
            let req = d["inputSchema"]["required"].as_array().unwrap();
            let r = crate::schema::validate(&d["inputSchema"], &json!({}));
            assert_eq!(r.is_ok(), req.is_empty(), "{}: {r:?}", d["name"]);
            crate::schema::validate(&json!({"type": "object"}), &d["outputSchema"]).unwrap();
        }
    }

    #[test]
    fn store_errors_are_typed_and_no_leader_is_retryable() {
        let b = error_body(&ToolError::Store(StoreError::NoLeader {
            retry_after_ms: 7,
        }))
        .unwrap();
        assert_eq!(b["code"], "no_leader");
        assert_eq!(b["retryable"], true);
        assert_eq!(b["retry_after_ms"], 7);
        let b = error_body(&ToolError::Store(StoreError::Corrupt("x".into()))).unwrap();
        assert_eq!(b["code"], "corrupt");
        assert_eq!(b["retryable"], false);
        assert!(error_body(&ToolError::UnknownTool("x".into())).is_none());
        assert_eq!(
            error_body(&ToolError::InvalidParams("bad".into())).unwrap(),
            json!({"code": "invalid_params", "message": "bad", "retryable": false})
        );
        for m in [
            "server h:1 connection lost: transport error",
            "server h:1 unavailable: connection refused",
        ] {
            let b = error_body(&ToolError::Store(StoreError::Storage(m.into()))).unwrap();
            assert_eq!(b["code"], "unavailable", "{m}");
            assert_eq!(b["retryable"], true);
        }
        let b = error_body(&ToolError::Store(StoreError::Storage("disk".into()))).unwrap();
        assert_eq!(
            (b["code"].as_str(), b["retryable"].as_bool()),
            (Some("storage"), Some(false))
        );
    }

    #[test]
    fn grain_names_are_the_wire_names() {
        for g in GRAINS.split(", ") {
            assert_eq!(grain_name(g.parse().unwrap()), g);
        }
    }
}
