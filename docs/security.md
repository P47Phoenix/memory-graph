# Security: authentication and transport

How to run `memory-graph serve` with authentication, and what it does and does not protect. The design is [ADR 0012](adr/0012-authentication.md).

```text
+----------------------------------------------------------------------+
| WARNING: memory-graph has no TLS. With --auth-tokens, bearer tokens, |
| the cluster secret and all indexed data still cross the network in   |
| clear text. Anyone on the path can read a token and replay it. Bind  |
| to loopback, or put a TLS-terminating proxy or a mesh in front.      |
+----------------------------------------------------------------------+
```

## What auth protects

- Without `--auth-tokens` (the default), anyone who can reach the gRPC port can read every indexed token, write, prune, shut the node down or change the cluster's membership.
- With it, every call needs a bearer token. A `read` token can call every query, `Admin.Status`, `Members`, `Leader`, `SysInfo`, `Metrics` and `ListBackups`. A `write` token can also index, prune, vacuum and run every other admin call. Node-to-node calls (Raft, `Join`, `UpdateAdvertise`) need the cluster secret, which no client token can stand in for.
- A method the server does not list in its role table is refused for every credential.
- Auth protects against callers who can reach the port but cannot see the traffic. It does not protect against an on-path attacker (see the box above), against a holder of a `write` token (backups are the recovery, [ADR 0006](adr/0006-snapshots-object-storage.md)), or against local access to `graph.redb`.
- There are no per-org or per-repo permissions yet: one `read` token reads every org. The follow-up is [#289](https://github.com/P47Phoenix/memory-graph/issues/289).

## Tokens

Create a token and add its entry to the server's token file:

```sh
memory-graph auth new-token --id ci-indexer --role write --append /etc/memory-graph/tokens.toml > ci.token
chmod 600 ci.token
```

- The token (`mgt_` plus 43 characters) is printed once. The file gets only its id, role and SHA-256; the server never stores the token, so a lost token is replaced, not recovered.
- `--token-out <file>` writes the token to a new file with mode 0600 instead of stdout. `--expires 2027-01-01T00:00:00Z` refuses it from that instant on.
- Without `--append`, the `[[token]]` entry goes to stderr:

```toml
[[token]]
id = "ci-indexer"       # logged on use; never the secret
role = "write"          # read | write
sha256 = "9f86d0..."    # 64 hex characters
# expires = "2027-01-01T00:00:00Z"
```

The server refuses the whole file, at start or on a reload, if any entry has an unknown key, an unknown role, a duplicate id or hash, a malformed hash or `expires`, or a `scope` (per-org/repo permissions are a separate policy file, below). An optional `groups = ["a", "b"]` names the token's groups for policies. A file with no tokens is refused, because it would lock everyone out.

## Running the server

```sh
memory-graph serve --db graph.redb --auth-tokens tokens.toml
```

- The token file is reloaded when its content changes (checked every 5 s, and at once on SIGHUP on Unix). A file that fails to read or validate keeps the set already loaded and logs an error; a reload never turns auth off. Rotation: add the new token, hand it out, then remove the old one.
- On Unix the server warns when the token file or the cluster secret file is readable by group or others. On Windows, restrict both files to the service account.
- Each node has its own token file; it is not replicated. Distribute it like any other secret.

### A cluster

Every node of a `--data-dir` cluster also needs the same cluster secret:

```sh
memory-graph auth new-secret --out cluster.secret      # once; copy to every node
memory-graph serve --data-dir /data --bootstrap --auth-tokens tokens.toml \
  --cluster-secret-file cluster.secret --listen 10.0.0.1:7000 --insecure-transport
```

- Every node-to-node call carries the secret: Raft, `Join` and its probes, `UpdateAdvertise`, the leader's probe of a joiner, and forwarded writes, membership changes and read barriers.
- A follower checks the client's token and role itself, then forwards with the secret and the role it granted (`mg-forwarded-role`). The client's token never travels between nodes, so forwarding keeps working while the nodes' token files differ during a rotation.
- The secret is the most powerful credential: whoever holds it can append Raft entries and install snapshots. It is at least a `write` token plus node authority. A direct `Store` or `Write` call with only the secret is refused (`PERMISSION_DENIED`), but that is defence in depth, not an isolation boundary: the holder can add `mg-forwarded-by` and `mg-forwarded-role: write` and read or write anything. Keep the secret as tightly as the data itself.
- Rotating the secret: the file may hold two lines, the current one first. Add the new secret as a second line on every node, swap the two lines on every node, then remove the old line. Each step is reloaded like the token file, and the cluster stays available throughout.

## Policies (per-org/repo permissions)

> **Roll policy and token files to every node.** The node a client calls decides first, and a call it forwards is decided again by the leader with the leader's own files (ADR 0013 D7). A grant therefore takes effect once it is on both nodes, and a revocation once it is on either. Compare `policy_hash` in `cluster status` on every node after each change.

[ADR 0013](adr/0013-policy-authorization.md). Without a policy file a token's `role` is all there is (above). With one, rules in a scope-only subset of [Cedar](https://www.cedarpolicy.com/) decide what each token may do:

```sh
memory-graph serve --db graph.redb --auth-tokens tokens.toml --auth-policies policies.cedar
```

```cedar
// policies.cedar, next to tokens.toml
@id("web-ci")
permit(principal == Token::"web-ci", action in [Action::"index", Action::"prune"], resource == Repo::"acme/web");

@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource in Repo::"acme/legal");

@id("readers")
permit(principal in Group::"readers", action in [Action::"read", Action::"search", Action::"status"], resource);
```

- **Model.** Principals: `Token::"<id>"` (the token file's `id`), `Group::"<g>"` (a token's `groups`) and `Role::"read"` / `Role::"write"`. Actions: `read`, `search`, `index`, `prune`, `delete`, `snapshot`, `status`, `admin`. Resources: `Org::"o"`, which contains every `Repo::"o/r"` (the id splits at the first `/`), and `Cluster::"this"`. Anything not permitted is denied; a `forbid` beats any `permit`.
- **Built-ins.** With a policy file, nothing is allowed that a policy does not permit. `--auth-builtins` adds the two role policies (`read` reads, searches and sees status; `write` may do anything). The role is always a ceiling: a `read` token never gets `index`, `prune`, `delete`, `snapshot` or `admin`, whatever the policies say.
- **What is checked.** Writes and cluster actions: `Index` (each file of a stream against the current files), `IndexFile` and `IngestExtraction` on the repo; `Prune`/`Delete` of one repo on the repo, of a whole org only with a grant on the `Org` (or the cluster) and no `forbid` on any repo inside it. Writes never depend on what the store holds: a repo that does not exist yet is decided the same way. An org-wide delete deletes the repos the leader listed when it admitted the call; a repo created after that survives. The list holds names, so a repo deleted and re-created under the same name in between is still deleted. `Vacuum`, `Compact`, membership, `UploadSnapshot` (it replaces the whole store) and `Shutdown` are `admin`, `TriggerSnapshot` is `snapshot`, and `Status`, `Members`, `Leader`, `SysInfo`, `Metrics`, `ListBackups` and a standalone `ReadIndex` are `status`, all on `Cluster::"this"`.
- **Forwarded calls.** A follower forwards a write, a membership change or a read barrier with the cluster secret, its own id (`mg-forwarded-by`), the role it granted (`mg-forwarded-role`) and the caller's token id (`mg-forwarded-token-id`); no token, group or policy travels. The leader looks the id up in **its own** token file and takes the role and groups from there; an id it does not know (or that expired there) is refused, and the role is the lower of the two. Each `mg-forwarded-*` header must appear exactly once, be printable ASCII and at most 128 bytes; anything else is `PERMISSION_DENIED`. A linearizable read's barrier carries `mg-forwarded-action: read|search` and is admitted for any token whose role covers `read`, so scoped readers keep `--read linearizable`; without it a `ReadIndex` is `status`. A client token's own `mg-forwarded-*` headers are ignored.
- **The cluster secret alone never authorises an action** with a policy file: a forwarded call without a valid token id is refused. A node's own calls, today the `--bootstrap-or-join` removal of its lost incarnation, present the token named by `serve --auth-node-token-id <id>` (a token in the node's own token file; `serve` refuses an id it does not have): the leader decides `admin` on `Cluster::"this"` for that token like any forwarded principal, so grant it, for example `permit(principal == Token::"node-ops", action == Action::"admin", resource == Cluster::"this");`, in every node's policy file. Without the flag, or without a grant, the removal is refused and the old member must be removed by hand (`memory-graph cluster remove`). Without a policy file nothing changes (ADR 0012). The secret is still a full-trust credential: its holder can name any token id and act as that token at the leader, within that token's grants (above).
- Audit denial lines are rate-limited per peer address (at most 4,096 peers tracked; past that, lines are counted as suppressed). A forwarded call's peer is the forwarding node, so every client behind one follower shares that node's window; MCP requests use the HTTP client's address.
- **Reads are scoped per repo** (ADR 0013 D5). For each token and read action (`read` or `search`, decided separately), a repo is *visible* if the policies allow the action on it; an org is visible if it has a visible repo or the token is allowed the action on the `Org` itself. Two rules:
  - A request that **names nothing** (an unfiltered search or symbol search, `describe` of everything, extractor gaps) is filtered inside the store query, before ranking, `--limit` and paging: hidden repos never show up in its rows, counts or pages, and it is never refused.
  - A request that **names something** (an org or repo filter on `search`, `symbols` or `describe`, a file for `file_tokens`) checks existence first: a name that does not exist now (never indexed, or deleted) gets the usual empty answer; an existing hidden one is `PERMISSION_DENIED` (exit 9). A repo filter without an org (`--repo r`) answers with the visible repos named `r` and is refused only if every existing one is hidden. A visible org that also holds hidden repos answers with its visible repos only.
  - The refusal reveals that the named org, repo or file exists (the owner's decision, ADR 0013 Q2), never its content; operators who need names kept secret must use separate servers.
  - **Node-id reads** (`get`, `children`, `descendants`, `ancestors`, `roots`, `count_nodes`) are still all or nothing in this release: a token whose scope is not everything gets empty `roots`/`count_nodes` and `PERMISSION_DENIED` for an existing node id (story 74b scopes them).
- **MCP.** The write tools run as the request's token: `index_path` and `prune` are decided on their repo exactly like `IndexFile` and `Prune` (and forwarded to the leader with the token id). A policy refusal is an `isError` result with `permission_denied` (HTTP 200) on both transports; HTTP 403 stays for a `read` token calling a write tool, and 401 for a missing or invalid token. The read tools follow the read rules above, decided by the Store service for the request's token.
- `cluster status` prints each node's `policy_hash` (a hash of the policies, the built-ins switch and the token file's ids, roles and groups) so differing nodes stand out.
- A policy refusal carries the denied action in the `PermissionDenied` error detail (`action`), never a resource name or a policy `@id`; the CLI exits 9.
- **Validation and reload.** The token file and the policy file are one pair: read, validated and swapped together, at start and on every reload (5 s poll, SIGHUP). If either is invalid, both keep the last good pair, and an invalid policy file at start refuses `serve` (exit 1). A policy naming a token or group the token file does not have only warns and matches nothing, so removing a token always revokes it. Errors name the line and the `@id`, never in a client-facing message: a client sees only `permission denied for action <a>` (exit 9).
- **Check a pair offline**, for operators and CI:

