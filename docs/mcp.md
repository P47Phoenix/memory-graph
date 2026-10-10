# MCP: the memory graph for AI assistants

`memory-graph mcp` serves the [Model Context Protocol](https://modelcontextprotocol.io) over stdio, so an assistant (Claude Code, Claude Desktop, or any MCP client) can search the graph, find symbols, describe repos and outline files. A running `memory-graph serve` can also serve the same tools over streamable HTTP (`--mcp-listen`, [below](#http-inside-serve)). The design is [ADR 0005](adr/0005-mcp.md).

- **Read-only.** There are no tools that write.
- **stdio is the default and the recommended way.** The client starts `memory-graph mcp` itself; no port is opened. The HTTP endpoint inside `serve` is opt-in, off by default and loopback only.
- **No MCP SDK.** The protocol code is hand-written (`crates/graph-mcp`), so the pure-Rust gate stays clean.

```text
+----------------------------------------------------------------------+
| WARNING: The MCP endpoint has no authentication. Anyone who can      |
| reach it can read every indexed source token. Keep it on loopback    |
| or behind an authenticating proxy until #105.                        |
+----------------------------------------------------------------------+
```

Over stdio this is fine: only the client that started the process can talk to it. It matters for the HTTP endpoint (`serve --mcp-listen`), which is why that one is off by default and refuses a non-loopback address unless told otherwise.

## Quick start

Index something first, then check the command works from a terminal:

```sh
memory-graph --db /abs/path/graph.redb index --org me --repo myproj ./myproj
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"sh","version":"0"}}}' \
  | memory-graph --db /abs/path/graph.redb mcp
```

It prints one line of JSON (the `initialize` result) on stdout, a startup line on stderr, and exits when stdin closes.

## Choosing the target

`mcp` uses the same target flags as every other command:

| You have | Run | Notes |
|---|---|---|
| a database file | `memory-graph --db /abs/path/graph.redb mcp` | Opens the file in this process and holds its lock while running: an `index` of the same file waits, then fails with `Locked`. Use a server if you index while the assistant is connected. |
| a `memory-graph serve` | `memory-graph --server 127.0.0.1:7000 mcp` | Several nodes as `h1:7000,h2:7000`. `--read linearizable` for reads that see every acknowledged write (default `local`). `MEMORY_GRAPH_SERVER` / `MEMORY_GRAPH_READ` also work. |

Always give an **absolute** `--db` path: the client decides the working directory of the process it starts.

## Client configuration

### Claude Code

From a shell:

```sh
claude mcp add memory-graph -- memory-graph --db /abs/path/graph.redb mcp
# or against a server:
claude mcp add memory-graph -- memory-graph --server 127.0.0.1:7000 mcp
```

Or check it into a project as `.mcp.json`:

```json
{
  "mcpServers": {
    "memory-graph": {
      "command": "memory-graph",
      "args": ["--db", "/abs/path/graph.redb", "mcp"]
    }
  }
}
```

`claude mcp list` shows whether it connected; `/mcp` inside Claude Code lists its tools.

### Claude Desktop

Edit `claude_desktop_config.json` (Settings > Developer > Edit Config; on macOS `~/Library/Application Support/Claude/`, on Windows `%APPDATA%\Claude\`) and restart Claude Desktop:

```json
{
  "mcpServers": {
    "memory-graph": {
      "command": "/abs/path/to/memory-graph",
      "args": ["--db", "/abs/path/graph.redb", "mcp"]
    }
  }
}
```

On Windows give the full path to `memory-graph.exe` as `command` (clients do not always search `PATH` for it), with escaped backslashes: `"C:\\tools\\memory-graph.exe"`, `"C:\\data\\graph.redb"`. The same holds for Claude Code's `.mcp.json` on Windows. A launcher that is itself a script (an `npx`-based one, or anything installed as a `.cmd`) cannot be started directly by some clients; wrap it as `"command": "cmd", "args": ["/c", "C:\\path\\launcher.cmd", ...]`. `memory-graph.exe` is a plain executable and needs no wrapper. For a server, the args are `["--server", "127.0.0.1:7000", "mcp"]`, optionally with `"--read", "linearizable"` before `mcp`.

### Other clients

Any MCP client with a stdio transport works: the command is `memory-graph`, the arguments are the target flags followed by `mcp`.

## HTTP inside `serve`

`memory-graph serve ... --mcp-listen 127.0.0.1:7071` also serves the tools at `http://127.0.0.1:7071/mcp` (the MCP streamable HTTP transport), on a port of its own: the gRPC port must reach the other cluster members, this one stays on loopback. Port 0 picks a free port; `serve` then prints `mcp on http://<addr>/mcp` before its `listening on` line. Without `--mcp-listen` no MCP endpoint listens.

```text
+----------------------------------------------------------------------+
| WARNING: The MCP endpoint has no authentication. Anyone who can      |
| reach it can read every indexed source token. Keep it on loopback    |
| or behind an authenticating proxy until #105.                        |
+----------------------------------------------------------------------+
```

| Flag (TOML key in `serve --config`) | Default | Meaning |
|---|---|---|
| `--mcp-listen HOST:PORT` (`mcp-listen`) | off | Serve MCP here. |
| `--mcp-allow-remote` (`mcp-allow-remote`) | off | Allow a non-loopback `--mcp-listen` (without it `serve` refuses to start). Logs a warning at every start. |
| `--mcp-allow-origin URL` (`mcp-allow-origin = [..]`) | none | A browser `Origin` accepted, exactly (`http://localhost:6274`); repeatable. |
| `--mcp-read local\|linearizable` (`mcp-read`) | `local` | How the tools read: this node's replica (a follower answers from its own copy and says `stale_possible` when it may lag), or after the leader's read barrier (`no_leader` without a quorum). |
| `--mcp-max-inflight N` (`mcp-max-inflight`) | 16 | Requests served at once; more get HTTP 429 with `Retry-After: 1`. |
| `--mcp-max-connections N` (`mcp-max-connections`) | 256 | Open connections at once; a connection past it is closed as soon as it is accepted. |
| `--mcp-header-timeout DURATION` (`mcp-header-timeout`) | 20s | How long a client may take to send a request's headers before its connection is closed. It also bounds the wait for the next request on a keep-alive connection (hyper arms the same timer there). |
| `--mcp-idle-timeout DURATION` (`mcp-idle-timeout`) | the header timeout | A keep-alive connection with no request in flight for this long is closed. A value above `--mcp-header-timeout` is refused at start, since it could never take effect. |

Guards (ADR 0005 D4), in the order they apply:
- **Host.** On a loopback address, a `Host` header other than `localhost` or the bound address (with the bound port, if it names a port) gets **403**. This stops DNS rebinding: a web page cannot reach the endpoint through a name of its own.
- **With `--mcp-allow-remote` the Host check is skipped.** The endpoint is then reached through names the operator's proxy chooses, which the server cannot know. DNS-rebinding protection then rests on the `Origin` allowlist (a browser always sends `Origin` on these requests, so a page on another site gets 403) and on the operator's authenticating proxy. `serve` says so in its warning at every start.
- **Origin.** A request carrying an `Origin` not listed by `--mcp-allow-origin` gets **403**. Requests without an `Origin` (command-line and desktop clients) pass.
- **Size.** A body over 1 MiB gets **413** (with `Connection: close`). A 429 and the session and JSON answers are sent after the whole body was read. An answer sent before the body was read (413, and the 403/404/405 refusals of a request with a body) carries `Connection: close` and is followed by a staged close (RFC 9112 section 9.6): the server stops sending, then reads and discards what the client still sends (for up to 30 s or 64 MiB, until 5 s pass with nothing) before it closes. Either way the client that is still sending gets the answer instead of a connection reset. An answer is at most about 4 MiB: a longer list is cut short with `next_offset`.
- **Load.** Past `--mcp-max-inflight` requests at once: **429** (checked after the session, so a bad session still gets its 400/404). A call that runs past 30 s is answered with JSON-RPC error `-32001` (its read finishes in the background and still holds its slot until then).
- **Connections (#234).** At most `--mcp-max-connections` (256) are open at once. A connection past the cap is closed as soon as it is accepted, before anything is read, so the client sees a bare TCP close with no HTTP answer and should back off and retry. These connections do not count against `--mcp-max-inflight`. A client that has not sent a whole request's headers within `--mcp-header-timeout` (20 s) is disconnected (slowloris); its staged close is cut to 2 s, since no answer of ours needs protecting. A keep-alive connection with no request in flight for `--mcp-idle-timeout` (the header timeout by default) is closed gracefully; a request in flight, including one whose body is still arriving, is never cut. A connection keeps its slot through its staged close, so a connection closed after a refusal can hold one for up to the linger bounds (5 s idle, 30 s in all). The cap is global, so on a non-loopback bind one host can take every slot. Before exposing the endpoint with `--mcp-allow-remote`, put it behind a proxy that authenticates and limits connections per client.
- **Read-only.** There are no write tools.
- **No TLS.** As for gRPC (ADR 0004): put a TLS-terminating, authenticating proxy in front if the endpoint must leave the host.

The transport: `POST /mcp` with one JSON-RPC message. `initialize` (sent without a session) answers with an `Mcp-Session-Id` header; every later message carries it (**400** without it, **404** for an unknown or ended session: initialize again). A session holds only the negotiated protocol version; a request whose `MCP-Protocol-Version` header names another gets **400**. A request without that header is accepted and served with the session's version (lenient, for clients of revisions that did not send it). A request is answered with `application/json`; a notification gets **202** with no body. `GET /mcp` is **405**: this version opens no server-to-client stream (no SSE). `DELETE /mcp` with the session header ends the session. At most 1024 sessions are kept: when a new one would pass that, the least recently used is ended. A session unused for 30 minutes ends too. An ended session's next request gets 404; initialize again.

Every tool reads through the node's own gRPC Store service in process, so answers, `stale_possible`, linearizable reads and errors (for example `unavailable` while a snapshot is installed) are the same as a gRPC client's on that node.

`memory-graph --server <node> cluster status` shows an `mcp` line (`http://<addr>/mcp`, `off`, or, for a wildcard bind such as `0.0.0.0`, "bound on all interfaces" with the port, since `0.0.0.0` is not an address a client can use); `cluster status --json` has `mcp_addr` (the bound address, empty when off). `serve` prints the same on start. `health` is unchanged (one `SERVING` line). `/metrics` counts calls as `mg_mcp_tool_calls_total{tool, outcome}`, where `outcome` is `ok`, `error` (an `isError` result), `rejected` (a JSON-RPC error, such as an unknown tool or invalid arguments), `timeout` (past the 30 s per-call deadline), `refused` (a 429 past `--mcp-max-inflight`) or `internal` (the call itself failed). A tool name the server does not know is counted as `tool="unknown"`, so a client cannot grow the label set.

A client that speaks streamable HTTP is configured with the URL, for example in Claude Code:

```sh
memory-graph serve --data-dir /data --mcp-listen 127.0.0.1:7071
claude mcp add --transport http memory-graph http://127.0.0.1:7071/mcp
```

## Protocol

- MCP revisions **2025-11-25** and **2025-06-18** (the latest stable one at build time and the one before it). A client asking for another `protocolVersion` gets a normal `initialize` result carrying 2025-11-25, as the MCP lifecycle prescribes; the client then disconnects if it cannot speak that. A missing or non-string `protocolVersion` is JSON-RPC error `-32602`.
- stdio: one JSON-RPC 2.0 message per line on stdin, one reply per line on stdout (HTTP: one message per POST, see above). Batches are refused (`-32600`), as MCP dropped them in 2025-06-18. Ids are strings or integers.
- Methods: `initialize`, `ping`, `tools/list`, `tools/call`; notifications `notifications/initialized` and `notifications/cancelled` (accepted; requests are answered one at a time, so a cancelled request has already finished). Before `initialize` only `initialize` and `ping` are answered (`-32002` otherwise).
- **stdout carries only protocol messages.** Logs and errors go to stderr.

## Tools

All seven are read-only (`readOnlyHint: true`), address things by org, repo and path names (never node ids), declare an `inputSchema` and an `outputSchema`, and answer with `structuredContent` plus the same JSON as a text block.

| Tool | Arguments | Answer |
|---|---|---|
| `describe` | `org?`, `repo?` | `{repos, stale_possible}`: per repo its files, languages, symbol kinds and token classes, `encodings` (files per non-UTF-8 source encoding) and `lossy` (the same as `describe --json`) |
| `list_repos` | `org?`, paging | items `{org, repo}` |
| `search` | `text` (one whole token, exact and case-sensitive), `grain?` (`token`, `symbol` (default), `method`, `class`, `file`, `repo`, `org`), `language?`, `org?`, `repo?`, `token_class?`, `symbol_kind?`, paging | items as in `search --json`: `{grain, org, repo, file, language, symbol, symbol_kind, lang_kind, token_class, span, name_pos, count, no_symbols, no_matching_symbol}`, plus `encoding`/`lossy` for a non-UTF-8 file |
| `find_symbols` | `pattern` (`name`, `prefix*`, `*`), `kind?`, `language?`, `org?`, `repo?`, `file?`, `exact_case?` (default `false`: ASCII letters match in either case), paging. No infix match ([what matches](guide/querying.md#what-matches-whole-tokens-prefixes-no-infix)) | items as in `symbols --json`, with `name_pos` (without the CLI's flat `name_line`/`name_col`) |
| `file_outline` | `org`, `repo`, `path`, paging | the file's symbols in source order |
| `file_tokens` | `org`, `repo`, `path`, `start_line?`, `end_line?`, paging | items `{text, token_class, span}` of the tokens starting on those lines |
| `list_files` | `org`, `repo`, `prefix?`, paging | items `{path, language, has_errors}`, plus `encoding`/`lossy` for a non-UTF-8 file |

A span is `{start, end, start_line, start_col, end_line, end_col}`: byte offsets `[start, end)` and 1-based lines and columns, as in the CLI's `--json`.

**Declaration position.** `find_symbols` items, and `search` rows of the `symbol`, `method` and `class` grains that picked a symbol, carry `name_pos` `{byte, line, col}`: where the symbol's name is, which can be below the span start when attributes or decorators come first (`[Serializable]` on line 1, `class Shape` on line 2: line 2). It is the first identifier token inside the span equal to the name, else the span start ([ADR 0010](adr/0010-symbol-lookup-and-positions.md) D3; see [Declaration position](guide/querying.md#declaration-position)).

**Case.** `find_symbols` matches names case-insensitively by default, folding ASCII letters only (`widget` finds `Widget`; non-ASCII letters match exactly). Pass `exact_case: true` for case-sensitive matching ([ADR 0010](adr/0010-symbol-lookup-and-positions.md) D4; see [Case](guide/querying.md#case)).

**Encodings.** Files in another encoding are decoded to UTF-8 before indexing ([ADR 0007](adr/0007-source-encodings.md)), and their spans point into the decoded text. An item about such a file (from `search`, `find_symbols`, `file_outline` or `list_files`) carries `encoding` (the WHATWG name, e.g. `UTF-16LE`, `windows-1252`, `Shift_JIS`) and, when invalid bytes were replaced with U+FFFD, `lossy: true`; both are absent for UTF-8. `describe` gives per repo `encodings` (files per non-UTF-8 encoding; the rest are UTF-8) and `lossy` (a count). See [Source encodings](guide/indexing.md#source-encodings).

**Paging.** List tools take `limit` (default 50, at most 500; `file_tokens` at most 20000) and `offset` (default 0, at most 4294967295) and answer `{items, next_offset, stale_possible}`. `next_offset` is the `offset` of the next page, or `null` at the end. Each page is read from the database as it is at that moment (there is no snapshot across pages in v1). A page whose items would exceed 4 MiB is cut short, with `next_offset` pointing at the first item left out.

**`stale_possible`** is `true` when a read of that call went to a server node that may have missed acknowledged writes (ADR 0004 D8). It is always `false` with `--db`.

**Arguments.** An optional argument may be left out or given as `null` (the same thing). Integers may also be written as integral floats (`5.0`).

**Errors.** Every refusal of a call to a known tool is a tool result with `isError: true` (a tool execution error, so the model sees it and can correct the call) whose text block is `{"code", "message", "retryable"}`:
- `invalid_params`: the arguments do not fit the `inputSchema` (a wrong type, an unknown argument, `limit` out of range, an unknown `grain` or `token_class`, `start_line` after `end_line`, `symbol_kind` with a grain that cannot hold it). The message lists the valid values.
- `invalid_argument`: the arguments name something not indexed (an org, repo, language, symbol kind or file). The message lists what is present.
- A store error has its kind as the code (`no_leader`, `locked`, `corrupt`, `rejected`, ...). `unavailable` is a lost connection to the server (a transport error, a deadline, the server unreachable). `no_leader`, `not_leader` and `unavailable` are retryable; `no_leader` carries `retry_after_ms`.

An unknown tool name and a malformed request (bad JSON, a batch, a bad id, a non-object `arguments`) are JSON-RPC errors instead.
