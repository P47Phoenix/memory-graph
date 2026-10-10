# ADR 0012: Authentication

**Status:** Proposed on 2026-10-09; the owner accepts it. No code before acceptance. Delivers issue [#105](https://github.com/P47Phoenix/memory-graph/issues/105) (ADR 0004 Q1) through [story 69](../epic-code-memory-graph.md#story-69), which unblocks story 33 (MCP write tools, [ADR 0005](0005-mcp.md) D2). Builds on [ADR 0004](0004-client-server-and-replication.md) (gRPC, `Hello`, forwarding, exit codes) and ADR 0005 (the MCP HTTP endpoint). TLS stays a follow-up: [#104](https://github.com/P47Phoenix/memory-graph/issues/104).

## In plain words

1. Today anyone who can reach a `serve` port can read every indexed token and write, prune or change cluster membership. ADR 0004 says "trusted networks only" until this ADR.
2. **Clients send a bearer token** in the `authorization` gRPC header. The server checks it in a tonic interceptor, in constant time (`subtle`), against SHA-256 hashes kept in a token file. The plaintext token is never stored on the server and never logged.
3. **Two roles in v1:** `read` and `write` (write includes admin). No per-org or per-repo permissions in v1; that is a planned follow-up.
4. **Raft peers use a separate cluster secret**, not a client token, so a leaked client token cannot impersonate a node.
5. **No TLS in v1.** A token sent in clear text can be sniffed. Operators who leave a trusted network must put a TLS-terminating proxy (or a mesh) in front. This is documented as a risk, not hidden.
6. Auth is **off by default** (unchanged behaviour). `serve --auth-tokens <file>` turns it on. Turning it on does not bump `PROTOCOL_VERSION`.

## Context

References are to `origin/main` on 2026-10-09 (`602a255`).

- `graph-server` already runs one interceptor: it checks `mg-protocol-version` and refuses a mismatch with `FAILED_PRECONDITION` and a typed `Protocol` detail (ADR 0004 D1). Auth is a second check on the same path.
- A follower forwards writes, membership changes and `Admin.ReadIndex` to the leader (`forward.rs`), marking them with `mg-forwarded-by` to stop loops. The forward is a new gRPC call from the follower, so caller metadata does not travel unless copied.
- `RemoteStore` (`graph-client`) owns its channel; the CLI resolves `--server` / `MEMORY_GRAPH_SERVER` in `target.rs`, where exit codes 3-6 live.
- The MCP HTTP endpoint (`--mcp-listen`) is off by default, loopback only, read-only, with a boxed no-auth warning (ADR 0005 D4). MCP over stdio has no socket.
- `grpc.health.v1` is served by `tonic-health` for load balancers and orchestrators.
- The pure-Rust gate deny-lists `ring`, `aws-lc-sys`, `openssl-sys`, `libz-sys`.

### Candidate crates, checked against the gate (2026-10-09)

A throwaway project with `--manifest-path` to `scripts/check-no-c-deps.py`, across all shipped targets:

| Crates | Result |
|---|---|
| `subtle 2.6`, `sha2 0.10`, `rand_core 0.6` (`getrandom`), `base64 0.22`, `zeroize 1`, `tonic 0.14` (`transport`, `codegen`) | **Passed** (67 deps); `cargo tree -i ring` / `-i aws-lc-sys` empty |
| the above plus `rustls 0.23` (`default-features = false`, `std`) and `rustls-rustcrypto 0.0.2-alpha` | **Failed**: `ring 0.17.14` (deny-listed, `cc` build script), pulled in by the rustls-rustcrypto dependency tree |

`sha2`, `subtle`, `rand_core`/`getrandom` and `base64` are already in the workspace graph or are small RustCrypto crates with no build script.

## Threat model

| Asset | Threat | In scope for v1 |
|---|---|---|
| Indexed source (tokens, symbols, paths) | Read by an unauthorised network caller | **Yes**: every Store RPC needs a `read` token |
| Graph integrity | Index/prune/vacuum/ingest by an unauthorised caller | **Yes**: `write` token |
| Cluster membership and availability | `Admin` join/remove/transfer-leader/snapshot/compact by a caller | **Yes**: `write` token |
| Replicated log | A rogue process speaking the `Raft` service (vote, append entries, install a snapshot) | **Yes**: cluster secret |
| Tokens in transit | Sniffed on the wire (no TLS) | **No** natively; mitigated by a TLS proxy, documented |
| Tokens at rest on the server | Token file read from disk | **Yes**: only SHA-256 hashes stored |
| Tokens at rest on the client | Env var / file read by another local user | Partly: file mode warning; the OS is the boundary |
| Token in logs, errors, traces, metrics | Leak through diagnostics | **Yes**: tested (D8) |
| Timing side channel on compare | Token guessed byte by byte | **Yes**: constant-time compare over fixed-length hashes |
| Brute force | Online guessing | Yes by entropy: tokens are 256-bit random; no rate limiting in v1 |
| A compromised `write` holder | Destroys data | **No**: out of scope; backups (ADR 0006) are the recovery |
| Local filesystem access to `graph.redb` | Bypasses the server | **No**: the OS is the boundary |
| Per-tenant isolation (org A reads org B) | | **No** in v1 (D6) |

## Decisions

| # | Question | Decision |
|---|---|---|
| D1 | How do clients authenticate? | Bearer token in `authorization: Bearer <token>` metadata, checked by a tonic interceptor; SHA-256 of the presented token compared in constant time (`subtle::ConstantTimeEq`) with every stored hash. |
| D2 | Provisioning, rotation, credential surface | `memory-graph auth new-token` prints a token and its hash line; the server reads a token file (`--auth-tokens`) and reloads it on SIGHUP and on change; clients use `--token-file` / `MEMORY_GRAPH_TOKEN_FILE` / `MEMORY_GRAPH_TOKEN`. |
| D3 | Raft peers vs clients | A separate cluster secret (`--cluster-secret-file`) authenticates the `Raft` service and forwarded calls; client tokens never authorise `Raft`. |
| D4 | TLS | No native TLS in v1. Recommend a TLS-terminating proxy; refuse to start with auth on a non-loopback address unless `--insecure-transport` is passed. |
| D5 | Roles | `read` and `write` (write implies read and admin). |
| D6 | Per-org/repo permissions | Out of v1. Planned follow-up; the token file format reserves a `scope` field. |
| D7 | Proto, `Hello`, `PROTOCOL_VERSION` | No version bump. Additive: `Hello` reports `auth_required`; a new typed `Unauthenticated` / `PermissionDenied` detail. |
| D8 | Errors, exit codes, "token never logged" | `UNAUTHENTICATED` -> exit 7, `PERMISSION_DENIED` -> exit 8; a trace-level log capture test asserts the token never appears. |
| D9 | Health, MCP | Health stays unauthenticated; MCP over HTTP requires a token when auth is on, stdio inherits the CLI target's credentials. |
| D10 | Tests | `RemoteStore` conformance with and without credentials, forwarded-write path, peer auth, timing-independent compare, log scan. |

### D1. Client authentication

- The client sends `authorization: Bearer <token>` on every call (gRPC metadata, the standard header; proxies and generic gRPC tools understand it).
- **Token format:** `mgt_` + 43 chars of unpadded base64url over 32 bytes from the OS RNG (`getrandom` via `rand_core::OsRng`). The prefix makes leaked tokens greppable by secret scanners. 256 bits of entropy makes online guessing and a fast unsalted hash safe (no password hashing such as argon2 is needed: these are not human-chosen secrets).
- **At rest:** the server stores `sha256(token)` only. The interceptor hashes the presented token, then compares it with **every** stored hash using `subtle::ConstantTimeEq` and ORs the results (no early exit), so timing depends only on the number of tokens, not on which one matched or how many bytes matched. Comparing fixed-length 32-byte digests also removes the length side channel.
- A missing header, a malformed header (not `Bearer`, not valid ASCII) and a wrong token all produce the same `UNAUTHENTICATED` status and message ("missing or invalid credentials"); the server never says which.
- The interceptor runs **before** the protocol-version interceptor's handler logic and before any service code, on `Store`, `Write` and `Admin`. The check result (token id and role, never the token) is put in the request extensions for the per-RPC role check (D5).
- The plaintext token is held in the client as `zeroize::Zeroizing<String>` and in a `MetadataValue` marked sensitive (`set_sensitive(true)`, so `http`'s `Debug` prints `Sensitive`).

### D2. Provisioning, rotation and the credential surface

**Server token file** (`--auth-tokens <path>`, TOML, one table per token):

```toml
[[token]]
id = "ci-indexer"          # free text, logged on use; never the secret
role = "write"             # read | write
sha256 = "9f86d0...c3a"    # hex sha256 of the token
# scope = [...]            # reserved for D6; v1 refuses the file if present
# expires = "2027-01-01"   # optional; an expired token is refused like a wrong one
```

- `memory-graph auth new-token --id <id> --role <read|write>` prints the token once to stdout and the TOML stanza to stderr (or `--append <file>`). It needs no server.
- **Rotation without downtime:** add the new hash, reload, move clients, remove the old hash, reload. Reload is on SIGHUP (Unix) and on a file-modification poll (every 5 s, covers Windows and Kubernetes secret mounts). A reload that fails to parse keeps the old set and logs an error; it never opens the server.
- Each node of a cluster has its own `--auth-tokens`; the file is not replicated through Raft in v1 (replicating it would make the log a secret store). Operators distribute it like any secret. Open question Q2.
- The server warns on start if the token file is readable by group/other (Unix).
- **Client surface**, highest precedence first: `--token-file <path>`, `MEMORY_GRAPH_TOKEN_FILE`, `MEMORY_GRAPH_TOKEN`. No `--token <value>` flag: command-line arguments leak through `ps` and shell history. The file is read once, trimmed of one trailing newline.
- `ClientConfig` gains `credentials: Option<Credentials>`; `RemoteStore` adds the header through a client interceptor.
- **MCP stdio** (`memory-graph mcp --server ...`) uses the same client surface. MCP over HTTP: D9.
- `serve` itself, when it forwards or talks to peers, never uses a client token (D3).

### D3. Raft peer authentication vs client authentication

- Peers authenticate with a **cluster secret** (`--cluster-secret-file`, the same 32-byte random format, `mgc_` prefix), sent as `authorization: Bearer` on the `Raft` service and checked the same constant-time way against its hash. Every node holds the same secret.
- With auth on, the `Raft` service accepts **only** the cluster secret; a client token, even `write`, is refused with `UNAUTHENTICATED`. With auth off and no secret, peer traffic is unchanged.
- **`Admin.Join`** from `serve --join` presents the cluster secret; this is the join credential, so a stranger with network access cannot add a learner. Operator-issued membership changes (`cluster add-learner`, `promote`, `remove`, `transfer-leader`) need a `write` client token.
- **Forwarded calls** (follower -> leader, `mg-forwarded-by` set): the follower has already authenticated and authorised the caller. It forwards with the **cluster secret** plus `mg-forwarded-role: read|write` and `mg-forwarded-token-id: <id>` (for the leader's logs). The leader accepts `mg-forwarded-*` headers only when the call carries the cluster secret; on a call with a client token they are ignored. This avoids copying the client's bearer token across nodes, keeps forwarding working when nodes' token files briefly differ during rotation, and makes the role check happen once, at the edge. A forwarded call carrying neither a valid cluster secret nor a valid client token is refused (story 69's forwarded-write criterion).
- Rotation of the cluster secret: `--cluster-secret-file` may hold two lines (current, next); a node accepts either and sends the first. Roll the second in everywhere, then swap.

### D4. TLS stance

- **v1 has no native TLS.** `rustls` needs a crypto provider; its defaults (`aws-lc-rs`, `ring`) are deny-listed, and the gate check above shows `rustls-rustcrypto 0.0.2-alpha` still pulls `ring` in. It is also an alpha with no audit. Revisit under #104 when a pure-Rust provider passes the gate and has a stable release.
- **Risk, stated plainly:** without TLS the bearer token and all data cross the network in clear text. Anyone on the path can read the token and replay it. Auth without TLS protects against callers who can reach the port but cannot see the traffic (a shared host, a flat office network, a misconfigured security group), not against an on-path attacker.
- **Recommended deployment:** bind `serve` to loopback or a private interface and terminate TLS in front of it (Envoy, nginx `grpc_pass`, Caddy, a mesh sidecar such as Linkerd or Istio with mTLS between pods). Peer traffic should also go over a private network or the mesh. `docs/security.md` (new) gives an Envoy and a Caddy example and repeats the boxed warning.
- **Guard rail:** with auth on and `--listen` not on loopback, `serve` refuses to start unless `--insecure-transport` is passed (the flag name says what it is; a proxy deployment passes it knowingly). `serve --help` and the start-up log carry the warning.
- The client never downgrades: `--server https://...` is refused in v1 with a message pointing to the proxy setup (the client cannot speak TLS either; a proxy on the client side, or a mesh, handles it). Open question Q1.

### D5. Roles

| RPC group | Role |
|---|---|
| `Store` (all reads, `Hello` excepted, snapshot handles), `Admin.Status/Members/Leader/SysInfo/Metrics/ReadIndex` | `read` |
| `Write` (all), `Admin.AddLearner/Promote/Remove/TransferLeader/TriggerSnapshot/Compact` | `write` |
| `Admin.Join`, `Raft` | cluster secret (D3) |
| `Store.Hello`, `grpc.health.v1` | none |

`Hello` stays open so a client can learn `auth_required` and the protocol version before it has credentials (and give a clear error); it returns no indexed data. A `read` token on a `write` RPC gets `PERMISSION_DENIED`.

### D6. Per-org/repo write permissions

Out of v1. A scope check has to look inside each request (an `Index` stream's files, a `Prune` filter, an `IngestExtraction`), and reads would need result filtering by org/repo, which touches every query path and conformance. The token file reserves `scope`; v1 refuses a file that sets it, so a later version can add it without silently widening access. A follow-up issue is filed when this ADR is accepted.

### D7. Proto, `Hello` and `PROTOCOL_VERSION`

- **No `PROTOCOL_VERSION` bump.** All changes are additive (ADR 0004 D1's rule): `HelloResponse.auth_required` (bool, default false keeps the old meaning), and two `StoreErrorDetail` variants, `Unauthenticated { msg }` and `PermissionDenied { msg, required_role }`, with `StoreError` variants of the same names in `graph-store` and conversions in `graph-proto/src/error.rs`.
- An old client against an auth-on server gets `UNAUTHENTICATED` without the detail it does not know; the status code alone is enough for a clear failure. A new client against an old server sends a header the old server ignores.
- The authorization header is metadata, not a proto field, so the generated code does not change apart from the additions above (regenerated by the xtask).

### D8. Errors, exit codes and "token never logged"

- New CLI exit codes in `target.rs`: **7** `UNAUTHENTICATED` (missing or wrong token), **8** `PERMISSION_DENIED` (role too low). Message examples: "server requires a token: set --token-file or MEMORY_GRAPH_TOKEN_FILE" (when `Hello.auth_required` and no credentials), "the token was refused" (never echoes it).
- `RemoteStore` never retries `UNAUTHENTICATED` or `PERMISSION_DENIED`; they are final (unlike `UNAVAILABLE`). A proxy's detail-less `UNAUTHENTICATED` maps to the same exit code.
- The server logs each refusal at `warn` with the peer address, the RPC and the reason class (missing, malformed, unknown, expired, role), rate-limited; accepted calls log the token `id` at `debug`. It never logs the header, the token, or its hash.
- `tower-http`/tonic trace layers are configured not to record request headers; OpenTelemetry spans (ADR 0009) record `auth.token_id` only, and the W3C propagation extractor reads only `traceparent`/`tracestate`.
- **Test:** the authenticated conformance run and the e2e suite run with `RUST_LOG=trace` captured to a buffer; the test fails if the token, its base64 body or its hex hash appears anywhere in the captured logs, in any `Status` message, or in `Debug` output of `ClientConfig`/`RemoteStore`. The same test runs for the cluster secret in the cluster testbed.

### D9. Health and MCP

- `grpc.health.v1` stays unauthenticated: load balancers and kubelets need it and it reveals only serving status.
- MCP over HTTP (`--mcp-listen`): with auth on, every request needs `Authorization: Bearer <token>` with the same token file and roles; story 33's write tools need `write`. The existing Origin/Host checks and loopback default stay. With auth off, ADR 0005 D4 is unchanged and the boxed warning remains. MCP over stdio has no socket; it uses the target's credentials (D2).

### D10. Test plan

- **Conformance, auth on, valid `write` token:** `run_all`, `run_differential(embedded, remote)` and `run_crash_rerun_differential` over `TestServer` with auth on pass unchanged.
- **Auth on, no token / wrong token / expired token:** every `StoreRead` and `Store` method fails with `StoreError::Unauthenticated` and returns no data (a table-driven test over every RPC, generated from the service descriptors so a new RPC cannot be missed); `Hello` succeeds and reports `auth_required`.
- **Auth on, `read` token:** reads pass; every write and admin RPC fails with `PermissionDenied`.
- **Auth off:** conformance without credentials passes unchanged, and a client that sends a token to an auth-off server is accepted (the token is ignored).
- **Forwarded-write path (cluster testbed):** a write with a valid `write` token sent to a follower is applied via the leader; with a `read` token the follower refuses it before forwarding; a call that sets `mg-forwarded-by`/`mg-forwarded-role: write` with no cluster secret is refused by the leader; a node with a wrong cluster secret cannot vote, append, install a snapshot or join.
- **Rotation:** two hashes accepted at once; removing one and reloading refuses it within one poll; a broken file on reload keeps the old set.
- **Constant time:** a unit test asserts the compare path visits every stored hash (a counting wrapper), rather than a flaky timing measurement.
- **Log scan:** D8.
- **Gate:** `check-no-c-deps.py` passes; `cargo tree -i ring` and `-i aws-lc-sys` print nothing.
- **Fuzz:** the header parser with arbitrary bytes never panics.

## Consequences

- An operator can expose `serve` beyond loopback with a meaningful access check, and story 33 (MCP write tools) is unblocked.
- Auth without TLS is weaker than it looks; the guard rail and docs make the proxy explicit, and #104 remains the real fix.
- Two secrets (client tokens, cluster secret) to distribute per node; no central store in v1.
- No per-tenant isolation: one `read` token reads every org.
- Small dependency additions (`subtle`, `zeroize`, plus crates already in the graph), all gate-clean.

## Open questions for the owner

- **Q1.** Is the `--insecure-transport` start-up refusal (D4) wanted, or only a warning?
- **Q2.** Should token files be replicated through Raft later (one place to rotate), or stay per-node files?
- **Q3.** Exit codes 7 and 8 as separate codes, or one code for both?
- **Q4.** Per-org/repo scopes (D6): file the follow-up now, and should it cover reads (result filtering) or only writes?
- **Q5.** `Hello` unauthenticated (D5) reveals the binary version and node id to any caller; acceptable, or trim it when `auth_required`?