```sh
memory-graph auth check --tokens tokens.toml --policies policies.cedar
memory-graph auth check --tokens tokens.toml --policies policies.cedar --explain web-ci index 'Repo::"acme/web"'
```

- An org whose name contains `/` cannot be addressed by a `Repo::` policy; `serve` warns about each at start. Grant it through `Org::"..."` or an unconstrained `resource`.
- Every decision of a token is logged on target `mg::audit` (token id, groups, role, action, resource such as `Repo::"acme/web"`, decision, matched `@id`s; denials and writes at `info`, denials rate-limited per peer like refusals, allowed reads at `debug`), never a token or policy text. The resource names in it are customer names: treat the audit log like the data. Metrics: `mg_authz_decisions_total{action,decision}`, `mg_authz_policies`, `mg_authz_reload_total{result}` and `mg_authz_decision_seconds`, with no token, group, resource or policy label; RPC spans gain only `authz.action` and `authz.decision`. The audit log is effectively local: none of its fields are in the OTLP log allow-list (`ALLOWED_LOG_FIELDS`), so an OTLP collector receives at most the bare message `authz`. Keep the server's own log output if you need the audit trail.

## The transport guard

With `--auth-tokens`, `serve` refuses to start when `--listen`, `--advertise`, `--mcp-listen` or `--metrics-listen` is not loopback (`127.0.0.0/8`, `::1`, `localhost`), unless `--insecure-transport` is given. `0.0.0.0` and `[::]` are not loopback. A multi-host cluster always needs the flag, because its advertised addresses are not loopback: that is the risk the box above describes, acknowledged on purpose.

