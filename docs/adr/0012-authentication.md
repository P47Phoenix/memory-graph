# ADR 0012: Authentication

**Status:** Accepted by the owner on 2026-10-10 (proposed on 2026-10-09), with its recommendations; the open questions are resolved in [Owner decisions (2026-10-10)](#owner-decisions-2026-10-10). Revised on 2026-10-09 after the dev and QA reviews of PR #281. Delivers issue [#105](https://github.com/P47Phoenix/memory-graph/issues/105) (ADR 0004 Q1) through [story 69](../epic-code-memory-graph.md#story-69), which unblocks story 33 (MCP write tools, [ADR 0005](0005-mcp.md) D2). Builds on [ADR 0004](0004-client-server-and-replication.md) (gRPC, `Hello`, forwarding, exit codes) and ADR 0005 (the MCP HTTP endpoint). TLS stays a follow-up: [#104](https://github.com/P47Phoenix/memory-graph/issues/104).

## In plain words

1. Today anyone who can reach a `serve` port can read every indexed token, write, prune, shut the node down or change cluster membership. ADR 0004 says "trusted networks only" until this ADR.
2. **Clients send a bearer token** in the `authorization` gRPC header. A tower layer in front of every service checks it in constant time (`subtle`) against SHA-256 hashes kept in a token file, then checks the caller's role against a per-method table that **denies any method it does not list**. The plaintext token is never stored on the server and never logged.
3. **Two roles in v1:** `read` and `write` (write includes admin). No per-org or per-repo permissions in v1; that is a planned follow-up.
4. **Nodes use a separate cluster secret** for Raft, join, forwarding and other node-to-node calls. It is the most powerful credential, and a client token can never stand in for it.
5. **No TLS in v1.** A token sent in clear text can be sniffed and replayed. Operators who leave a trusted network must put a TLS-terminating proxy (or a mesh) in front. This is documented as a risk, not hidden.
6. Auth is **off by default** (unchanged behaviour). `serve --auth-tokens <file>` turns it on. It adds proto fields but does not bump `PROTOCOL_VERSION`.

## Context

References are to `origin/main` on 2026-10-09 (`602a255`).

- `graph-server` already checks `mg-protocol-version` on each call and refuses a mismatch with `FAILED_PRECONDITION` and a typed `Protocol` detail (ADR 0004 D1). It also has a tower layer that sees each call's method path, `observe::RpcLayer` (installed in `server.rs` around line 916). A tonic `Interceptor` cannot see the method path, so the auth check is a tower layer like that one.
- Internal gRPC clients that a node opens to other nodes: `raft/network.rs` (`GrpcNetwork`, about line 588: `AppendEntries`, `Vote`, `InstallSnapshot`), `join.rs` (about line 66: `Admin.Join`, the cluster-id probe), `services/admin.rs` (about line 141: the `Status` probe the leader sends to a joiner), `advertise.rs` (`Admin.UpdateAdvertise`) and `forward.rs` (forwarded writes, membership changes and `Admin.ReadIndex`, marked with `mg-forwarded-by`). A forward is a new call, so caller metadata does not travel unless it is copied.
- `RemoteStore` (`graph-client`) owns its channel. The CLI resolves `--server` / `MEMORY_GRAPH_SERVER` in `target.rs`, where the exit codes are: 1 `NOT_SERVING` (and any other failure), 3 `NO_LEADER`, 4 `WRITE_DEADLINE`, 5 `PROTOCOL`, 6 `WRONG_CLUSTER`, 7 `TELEMETRY_CONFIG`.
- The MCP HTTP endpoint (`--mcp-listen`) is off by default, read-only, with Origin/Host checks and a boxed no-auth warning (ADR 0005 D4). It binds loopback unless `--mcp-allow-remote`, which logs a warning at every start. MCP over stdio has no socket.
- `--metrics-listen` serves Prometheus text on a separate HTTP listener.
- `grpc.health.v1` is served by `tonic-health` for load balancers and orchestrators.
- The pure-Rust gate deny-lists `ring`, `aws-lc-sys`, `openssl-sys`, `libz-sys`.

### Candidate crates, checked against the gate (2026-10-09)

I built a throwaway project and passed its `--manifest-path` to `scripts/check-no-c-deps.py`, which checks all shipped targets:

| Crates | Result |
|---|---|
| `subtle 2.6`, `sha2 0.10`, `rand_core 0.6` (`getrandom`), `base64 0.22`, `zeroize 1`, `tonic 0.14` (`transport`, `codegen`) | **Passed** (67 deps); `cargo tree -i ring` / `-i aws-lc-sys` empty |
| the above plus `rustls 0.23` (`default-features = false`, `std`) and `rustls-rustcrypto 0.0.2-alpha` | **Failed** on `ring 0.17.14` (deny-listed, `cc` build script) |
| `rustls` and `rustls-rustcrypto` alone (same versions and features) | `cargo tree -i ring --target all` printed nothing |

**Unverified:** which edge brought `ring` into the combined project. It does not come from `rustls-rustcrypto` alone, and the exact path was not captured. D4 therefore does not rest on `ring`. It rests on `rustls-rustcrypto` being an unaudited alpha. Re-check under #104.

## Threat model

| Asset | Threat | In scope for v1 |
|---|---|---|
| Indexed source (tokens, symbols, paths) | Read by an unauthorised network caller | **Yes**: every data-bearing RPC needs `read` |
| Graph integrity | Index/prune/vacuum/ingest by an unauthorised caller | **Yes**: `write` |
| Node and cluster control | `Admin` membership, shutdown, election, snapshot/upload, compaction | **Yes**: `write` or the cluster secret (D5) |
| Replicated log | A rogue process speaking `Raft` (vote, append entries, install a snapshot) or `Join` / `UpdateAdvertise` | **Yes**: cluster secret |
| **The cluster secret** | It is accepted on the same client-facing port. Whoever holds it can append Raft entries, install a snapshot, join, and act on forwarded roles. It is the highest-privilege credential. | **Yes**: separate file, separate prefix (`mgc_`), refused on client paths, never sent by `RemoteStore` |
| Tokens in transit | Sniffed and replayed on the wire (no TLS) | **No** natively. **Replay is explicitly out of scope** in v1 (no nonce or binding). Mitigated by a TLS proxy (D4). |
| Tokens at rest on the server | Token file read from disk | **Yes**: only SHA-256 hashes stored |
| Tokens at rest on the client | Env var or file read by another local user; env visible in `/proc/<pid>/environ`, crash reports, CI logs | Partly: a file-mode warning; the env risk is documented; env values are never echoed |
| Token in logs, errors, traces, metrics, panics | Leak through diagnostics | **Yes**: tested (D8) |
| Timing side channel on compare | Token guessed byte by byte | **Yes**: constant-time compare over fixed-length hashes |
| Brute force and refusal floods | Online guessing; log or CPU flooding | Guessing defeated by 256-bit tokens; refusals rate-limited in logs and counted per peer; no lockout in v1 |
| A compromised `write` holder | Destroys data | **No**: backups (ADR 0006) are the recovery |
| Local filesystem access to `graph.redb` | Bypasses the server | **No**: the OS is the boundary |
| Per-tenant isolation (org A reads org B) | | **No** in v1 (D6) |

## Decisions

| # | Question | Decision |
|---|---|---|
| D1 | How do clients authenticate? | `authorization: Bearer <token>`, checked by an `AuthLayer` tower layer that sees `:path`. The presented token is hashed with SHA-256 and compared in constant time (`subtle`) against every stored hash. A deny-by-default per-method role table follows. |
| D2 | Provisioning, rotation, credential surface | `memory-graph auth new-token` creates tokens. The server reads a token file (`--auth-tokens`) into an `ArcSwap` and reloads it on SIGHUP or a content-hash poll. Clients use `--token-file`, `MEMORY_GRAPH_TOKEN_FILE` or `MEMORY_GRAPH_TOKEN`. |
| D3 | Node-to-node vs clients | A cluster secret (`--cluster-secret-file`) is used by every internal client. It is accepted only on node-to-node methods and on forwarded calls. It grants nothing on `Store`/`Write` directly. |
| D4 | TLS | No native TLS in v1. Recommend a TLS-terminating proxy. With auth on, refuse a non-loopback `--listen`, `--advertise` or `--mcp-listen` unless `--insecure-transport` is passed. |
| D5 | Roles | `read`, `write` (write implies read and admin), and `node` (cluster secret only). Any method not in the table is refused. |
| D6 | Per-org/repo permissions | Out of v1. Planned follow-up; the token file reserves a `scope` field. |
| D7 | Proto, `Hello`, `PROTOCOL_VERSION` | No version bump. Additive changes only: `Hello` reports `auth_required` and is trimmed for unauthenticated callers; new typed `Unauthenticated` / `PermissionDenied` details. |
| D8 | Errors, exit codes, "token never logged" | `UNAUTHENTICATED` -> exit **8**, `PERMISSION_DENIED` -> exit **9**. Auth is checked before the protocol-version check. A trace-level log scan proves no secret leaks. |
| D9 | Health, metrics, MCP | Health stays open. The metrics listener stays open, aggregate counters only. MCP over HTTP requires a token when auth is on, with the same role table. Stdio uses the CLI's credentials. |
| D10 | Tests | See the test plan. |

### D1. Client authentication

- The client sends `authorization: Bearer <token>` on every call. This is the standard header, so proxies and generic gRPC tools understand it.
- **Token format:** `mgt_` followed by 43 characters of unpadded base64url encoding 32 bytes from the OS RNG (`getrandom` via `rand_core::OsRng`). The prefix makes a leaked token findable by secret scanners. With 256 bits of entropy, a fast unsalted hash is safe; password hashing such as argon2 is unnecessary because these are not human-chosen secrets.
- **Pre-hash checks:** the layer refuses a header without the `Bearer ` scheme, a value over 128 bytes, non-ASCII bytes, and anything not exactly `mgt_` plus 43 base64url characters, before hashing. A `mgc_` value is a cluster secret, which is never valid on a client path, so it is also refused there.
- **At rest:** the server stores only `sha256(token)`. The layer hashes the presented token, then compares it with **every** stored hash using `subtle::ConstantTimeEq`. It ORs the results with no early exit, so timing depends only on the number of tokens, not on which one matched or how many bytes matched. Comparing fixed-length 32-byte digests also removes the length side channel.
- A missing header, a malformed header and a wrong or expired token all return the same `UNAUTHENTICATED` status with the same message ("missing or invalid credentials"). The server never says which.
- **Where:** an `AuthLayer` tower layer wraps the whole router, like `observe::RpcLayer`. It reads the request's `:path` (`/memory_graph.v1.Write/Index`), checks the credential, looks the method up in the role table (D5) and refuses the call before any service code runs. **Streaming RPCs** (`Write.Index`, `Raft.InstallSnapshot`) are checked on the request headers, before the first body frame is polled, so an unauthenticated stream ingests nothing. The caller identity (token `id` and role, never the token) goes into the request extensions for logs and spans.
- **Order:** auth runs **before** the `mg-protocol-version` check. A bad token and a bad version together produce `UNAUTHENTICATED`, so an unauthenticated caller learns nothing about versions.
- **Client side:** the token is held as `zeroize::Zeroizing<String>`. The outgoing `MetadataValue` is marked sensitive with `set_sensitive(true)`, so `is_sensitive()` holds and `Debug` prints `Sensitive`.

### D2. Provisioning, rotation and the credential surface

**Server token file** (`--auth-tokens <path>`, TOML, one table per token):

```toml
[[token]]
id = "ci-indexer"          # free text, logged on use; never the secret
role = "write"             # read | write
sha256 = "9f86d0...c3a"    # 64 hex chars: sha256 of the token
# expires = "2027-01-01T00:00:00Z"   # optional; refused from this instant on
# scope = [...]            # reserved for D6; v1 refuses the file if present
```

- **Validation (the whole file is refused, at start or reload, on):** a `scope` key, an unknown `role`, a duplicate `id`, a duplicate `sha256`, a hash that is not 64 lowercase-or-uppercase hex characters, an unparsable `expires`, or unknown keys. An empty file, or one with no tokens, is refused at start, because it would lock everyone out. `expires` is checked on every call: a token is valid while `now < expires` and refused from `expires` on.
- `memory-graph auth new-token --id <id> --role <read|write>` prints the token once to stdout and the TOML stanza to stderr. With `--append <file>` it appends the stanza instead, creating the file with mode 0600 on Unix. The token never appears in the stanza, in `--append` errors or in any other output. `--token-out <file>` writes the token to a new 0600 file instead of stdout.
- **State:** the parsed set lives in an `ArcSwap<TokenSet>`. Each request loads one snapshot at its start and uses it to the end, so a swap never splits a request.
- **Reload:** on SIGHUP (Unix) and on a poll every 5 s on all platforms. The poll re-resolves the path, following symlinks, and compares a SHA-256 of the file's content, not its mtime, which covers Kubernetes secret mounts that swap a `..data` symlink. A SIGHUP and a poll that land together serialize on one reload mutex, and a reload of identical content is a no-op. A file that fails to parse or validate, is half-written, or has been deleted **keeps the old set** and logs an error (rate-limited). A reload never opens the server. Tests drive reloads through a `reload_now()` hook, not sleeps.
- Each node of a cluster has its own `--auth-tokens` file. The file is not replicated through Raft in v1, because that would turn the log into a secret store. Operators distribute it like any other secret. Owner decision Q2 (2026-10-10).
- **File-permission warnings:** on Unix, the server warns if the token file or the cluster-secret file is readable by group or others, and the client warns the same about `--token-file`. On Windows no ACL check is made in v1; `docs/security.md` says to restrict the file to the service account.
- **Client surface**, highest precedence first: `--token-file <path>`, then `MEMORY_GRAPH_TOKEN_FILE`, then `MEMORY_GRAPH_TOKEN`. There is no `--token <value>` flag, because command-line arguments leak through `ps` and shell history. `docs/security.md` documents the env-var risk (process environment, CI logs, crash dumps). No error or log line ever echoes an env value. The file is read once, with one trailing newline trimmed.
- `ClientConfig` gains `credentials: Option<Credentials>`, whose `Debug` is redacted. `RemoteStore` adds the header on every call through a client interceptor.

### D3. Node-to-node authentication

- **Cluster secret:** `--cluster-secret-file` holds the same format with an `mgc_` prefix. Every node holds the same secret. It is sent as `authorization: Bearer mgc_...` and checked in constant time against its hash. It is required whenever `--auth-tokens` is set on a clustered node (`--data-dir`).
- **Every internal client sends it:** `GrpcNetwork` (`AppendEntries`, `Vote`, `InstallSnapshot`), `join.rs` (`Admin.Join` and the cluster-id probe), the leader's `Status` probe in `services/admin.rs`, `advertise.rs` (`UpdateAdvertise`), and `forward.rs` (every forwarded call). A test lists these call sites, so a new internal client without the secret fails.
- **The join probes:** the cluster-id probe in `join.rs` calls `Admin.Status` (about lines 79 and 353), and `--bootstrap-or-join` also calls `Admin.Members` on a peer it found (about line 418). Both RPCs are `read` in D5. The leader probes a joiner with `Admin.Status` as well.
- **Where the secret is accepted:** on methods whose table entry is `node` (D5); on `Admin.Status` and `Admin.Members`, for the join probes above; and on `read`/`write` methods **only** when the call also carries `mg-forwarded-by` (a forward). **It does not grant `Store` or `Write` on a direct call.** A caller who holds the secret but no `mg-forwarded-by` gets `PERMISSION_DENIED`. A forged `mg-forwarded-by` still needs the secret, so this is defence in depth, not a separate privilege.
- **Client tokens never authorise `node` methods.** A `write` token on `AppendEntries`, `Vote`, `InstallSnapshot`, `Join` or `UpdateAdvertise` gets `UNAUTHENTICATED`.
- **Forwarded calls:** the follower authenticates and authorises the caller first, against its own token file and the role table, and refuses there if the caller lacks the role. It then forwards with the **cluster secret**, plus `mg-forwarded-role: read|write` and `mg-forwarded-token-id: <id>` for the leader's logs. The leader honours `mg-forwarded-role` only on a call that authenticated with the cluster secret. On a client-token call those headers are ignored, so a `read` token with a spoofed `mg-forwarded-role: write` gets `PERMISSION_DENIED`. Forwarding never copies the client's bearer token across nodes, the role check happens once at the edge, and forwarding keeps working while the nodes' token files differ during rotation. The same rule covers forwarded `Admin.ReadIndex` (role `read`) and forwarded membership changes (role `write`).
- **Rotating the secret:** `--cluster-secret-file` may hold two lines, current then next. A node accepts either and sends the first. Add the second line on every node, swap the two lines everywhere, then drop the old one. The file is reloaded the same way as the token file.

### D4. TLS stance

- **v1 has no native TLS.** `rustls` needs a crypto provider, and its defaults (`aws-lc-rs`, `ring`) are deny-listed. The only pure-Rust candidate, `rustls-rustcrypto`, is a `0.0.2-alpha` with no audit. The `ring` failure in the combined gate check is unverified (see Context). Revisit under #104 once a pure-Rust provider passes the gate and has a stable release.
- **Risk, stated plainly:** without TLS, the bearer token, the cluster secret and all data cross the network in clear text. Anyone on the path can read a token and replay it. Auth without TLS protects against callers who can reach the port but cannot see the traffic (a shared host, a flat office network, a misconfigured security group), not against an on-path attacker.
- **Recommended deployment:** bind `serve` to loopback or a private interface and terminate TLS in front of it (Envoy, nginx `grpc_pass`, Caddy, or a mesh sidecar such as Linkerd or Istio with mTLS between pods). Peer traffic should go over a private network or the mesh. A new `docs/security.md` gives an Envoy and a Caddy example and repeats the boxed warning.
- **Guard rail:** with auth on, `serve` refuses to start if `--listen`, `--advertise` or `--mcp-listen` is not loopback, unless `--insecure-transport` is passed. Loopback means `127.0.0.0/8`, `::1` and `localhost` as resolved; `0.0.0.0` and `[::]` are not loopback. The flag's name says what it is, and a proxy or mesh deployment passes it knowingly. **A multi-host cluster always needs the flag**, because its `--advertise` addresses are not loopback. That is intended: running a cluster without TLS is exactly the risk being acknowledged. `serve --help` and the start-up log carry the warning.
- The client refuses `--server https://...` in v1, with a message pointing to the proxy setup: the client cannot speak TLS either, so a client-side proxy or a mesh handles it. Owner decision Q1 (2026-10-10).

### D5. Roles: deny by default

The `AuthLayer` holds a static table from full method path to required credential. **A method that is not in the table is refused with `PERMISSION_DENIED` for every credential**, so a new RPC fails closed until someone classifies it. A unit test enumerates every method in the generated service descriptors and fails if one is missing from the table.

| Credential | Methods |
|---|---|
| none | `Store.Hello` (trimmed, D7); `grpc.health.v1.Health/*` |
| `read` | `Store.*` (every other read, including `OpenSnapshot`, `CloseSnapshot`, `SnapshotStats`); `Admin.Status`, `Members`, `Leader`, `SysInfo`, `Metrics`, `ReadIndex`, `ListBackups` |
| `write` | `Write.*` (`Index`, `IndexFile`, `IngestExtraction`, `Prune`, `Vacuum`); `Admin.AddLearner`, `Promote`, `Remove`, `TransferLeader`, `TriggerElect`, `TriggerSnapshot`, `UploadSnapshot`, `Compact`, `Shutdown` |
| `node` (cluster secret only) | `Raft.AppendEntries`, `Vote`, `InstallSnapshot`; `Admin.Join`, `UpdateAdvertise` |

- `write` implies `read`. The cluster secret also satisfies `Admin.Status` and `Admin.Members` (`read`) because the join probes use them (D3), and satisfies `read`/`write` methods only on forwarded calls (D3).
- A `read` token on a `write` method gets `PERMISSION_DENIED`.
- **Snapshot handles** (`OpenSnapshot` ids) stay bound to the connection that opened them, as today. With auth on, a handle is also bound to the token `id` that opened it. Another token, or an unauthenticated caller, gets the same `SnapshotExpired`/not-found answer as a missing handle, so it cannot tell whether the handle exists.

### D6. Per-org/repo write permissions

Out of v1. A scope check would have to look inside each request (an `Index` stream's files, a `Prune` filter, an `IngestExtraction`), and reads would need result filtering by org/repo, which touches every query path and the conformance suite. The token file reserves `scope`, and v1 refuses any file that sets it, so a later version can add scopes without silently widening access. A follow-up issue will be filed when this ADR is accepted.

### D7. Proto, `Hello` and `PROTOCOL_VERSION`

- **No `PROTOCOL_VERSION` bump.** All changes are additive, which ADR 0004 D1's rule allows:
  - `HelloResponse.auth_required` (bool; the default, false, keeps the old meaning);
  - two `StoreErrorDetail` variants, `Unauthenticated { msg }` and `PermissionDenied { msg, required_role }`, with `StoreError` variants of the same names in `graph-store` and conversions in `graph-proto/src/error.rs`.
- **Unauthenticated `Hello`:** today `Hello` returns the binary, protocol and store-format versions, the extractor hash, the node id, the cluster id and the leader address. When auth is on and the caller has no valid credential, it returns exactly `protocol_version` and `auth_required = true`; every other field is left at its default. An authenticated caller gets the full response. A test pins this list of fields.
- An old client against an auth-on server gets `UNAUTHENTICATED` without the detail it does not know, and the status code alone is enough for a clear failure. A new client against an old server sends a header the old server ignores.
- The authorization header is metadata, not a proto field. The xtask regenerates the code for the additions above.

### D8. Errors, exit codes and "token never logged"

- **New CLI exit codes** in `target.rs`: **8** for `UNAUTHENTICATED` (missing or wrong token) and **9** for `PERMISSION_DENIED` (role too low). Code 7 is `TELEMETRY_CONFIG`; 1, 3-6 are unchanged.
- Messages never echo a credential. Examples: "server requires a token: set --token-file or MEMORY_GRAPH_TOKEN_FILE" (when `Hello.auth_required` is set and no credential was given), and "the token was refused".
- `RemoteStore` never retries `UNAUTHENTICATED` or `PERMISSION_DENIED`; they are final, unlike `UNAVAILABLE`. A detail-less `UNAUTHENTICATED` or `PERMISSION_DENIED` from a proxy maps to the same exit code.
- **Server logging:**
  - Each refusal is logged at `warn` with the peer address, the method and the reason class (missing, malformed, unknown, expired, role, node-only). These lines are rate-limited, at most one per peer per 10 s, with a suppressed count.
  - A counter `mg_auth_refusals_total{reason}` counts refusals, and accepted calls log the token `id` at `debug`.
  - The header, the token and its hash are never logged.
  - Refusals happen before any store work, so a flood of bad tokens costs one SHA-256 per call and does not starve valid callers.
- OpenTelemetry spans (ADR 0009) record `auth.token_id` and `auth.role` only. The W3C propagation extractor reads only `traceparent` and `tracestate`. No layer records request headers.
- **The leak test.** The authenticated conformance run, the e2e suite and the cluster testbed run with `RUST_LOG=trace` captured to a buffer. Each run is searched for the token, its base64 body, its hex hash and the raw `Bearer ` header line, and the same is done for the cluster secret. The test fails if any of them appears in:
  - the captured logs;
  - any `Status` message;
  - the `Debug` output of `ClientConfig`, `Credentials` and `RemoteStore`;
  - the `Display` of `StoreError` and of the CLI's `anyhow` chain;
  - CLI stderr at exit codes 8 and 9;
  - a forced panic message and backtrace (an injected panic in a handler);
  - exported OTel span attributes;
  - the output and errors of `auth new-token --append`.

### D9. Health, metrics and MCP

- **Health:** `grpc.health.v1` stays unauthenticated, because load balancers and kubelets need it, and it reveals only the serving status.
- **Metrics:** the `--metrics-listen` HTTP listener stays unauthenticated in v1. It exposes aggregate counters and histograms only: RPC counts and latencies per method, store and Raft gauges, and `mg_auth_refusals_total{reason}`. It never exposes token ids, token values or indexed content, and it must not gain a per-token label. The listener should be bound to loopback or scraped through the proxy. With auth on it falls under the same `--insecure-transport` guard.
- **MCP over HTTP** (`--mcp-listen`):
  - With auth on, every request needs `Authorization: Bearer <token>`, checked against the same token file. The MCP host runs in-process, so it applies the same role table: read tools need `read`, and story 33's write tools need `write`.
  - A missing or wrong token gets HTTP 401 with `WWW-Authenticate: Bearer`; a `read` token on a write tool gets HTTP 403.
  - With auth on, `--mcp-allow-remote` also requires `--insecure-transport`.
  - The existing Origin/Host checks stay and run in addition to auth.
  - With auth off, ADR 0005 D4 is unchanged and the boxed warning remains, now also naming `--auth-tokens`.
- **MCP over stdio** has no socket. It uses the target's credentials (D2).

### D10. Test plan

**Conformance and RPC coverage**
- With auth on and a valid `write` token, `run_all`, `run_differential(embedded, remote)` and `run_crash_rerun_differential` pass unchanged over `TestServer`.
- With auth on and no token, a wrong token, an expired token, an `mgc_` value, an oversized header or a non-`mgt_` value, every method fails and returns no data. Every method except `Hello` and health gets `UNAUTHENTICATED`.
  - The test is table-driven from the service descriptors, so a new RPC cannot be missed.
  - `Hello` returns exactly the pinned trimmed fields (D7).
  - A bad token together with a bad `mg-protocol-version` returns `UNAUTHENTICATED`.
- With a `read` token, reads pass and every `write` method gets `PERMISSION_DENIED`.
- An unlisted method (a test-only service mounted through the layer) is refused for every credential.
- With auth off, conformance without credentials passes unchanged, and a client that sends a token is accepted (the token is ignored).
- With auth on, the health check answers `SERVING` with no token.

**Streaming and snapshots**
- A `Write.Index` stream opened without a valid `write` token is refused at open, and the store is byte-for-byte unchanged (`count_nodes` and `describe` are equal before and after).
- `Raft.InstallSnapshot` without the cluster secret is refused before any chunk is written to `snapshots/`.
- A snapshot handle opened by token A is unusable by token B, by an unauthenticated caller, and from another connection.

**Client tokens and the cluster secret**
- A client `write` token on `AppendEntries`, `Vote`, `InstallSnapshot`, `Join` and `UpdateAdvertise` is refused.
- The cluster secret on a direct `Store` or `Write` call, with no `mg-forwarded-by`, gets `PERMISSION_DENIED`.
- A node with the wrong secret cannot vote, append, install a snapshot, join or update its advertise address.
- Two-line secret rotation across a three-node cluster keeps the cluster available at every step.
- The internal-client inventory test (D3) passes.

**Forwarding (cluster testbed)**
- A write sent to a follower with a valid `write` token is applied through the leader.
- With a `read` token, the follower refuses the write before forwarding.
- A forwarded `Admin.ReadIndex` needs `read`.
- Forwarded `AddLearner`, `Promote`, `Remove` and `TransferLeader` need `write`.
- A direct call that sets `mg-forwarded-by` and `mg-forwarded-role: write`:
  - with a `read` token, gets `PERMISSION_DENIED`;
  - with no credential, gets `UNAUTHENTICATED`.
- **Rotation skew:** a token present only on the follower's file is honoured for a forwarded write, and a token present only on the leader's file is refused by the follower.

**Reload**
- Both an old and a new hash are accepted at once.
- After a hash is removed and the set is reloaded, the token is refused on the next call.
- An expired token is refused on the first call at or after `expires`.
- A SIGHUP and a poll fired together give one consistent set.
- An in-flight streaming request keeps the snapshot it started with across a swap.
- Each of these keeps the old set:
  - a half-written file;
  - a deleted file;
  - a file that fails validation.
- A symlink swap (the Kubernetes `..data` pattern) is picked up by content hash.
- The Unix SIGHUP path and the poll path (the only one on Windows) are tested separately.
- All of these use the deterministic `reload_now()` hook.

**Token file validation**
- Each of these is refused with a message that names the line and never the secret:
  - `scope` present;
  - an unknown role;
  - a duplicate id;
  - a duplicate hash;
  - bad hex;
  - a hash of the wrong length;
  - an unknown key;
  - an empty file or zero tokens at start;
  - an unparsable `expires`.
- At `expires` minus 1 s the token is accepted; at `expires` and later it is refused.

**Exit codes and errors (e2e, real binary)**
- Exit 8 with no token or a wrong token.
- Exit 9 with a `read` token on `index`.
- A stub proxy returning detail-less `UNAUTHENTICATED` / `PERMISSION_DENIED` maps to 8 and 9.
- A counting server sees exactly one call: these errors are never retried.
- `--server https://...` is refused.

**`--insecure-transport` matrix**
- With auth on, `127.0.0.1`, `::1` and `localhost` start.
- `0.0.0.0`, `[::]` and a LAN address are refused, and pass with the flag.
- The same rules apply to `--advertise`, `--mcp-listen` and `--metrics-listen`.

**MCP HTTP**
- No token -> 401, a wrong token -> 401, and a `read` token on a write tool -> 403. A `read` token on `search` succeeds.
- Origin and Host violations are still refused with a valid token.
- Stdio inherits `--token-file` and the env.

**Leak and abuse**
- The D8 leak test.
- `is_sensitive()` is set on the outgoing header.
- 10,000 bad-token calls:
  - produce at most a bounded number of log lines (rate limit) and the matching `mg_auth_refusals_total` count;
  - leave a concurrent valid caller's p99 latency within 2x of the unloaded baseline.
- Env values are never echoed in any error.
- `new-token --append` and `--token-out` create 0600 files on Unix, and the client warns on a group- or world-readable `--token-file`.

**Gate and fuzz**
- `check-no-c-deps.py` passes, and `cargo tree -i ring` / `-i aws-lc-sys` print nothing.
- The header parser and the token-file parser never panic on arbitrary bytes.

## Consequences

- An operator can expose `serve` beyond loopback with a meaningful access check, and story 33 (MCP write tools) is unblocked.
- Auth without TLS is weaker than it looks. The guard rail and the docs make the proxy explicit, and #104 remains the real fix. Every multi-host cluster passes `--insecure-transport` until then.
- There are two kinds of secret to distribute per node (client tokens and the cluster secret), with no central store in v1. The cluster secret is the most sensitive file on a node.
- There is no per-tenant isolation: one `read` token reads every org.
- The new dependencies (`subtle`, `zeroize`, `arc-swap`, plus crates already in the graph) are gate-clean.

## Owner decisions (2026-10-10)

The owner accepted this ADR with its recommendations, and made Q1 explicit.

- **Q1.** Should the `--insecure-transport` start-up refusal (D4) be kept, given that every multi-host cluster must pass it, or only warn? **Kept, as a refusal, not a warning:** with auth on, `serve` on a non-loopback address refuses to start unless `--insecure-transport` is given explicitly.
- **Q2.** Should token files be replicated through Raft later (one place to rotate), or stay per-node files? **Per-node files** in v1, as D2 recommends; the Raft log does not become a secret store.
- **Q3.** Separate exit codes 8 and 9, or one code for both? **Separate:** 8 for `UNAUTHENTICATED`, 9 for `PERMISSION_DENIED` (D8).
- **Q4.** Per-org/repo scopes (D6): file the follow-up now? Should it cover reads (result filtering) or only writes? **As D6 recommends:** v1 has the `read` and `write` roles only, and the follow-up issue for per-org/repo scopes is filed now that the ADR is accepted. Whether scopes cover reads (result filtering) as well as writes is decided in that follow-up.
- **Q5.** The trimmed unauthenticated `Hello` (D7) hides the node id, cluster id and leader address. Is that right, or is the full response wanted for tooling? **Trimmed, as D7 proposes;** the full response needs a token.
- **Q6.** Should the metrics listener (D9) stay unauthenticated in v1? **Yes,** aggregate counters only, under the same `--insecure-transport` guard (D9).
