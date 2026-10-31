# ADR 0013: Policy-based authorization (per-org/repo scopes)

**Status:** Proposed (2026-10-31). Not accepted; the owner accepts it. Amends [ADR 0012](0012-authentication.md) D5 and D6 (and the D9 MCP role rule). Answers issue [#289](https://github.com/P47Phoenix/memory-graph/issues/289) (ADR 0012 Q4). Builds on [ADR 0004](0004-client-server-and-replication.md) (forwarding, Raft), [ADR 0005](0005-mcp.md) (MCP tools) and [ADR 0009](0009-opentelemetry.md) (telemetry).

## In plain words

1. Today a token is `read` or `write` for the **whole** database. This ADR lets an operator say "this token may read and search `acme/*`, and index only `acme/web`", and "nobody may prune `acme/legal`".
2. Rules are **policies** written in a small subset of the [Cedar](https://www.cedarpolicy.com/) language: `permit(...)` and `forbid(...)` over a **principal** (a token, or a group of tokens), an **action** (read, search, index, prune, ...) and a **resource** (an org, a repo, or the cluster). Anything not permitted is denied, and a `forbid` always beats a `permit`.
3. The policies live in a file next to the token file, are checked when loaded, and are reloaded like the token file. A bad file keeps the last good set.
4. **Existing token files keep working unchanged.** `role = "read"` and `role = "write"` become two built-in policies with exactly today's meaning.
5. **Scoped reads are filtered inside the store query**, not after it, so a scoped token cannot learn about other orgs through counts, page sizes, "not found" versus "denied", or result limits.
6. We **do not** use the `cedar-policy` crate: it fails the pure-Rust gate (`stacker`/`psm` compile C and assembly with `cc`), and it adds about 90 crates, about 5.3 MB and about a minute of compile time. A small in-house evaluator of the Cedar subset we need is about 500x faster per decision in the spike.

## Context

References are to `origin/main` on 2026-10-31 (`c7be3cd`).

- `crates/graph-server/src/auth.rs`: `need(path) -> Option<Need>` is the static deny-by-default table from gRPC method to `Open | Read | Write | Node`. `Auth::check` authenticates (constant-time hash compare), applies the table and puts a `Caller` (token id, role, forwarded role) into the request extensions. `parse_token_file` refuses a `scope` key ("reserved for per-org/repo permissions (#289)"). Reload is `ArcSwap<TokenSet>` with SIGHUP plus a 5 s content-hash poll and `reload_now()`.
- `forward.rs`: a non-leader forwards writes, membership changes and `Admin.ReadIndex` to the leader with the cluster secret and `mg-forwarded-by`; `auth.rs` adds `mg-forwarded-role` and `mg-forwarded-token-id`. The leader honours `mg-forwarded-role` only on a cluster-secret call (ADR 0012 D3).
- `graph-server/src/mcp/mod.rs`: `tool_need(name) -> Role`: `index_path` and `prune` (delivered by story 33, PR #306) need `write`, every other tool `read`.
- `StoreRead` (`graph-store/src/api.rs`) has two families of reads:
  - **named:** `describe(org, repo)`, `describe_by_scan`, `extractor_gaps(org, repo)`, `file_tokens(org, repo, path)`, `search(&Query)`, `search_symbols(&SymbolQuery)` (both with optional org/repo filters);
  - **by node id or global:** `get`, `parent`, `roots`, `children(_page)`, `descendants(_page)`, `ancestors`, `count_nodes(kind)`.
- MCP tools: `describe`, `list_repos`, `search`, `find_symbols`, `file_outline`, `file_tokens`, `list_files`, `index_path`, `prune`.

## Spike (2026-10-31)

A throwaway workspace under `D:/tmp/adr13-spike` (deleted afterwards) had three binaries: an empty `baseline`, `cedarbin` (`cedar-policy 4.14.0`, the latest stable), and `inhouse` (the evaluator sketched in D3). Windows 11, x86_64-pc-windows-msvc, `--release`, `strip = true`, clean target dir per build.

**Workload:** 10 orgs x 10 repos (`Repo::"oN/rM"` in `Org::"oN"`), 20 tokens in 5 groups, **50 policies** (20 token-to-org read/search permits, 10 group-to-repo read permits, 10 token-to-repo index/prune permits, 10 group-to-repo prune forbids). 200,000 random (token, action, repo) requests; latency is the `is_authorized` / `decide` call only (request construction excluded), two runs each.

| | `cedar-policy 4.14.0` | in-house evaluator |
|---|---|---|
| Pure-Rust gate (`check-no-c-deps.py`, all shipped targets) | **Fails**: `stacker 0.1.25` (build.rs uses `cc`), `psm 0.1.32` (build.rs uses `cc`), `defmt 1.2.0` (`links`) | Passes (no dependencies) |
| Same, `default-features = false` | **Fails** identically: `stacker` is a non-optional dependency of `cedar-policy-core` | |
| `cargo tree -i ring` / `-i aws-lc-sys` (`--target all`) | Empty | Empty |
| Dependencies (unique crates, `cargo tree -e normal --target all`) | **90** (of which the repo already has some, e.g. `serde`) | 0 |
| Clean release build of the binary | **67.8 s** (baseline 2.7 s) | 0.9 s |
| Stripped binary size | 5,475,840 B: **+5.34 MB** over baseline (139,264 B) | 175,616 B: +36 KB |
| Parse 50 policies | 1.40-1.67 ms | 30-54 µs |
| Decision latency p50 / p99 | **53.7-56.8 µs / 72.3-85.7 µs** (max 19-33 ms, one-off) | **<= 0.1 µs / <= 0.2 µs** (timer resolution; max 5-11 µs) |
| Allowed decisions (same requests) | 11,269 / 200,000 | 11,269 / 200,000 (identical: a differential check) |

Notes:
- `stacker`/`psm` grow the stack for Cedar's recursive evaluator; they cannot be removed by features. `defmt` is listed by `cargo metadata` through `jiff`, but `cargo tree -i defmt` prints nothing, so it is probably a weak-feature edge the gate over-reports; it was not investigated further because `stacker` already fails the gate.
- The in-house numbers are for an evaluator of the subset in D3 with a per-principal index; they are an estimate of the shipped code, which will add entity-hierarchy lookups (a hash-map probe each) and audit bookkeeping. Budget: p99 < 2 µs for 500 policies (a test, D10).
- Using Cedar would need a gate exception for `stacker` and `psm` in `scripts/c-deps-exceptions.txt`, i.e. a C toolchain on every build host, which CLAUDE.md's pure-Rust invariant exists to rule out.

## Decisions

| # | Question | Decision |
|---|---|---|
| D1 | Model | Principals `Token::"<id>"` in `Group::"<g>"` and the built-in `Role::"read"` / `Role::"write"`; actions `read`, `search`, `index`, `prune`, `delete`, `snapshot`, `status`, `admin`; resources `Org::"o"` ⊃ `Repo::"o/r"`, and `Cluster::"this"`. Default deny; `forbid` beats `permit`. |
| D2 | Policy file | `--auth-policies <file>` (convention: next to the token file, `tokens.toml` + `policies.cedar`), a strict Cedar subset; validated at load; reloaded with the token file as one pair; a bad file keeps the last good pair. |
| D3 | Engine | An in-house evaluator in `graph-server` (no new crate dependency). Not `cedar-policy` (gate failure, size, latency: see Spike). Policies stay valid Cedar, so operators can check them with Cedar tooling offline. |
| D4 | Backward compatibility | `role = "read"` / `"write"` become membership of `Role::"read"` / `Role::"write"` plus two built-in policies equal to ADR 0012 D5. No policy file means exactly v1 behaviour. Token files are unchanged; `scope` stays refused. |
| D5 | Read filtering | The server derives a `ReadScope` from the principal and pushes it into the store query. `Query`, `SymbolQuery` and the describe-family gain a scope; node-id reads answer out-of-scope ids as "not found". Proven by a scoped differential in `conformance.rs`. |
| D6 | Writes and admin | Per-request resource extraction: each `Index` file, `IndexFile`, `IngestExtraction` and `Prune` names its repo; whole-store operations act on `Cluster::"this"`. |
| D7 | Forwarding and cluster | The forwarding node checks; the leader re-checks with its own policy set; the principal travels in `mg-forwarded-*` headers. The cluster secret alone never authorises a scoped action. Raft-internal RPCs and log apply are not policy-checked. |
| D8 | Snapshot and backup | Whole-store snapshot, upload, backup listing and restore need `snapshot` on `Cluster::"this"`; a read snapshot handle inherits the caller's `ReadScope`. |
| D9 | Telemetry and errors | Audit log target `mg::audit`; metric `mg_authz_decisions_total{action,decision}`; no policy text, token, hash or resource name in OTel. Out-of-scope reads look like "absent"; denied writes are `PERMISSION_DENIED` (exit 9). |
| D10 | Tests | See the test plan. |

### D1. Model

**Principals**
- `Token::"<id>"`: the token file's `id` (never the secret).
- `Group::"<name>"`: optional; a token lists its groups in the token file: `groups = ["indexers", "acme-readers"]`. Groups are flat in v1 (no group-in-group), which keeps the hierarchy check one hash-map probe.
- `Role::"read"`, `Role::"write"`: built-in groups that every token belongs to according to its `role` (D4). `write` tokens are in both.
- The cluster secret is **not** a principal. It identifies a node, and a node acts only for a forwarded principal (D7).

**Actions** (each gRPC method and MCP tool maps to exactly one; the static table replaces `Need::Read`/`Need::Write`; `Open` and `Node` stay outside policy):

| Action | Resource | gRPC methods | MCP tools |
|---|---|---|---|
| `read` | repo | `Store.Get`, `Parent`, `Roots`, `Children(Page)`, `Descendants(Page)`, `Ancestors`, `FileTokens`, `Describe`, `DescribeByScan`, `ExtractorGaps`, `CountNodes`, `OpenSnapshot`, `CloseSnapshot`, `SnapshotStats` | `describe`, `list_repos`, `list_files`, `file_outline`, `file_tokens` |
| `search` | repo | `Store.Search`, `SearchSymbols` | `search`, `find_symbols` |
| `index` | repo | `Write.Index`, `IndexFile`, `IngestExtraction` | `index_path` |
| `prune` | repo | `Write.Prune` | `prune` |
| `delete` | repo / org | reserved: no RPC deletes a whole repo or org today; the action exists so policies written now stay valid when one is added | |
| `snapshot` | cluster | `Admin.TriggerSnapshot`, `UploadSnapshot`, `ListBackups`; `serve --restore` is local, not an RPC | |
| `status` | cluster | `Admin.Status`, `Members`, `Leader`, `SysInfo`, `Metrics`, `ReadIndex` | |
| `admin` | cluster | `Admin.AddLearner`, `Promote`, `Remove`, `TransferLeader`, `TriggerElect`, `Compact`, `Shutdown`; `Write.Vacuum` (whole-store) | |

`read` and `search` are separate because "may list what exists" and "may full-text search the source" are different grants in practice (open question Q3). `CountNodes` is `read` but answers the scoped count (D5).

**Resources**
- `Org::"o"` and `Repo::"o/r"`, with `Repo::"o/r" in Org::"o"`. The hierarchy is computed from the name, not stored, so a resource that does not exist yet (the first `Index` of a new repo) can still be matched. `resource in Org::"acme"` matches the org itself and every repo in it.
- `Cluster::"this"`: the one cluster-level resource (whole-store and node operations).
- Org and repo names are compared byte for byte, exactly as the store stores them; a `/` in an org name is refused at policy load (it would make `Repo::` ids ambiguous).

**Evaluation:** default deny. A request is allowed iff at least one `permit` matches and no `forbid` matches. Policy order never matters.

### D2. Policy file, validation and reload

```cedar
// policies.cedar, next to tokens.toml
@id("acme-read")
permit(principal in Group::"acme-readers", action in [Action::"read", Action::"search"], resource in Org::"acme");

@id("web-ci")
permit(principal == Token::"web-ci", action in [Action::"index", Action::"prune"], resource == Repo::"acme/web");

@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource in Repo::"acme/legal");
```

- **Grammar:** the Cedar policy grammar restricted to: effect `permit|forbid`; scope only (no `when`/`unless` conditions in v1, open question Q4); `principal`, `principal == E`, `principal in E`; `action`, `action == A`, `action in [A, ...]`; `resource`, `resource == E`, `resource in E`; `@id("...")` annotations; `//` comments. Anything else (a condition, `is`, `like`, an unknown entity type or action, a template slot) is a **load error**, never silently ignored. Every accepted file is valid Cedar with the same meaning, which a test checks against the `cedar` CLI in a dev-only job (not in the shipped graph, Q5).
- **Location:** `serve --auth-policies <path>`, requires `--auth-tokens`. The docs recommend `policies.cedar` next to `tokens.toml` with the same permissions and the same per-node distribution (ADR 0012 Q2: per-node files, not replicated through Raft).
- **Validation at load** (start or reload) refuses the file, naming the line and the `@id`, on: a parse error; an unknown action or entity type; a `Token::` id or `Group::` that no token in the current token file has (a typo would silently grant nothing; checked against the token file of the same reload pair); a duplicate `@id`; an org name with `/`; more than 10,000 policies or a file over 1 MiB. A file with only `forbid`s is accepted (and logged at `warn`).
- **Reload:** the token file and the policy file are one `ArcSwap<AuthState { tokens, policies }>`, reloaded by the existing reloader (SIGHUP and the 5 s content-hash poll, one mutex, `reload_now()`). Both files are read and validated together; if either fails, **both keep the last good pair** (so a new token and the policy that scopes it can never be split by a half-applied reload). A request loads one `AuthState` at its start and uses it to the end, including every per-file check of an `Index` stream.
- At start, an invalid policy file refuses `serve` (exit 1, a configuration error, like an invalid token file).

### D3. Engine: in-house, not `cedar-policy`

- The spike's `cedar-policy 4.14.0` fails the gate on its mandatory `stacker`/`psm` dependencies, and costs 90 crates, +5.3 MB stripped, about +65 s of clean compile and ~55 µs per decision. The in-house sketch decides the same 200,000 requests identically in <= 0.2 µs p99.
- **Shape:** `authz.rs` in `graph-server` (or a small `graph-authz` crate if the CLI's `auth check` subcommand needs it without the server): a hand-written recursive-descent parser over the grammar in D2 (no parser-generator dependency), `Policy { id, effect, principal: Scope, actions: ActionSet (a u16 bitset), resource: Scope }`, and an index `principal -> [policy]` for `Token::`, each `Group::`/`Role::`, and `principal` (any). `decide(principal, groups, action, resource) -> Decision { allowed, matched: Vec<PolicyId> }` walks only the principal's buckets.
- **`ReadScope` compilation** (D5): for a principal and an action, the policy set compiles to `ReadScope::All` or `ReadScope::Only { orgs: BTreeSet, repos: BTreeSet, deny_orgs, deny_repos }` once per request; the store tests membership per repo, never per token.
- Revisit `cedar-policy` only if it drops `stacker` (or makes it optional) and conditions (`when`) become a requirement.

### D4. Backward compatibility

- No `--auth-policies`: the policy set is exactly the two built-ins, and every v1 test passes unchanged:
  ```cedar
  @id("builtin:read")  permit(principal in Role::"read",  action in [Action::"read", Action::"search", Action::"status"], resource);
  @id("builtin:write") permit(principal in Role::"write", action, resource);
  ```
- With `--auth-policies`, the built-ins are **not** loaded unless the file says `@builtins("on")` at the top (default off, Q1). This makes "scoped" the meaning of a policy file, instead of every scoped grant being silently widened by `builtin:read`. A token's `role` still bounds it: a `read` token is never allowed `index`, `prune`, `delete`, `snapshot` or `admin`, whatever the policies say (the role is a ceiling, policies grant within it), so a policy typo cannot upgrade a `read` token.
- The token file is unchanged except for the new optional `groups` key. `scope` stays refused, now with a message pointing to `--auth-policies`.
- Auth off: no policy evaluation at all (ADR 0012 behaviour); `--auth-policies` without `--auth-tokens` refuses to start.
- No `PROTOCOL_VERSION` bump: the only proto change is an additive `action` field in the `PermissionDenied` detail.

### D5. Read filtering inside the store

Filtering after the query is not enough: a `search` with `limit = 10` that matched 10 out-of-scope hits returns 0 instead of the 10 in-scope hits further down, a page's `total` counts hidden rows, `count_nodes` reveals the size of other orgs, and "denied" versus "not found" reveals that a repo exists. So the scope goes into the query.

**`StoreRead` changes** (graph-store, all implementations; `RemoteStore` sends nothing new, because the server derives the scope from the caller and a client can never widen it):

| API | Change |
|---|---|
| `Query`, `SymbolQuery` | New field `scope: ReadScope` (default `All`). The store intersects it with the org/repo filters **before** ranking, `limit` and paging. |
| `describe`, `describe_by_scan`, `extractor_gaps` | New `ReadScope` argument (via a `ReadOpts { scope }` struct to keep the trait object-safe and extensible); repos outside the scope are absent from the result, and a named org/repo outside it returns the same empty result as a missing one. |
| `file_tokens` | Out-of-scope `(org, repo)` returns `Ok(None)`, as for a missing file. |
| `get`, `parent`, `children(_page)`, `descendants(_page)`, `ancestors`, `roots` | A new `StoreRead::scoped(&self, ReadScope) -> Box<dyn StoreRead + '_>` view (like `snapshot()`) that resolves each node's repo (the V2 stream already knows each node's file and repo) and answers an out-of-scope node exactly like a missing id: `Ok(None)` / empty / `NotFound`. `roots` lists only in-scope orgs, and `children` of an org only in-scope repos. Page `total`s count only visible children. |
| `count_nodes(kind)` | On a scoped view, the sum over in-scope repos from the `catalog` table (O(repos)), not the global count. |
| `snapshot()` | A snapshot of a scoped view stays scoped. |

`search` and `search_symbols` read postings per file; the scope check is a repo-id bitset built once per query from the catalog, so the cost is one bit test per candidate file. The size gate and on-disk format are untouched (no stored bytes change, so no schema bump).

**MCP:** every read tool goes through the scoped view, so `list_repos`, `describe`, `list_files`, `search`, `find_symbols`, `file_outline` and `file_tokens` inherit the filtering; `tools/list` hides `index_path`/`prune` from a principal that has no `index`/`prune` permit anywhere (they are still refused if called).

**How tests prove "scoped read == unscoped read filtered"** (new cases in `conformance.rs`, run by `run_all` for every `Store`, embedded and `RemoteStore`):
1. `run_scoped_differential(make_store, scope)`: index the conformance corpus into store A; index **only the in-scope repos** of the same inputs into store B; then every query in the conformance query list (search at every grain, symbol search, describe at each level, file_tokens, outline, children/descendants pages at several offsets and limits, count_nodes per kind) must answer identically on `A.scoped(scope)` and on `B`, comparing by stable keys (org, repo, path, span), never by node id. This is the strong form: a scoped reader cannot tell the hidden repos exist, through counts, page totals, limits, ordering or errors.
2. The weak form, for the API that keeps node ids: for every query, `A.scoped(scope).q()` equals `filter(scope, A.q_unlimited())` truncated to the requested limit and offset.
3. Out-of-scope probes: every node id of a hidden repo, every hidden `(org, repo, path)` and every hidden org name gives the same answer (`None` / empty) as an id or name that never existed; the `StoreError` values (and their `Display`) are equal.
4. `run_differential` keeps holding per scope across chunk/cache/jobs settings, and `ReadScope::All` equals the unscoped store exactly.
5. Scopes are generated by proptest from the corpus's org/repo names (including `forbid` holes inside a permitted org).

### D6. Writes, admin and per-request resources

- **Resource extraction** happens after authentication and before any store work, from the request itself: `IndexFile` and `IngestExtraction` name their `(org, repo)`; each `Index` stream message is checked as it is read, against the `AuthState` loaded at stream open. A denied file fails the stream with `PERMISSION_DENIED` before that file is prepared; batches already committed stay committed, exactly like any other mid-stream failure today (Q6 asks whether to require the repo up front instead).
- `Prune` names a repo and a file set; an org-wide or store-wide prune needs `prune` on the org, or on every repo it would touch, resolved against the catalog before any work.
- Cluster actions (`snapshot`, `status`, `admin`) check `Cluster::"this"`; `Vacuum` is `admin` because it rewrites the whole store.
- The `need()` table keeps failing closed: a method with no action entry is refused for every credential, and `every_rpc_is_classified` now checks the action table.

### D7. Forwarding, cluster and Raft

- **The forwarding node checks first**, with its own `AuthState`, exactly as for a direct call (D6), and refuses there.
- It forwards with the cluster secret and: `mg-forwarded-by`, `mg-forwarded-role`, `mg-forwarded-token-id` (as in ADR 0012), plus new `mg-forwarded-groups` (comma-separated group names). No token, hash or policy text travels.
- **The leader re-checks** with its own policy set, for the principal named in those headers, and the call proceeds only if both nodes allowed it (the effective permission is the intersection). During a policy rollout, a grant takes effect once it is on both the follower and the leader, and a revocation once it is on either.
- **The cluster secret alone is never enough for a scoped action:** a cluster-secret call with `mg-forwarded-by` but no `mg-forwarded-token-id` is refused for every `read`/`search`/`index`/`prune`/`delete`/`snapshot`/`admin` action whenever `--auth-policies` is set. (Without a policy file, ADR 0012's `mg-forwarded-role` rule is unchanged.) The forwarded-role ceiling (D4) still applies.
- The headers are honoured only on a cluster-secret call (as `mg-forwarded-role` is today); on a client-token call they are ignored, so a client cannot claim another principal or extra groups.
- **Raft-internal RPCs** (`AppendEntries`, `Vote`, `InstallSnapshot`, `Join`, `UpdateAdvertise`) stay `Node`-only and are **not** policy-checked. The leader authorises a write at admission, and the state machine applies committed entries **without** re-checking: policy files are per node and may differ, and a check at apply would make replicas diverge.
- Forwarded `Admin.ReadIndex` is `status`; forwarded membership changes are `admin`.

### D8. Snapshot and backup

- `TriggerSnapshot`, `UploadSnapshot` and `ListBackups` produce or reveal whole-store artefacts, which no repo-scoped grant can safely cover, so they need `snapshot` on `Cluster::"this"`. A scoped token never gets a whole-store snapshot.
- `serve --restore` and the `snapshots/` directory are local operations, bounded by filesystem access (out of scope, as in ADR 0012).
- `OpenSnapshot` (a read-consistency handle, ADR 0004) is `read`; the handle stores the `ReadScope` it was opened with as well as the token id (ADR 0012 D5), so a later reload that narrows the policy also applies on the next call through the handle (the scope is recompiled from the current `AuthState`, intersected with the stored one).
- `InstallSnapshot` between nodes is Raft-internal (D7).

### D9. Telemetry, audit and errors

- **Audit log:** every denial, and every allowed write or admin action, is logged at `info` on target `mg::audit` with: token id, groups, action, resource (`Repo::"acme/web"`), decision and the matched policy `@id`s. Allowed reads are logged at `debug` only (volume). Denial lines keep ADR 0012's rate limit per peer.
- **Never logged or exported:** the token, its hash, the `authorization` header, the cluster secret, and policy text.
- **Metrics:** `mg_authz_decisions_total{action, decision}` (allow/deny), `mg_authz_policies` (gauge), `mg_authz_reload_total{result}`, and `mg_authz_decision_seconds` (histogram). No token id, group, resource or policy id label (ADR 0012 D9: no per-token labels; also cardinality).
- **OTel spans** (ADR 0009) gain `authz.action` and `authz.decision` only. Resource names and policy ids are not span attributes (they may be customer names), and no policy text appears anywhere.
- **Errors and exit codes:**
  - a denied write, prune, snapshot, status or admin action: `PERMISSION_DENIED` with the `PermissionDenied` detail plus the new `action` field; the CLI exits **9** (unchanged);
  - an out-of-scope read: never an error; it answers like an absent org, repo, file or node (D5), so existence does not leak;
  - a denied `index` of a repo that does not exist yet: `PERMISSION_DENIED` (same answer whether or not it exists);
  - MCP over HTTP: a denied write tool is HTTP 403 (as ADR 0012 D9); an out-of-scope read tool returns an empty result, not an error;
  - an invalid policy file: `serve` refuses to start (exit 1); on reload it keeps the last good pair and logs at `error` with the line and `@id`, never the text.
- **`memory-graph auth check --tokens <f> --policies <f>`** validates a pair offline and `--explain <token-id> <action> <resource>` prints the decision and matched `@id`s, for operators and CI.

### D10. Test plan

**Refusal per action and resource** (table-driven from the action table, so a new RPC or tool cannot be missed)
- For each action x {permitted repo, sibling repo in the same org, other org, cluster}: allow or `PERMISSION_DENIED` (writes) / absent (reads) as expected, embedded and over `TestServer`.
- `forbid` beats `permit` at every level combination (org permit + repo forbid, repo permit + org forbid, group permit + token forbid).
- Default deny: an empty permit set denies everything; a method with no action entry is refused for every credential.
- The role ceiling: a `read` token with a `permit(... action ...)` for `index` is still denied.
- Backward compatibility: every ADR 0012 D10 test passes with no policy file, and again with a policy file holding only `@builtins("on")`.

**Scoped reads** (D5): `run_scoped_differential` (strong and weak forms), out-of-scope probes indistinguishable from missing, count and page-total tests, proptest scopes; the same through `RemoteStore` and MCP.

**Forwarded path** (cluster testbed)
- Allowed on both nodes: applied. Denied on the follower: refused before forwarding (a counting leader sees no call). Allowed on the follower, denied on the leader: refused by the leader, store unchanged.
- A cluster-secret call with `mg-forwarded-by` and no `mg-forwarded-token-id`: refused for every scoped action.
- A client token sending `mg-forwarded-token-id`/`mg-forwarded-groups`: ignored.
- Policy skew during a rollout (grant on one node only, revoke on one node only).
- Replicas apply identical entries when their policy files differ (no check at apply).

**MCP**: each tool x scope over stdio and `/mcp`; `tools/list` hides write tools with no grant; a denied `index_path` is 403 over HTTP and an `isError` result over stdio; read tools return the scoped differential's answers.

**Reload races**: token and policy file changed together, separately, half-written, deleted, invalid: the pair stays consistent and last-good is kept; an `Index` stream keeps the `AuthState` it opened with; a SIGHUP and a poll together give one state; a narrowing reload takes effect on the next call through an open snapshot handle; all through `reload_now()`.

**Fuzzed policy parser**: a `cargo fuzz`-style proptest over arbitrary bytes and over grammar-generated policies (never panics, never accepts outside the grammar, round-trips print -> parse); a differential against the `cedar` CLI on generated policies in a dev-only CI job (Q5).

**Mutation testing**: `cargo mutants` on `authz.rs`, the scope push-down in `graph-store` and the forwarding header code; every surviving mutant is either killed by a new test or listed in the PR with a reason.

**Leak and performance**: the ADR 0012 D8 leak scan extended to policy text and the new headers; p99 decision latency < 2 µs with 500 policies; scoped search within 5% of unscoped on the bench corpus; the size gate unchanged.

**Gate**: `check-no-c-deps.py` and `cargo tree -i ring` / `-i aws-lc-sys` stay clean (no new runtime dependency).

## Rollout

**PRs** (each with docs in the same PR):
1. **PR 1, policy engine and file** (story 71): `authz.rs` parser and evaluator, `groups` in the token file, `--auth-policies`, the paired reload, built-ins, `auth check --explain`, the parser fuzz and mutation tests. Enforcement is wired for cluster actions and writes on direct calls only; no read filtering yet, so a policy file that grants scoped reads is refused at load until PR 3 (no false sense of isolation).
2. **PR 2, enforcement on writes, forwarding and MCP writes** (story 72): per-request resource extraction (D6), `mg-forwarded-token-id`/`-groups` and the leader re-check (D7), snapshot/backup (D8), MCP write tools, audit log and metrics (D9), the forwarded-path tests.
3. **PR 3, scoped reads** (story 73): `ReadScope` in `graph-store` (D5), the scoped view, `run_scoped_differential` in `conformance.rs`, MCP read tools, the load-time refusal from PR 1 lifted.

**Proposed epic stories** (for the owner to approve; not added to the epic by this PR):

| Story | Title | Points |
|---|---|---|
| 71 | Policy engine, policy file and paired reload (PR 1) | 5 |
| 72 | Scoped writes, forwarding re-check, snapshot/admin actions, MCP write tools, audit (PR 2) | 8 |
| 73 | Scoped reads pushed into the store, scoped differential conformance (PR 3) | 13 |
| 74 | Operator docs: `docs/security.md` policy guide, examples, `auth check` in CI | 2 |

## Consequences

- A token can be limited to orgs and repos for reads and writes, with deny rules that cannot be overridden, and v1 deployments change nothing.
- `StoreRead` grows a scope parameter and a scoped view; every store implementation and the conformance suite pick them up. This is the largest part of the work (story 73).
- The policy language is a subset of Cedar without conditions; anything richer needs a new ADR (or `cedar-policy` once it is gate-clean).
- Policies are per node like tokens; an operator must roll them to every node, and D7's intersection rule makes a partial rollout fail safe.
- The audit log carries resource names; operators who ship logs off-host should know that.

## Open questions for the owner

- **Q1.** With a policy file, should the built-in `role` policies be **off** by default (recommended: a policy file means "scoped", D4), or on unless the file disables them?
- **Q2.** Should an out-of-scope read look exactly like an absent resource (recommended, D5/D9), or return `PERMISSION_DENIED` so operators see misconfigurations more easily (at the cost of leaking that the repo exists)?
- **Q3.** Keep `read` and `search` as separate actions (recommended), or merge them into one `read`?
- **Q4.** Is the scope-only Cedar subset (no `when`/`unless` conditions, no attributes) enough for v1, or is there a concrete need for conditions (for example, time windows or path prefixes inside a repo)?
- **Q5.** May the `cedar` CLI be used as a dev-only differential oracle in a separate CI job (outside the shipped graph and the gate), or should the parser be tested only against our own grammar?
- **Q6.** For `Write.Index` streams, check each file as it arrives (recommended, D6; batches committed before a denied file stay), or require the stream to declare its `(org, repo)` up front and refuse any file outside it?
- **Q7.** Approve stories 71-74 and their points for the epic, and the three-PR breakdown?