## Putting TLS in front

Terminate TLS in a proxy next to the server and let the server listen on loopback, or use a mesh (Linkerd, Istio) with mTLS between pods.

Envoy (gRPC over HTTP/2 to a loopback upstream):

```yaml
static_resources:
  listeners:
  - address: { socket_address: { address: 0.0.0.0, port_value: 7443 } }
    filter_chains:
    - transport_socket:
        name: envoy.transport_sockets.tls
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext
          common_tls_context:
            tls_certificates:
            - certificate_chain: { filename: /etc/certs/server.crt }
              private_key: { filename: /etc/certs/server.key }
      filters:
      - name: envoy.filters.network.http_connection_manager
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
          stat_prefix: memory_graph
          route_config:
            virtual_hosts:
            - name: mg
              domains: ["*"]
              routes: [{ match: { prefix: "/" }, route: { cluster: mg } }]
          http_filters: [{ name: envoy.filters.http.router, typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router } }]
  clusters:
  - name: mg
    type: STATIC
    typed_extension_protocol_options:
      envoy.extensions.upstreams.http.v3.HttpProtocolOptions:
        "@type": type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions
        explicit_http_config: { http2_protocol_options: {} }
    load_assignment:
      cluster_name: mg
      endpoints: [{ lb_endpoints: [{ endpoint: { address: { socket_address: { address: 127.0.0.1, port_value: 7000 } } } }] }]
```

Caddy:

```text
mg.example.com {
    reverse_proxy h2c://127.0.0.1:7000
}
```

The client does not speak TLS either: `--server https://...` is refused. On the client side, run a local proxy (or a mesh sidecar) that accepts plain gRPC on loopback and connects to the server over TLS, and point `--server` at it. Native TLS is [#104](https://github.com/P47Phoenix/memory-graph/issues/104).

## Clients

```sh
memory-graph --server mg:7000 --token-file ci.token index --org o --repo r .
```

- The token comes from `--token-file`, then `MEMORY_GRAPH_TOKEN_FILE`, then `MEMORY_GRAPH_TOKEN`. There is no flag that takes the token itself, because command lines show in `ps` and shell history.
- `MEMORY_GRAPH_TOKEN` is convenient but visible to the process's environment (`/proc/<pid>/environ`), crash dumps and CI logs that print the environment. Prefer a file with mode 0600; the client warns on Unix when the file is readable by group or others.
- Exit codes: **8** when the server requires a token and none was given, or it refused the one given; **9** when the token's role may not do this. Neither is retried. Messages never contain a token or an environment variable's value.
- A malformed local credential (a token file or `MEMORY_GRAPH_TOKEN` that does not hold an `mgt_` token, or a file that cannot be read) is a local configuration error: the client refuses it before calling the server, and exits **1**, not 8.
- `memory-graph mcp` over stdio uses the same credentials.

## Health, metrics and MCP

- `grpc.health.v1` stays open (load balancers and kubelets need it); `memory-graph health` needs no token.
- `--metrics-listen` stays open and exposes aggregate counters only, including `mg_auth_refusals_total{reason}`; never a token id. Bind it to loopback or scrape it through the proxy; it is under the same transport guard.
- With `--auth-tokens`, the MCP HTTP endpoint (`--mcp-listen`) needs `Authorization: Bearer <token>` on every request: a missing or wrong token gets 401 with `WWW-Authenticate: Bearer`, and a token whose role is too low for a tool gets 403. The Origin and Host checks of [docs/mcp.md](mcp.md) still apply.
- `Store.Hello` answers an unauthenticated caller only the protocol version and that a token is required.

## What is logged

Refusals are logged at `warn` with the peer address, the method and a reason class (missing, malformed, unknown, expired, role, node_only, secret_direct, unlisted), at most one line per peer per 10 s with a count of the suppressed ones. Accepted calls log the token id at `debug`, and OpenTelemetry spans carry `auth.token_id` and `auth.role`. No log line, error, status, span or metric ever contains a token, the cluster secret, their SHA-256 or the `authorization` header; a test at `trace` level checks it. `serve` takes its log filter from `--log-level` (or `MEMORY_GRAPH_LOG`), not `RUST_LOG`: `--log-level trace` is the trace level.

An `authorization` header longer than 128 bytes is refused as malformed. One so large that the request's header block exceeds the HTTP/2 limits (about 16 KB) is refused by the HTTP layer before authentication runs (431); a gRPC client sees `UNKNOWN`, and nothing is answered or logged as an auth refusal.
