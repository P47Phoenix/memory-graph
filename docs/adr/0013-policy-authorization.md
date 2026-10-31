# ADR 0013: Policy-based authorization (per-org/repo scopes)

**Status:** Proposed (2026-10-31). Not accepted; the owner accepts it. Revised on 2026-10-31 after the architect and QA reviews of PR #326. Amends [ADR 0012](0012-authentication.md) D5 and D6 (and the D9 MCP role rule). Answers issue [#289](https://github.com/P47Phoenix/memory-graph/issues/289) (ADR 0012 Q4). Builds on [ADR 0004](0004-client-server-and-replication.md) (forwarding, Raft), [ADR 0005](0005-mcp.md) (MCP tools) and [ADR 0009](0009-opentelemetry.md) (telemetry).

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
- The in-house numbers are for an evaluator of the subset in D3 with a per-principal index; they are an estimate of the shipped code, which will add entity-hierarchy lookups (a hash-map probe each) and audit bookkeeping. The CI gate is the deterministic number of policies visited per decision; µs figures are recorded by an `--ignored` bench, not asserted (D10).
- Using Cedar would need a gate exception for `stacker` and `psm` in `scripts/c-deps-exceptions.txt`, i.e. a C toolchain on every build host, which CLAUDE.md's pure-Rust invariant exists to rule out.

## Decisions

| # | Question | Decision |
|---|---|---|
| D1 | Model | Principals `Token::"<id>"` in `Group::"<g>"` and the built-in `Role::"read"` / `Role::"write"`; actions `read`, `search`, `index`, `prune`, `delete`, `snapshot`, `status`, `admin`; resources `Org::"o"` ⊃ `Repo::"o/r"`, and `Cluster::"this"`. Default deny; `forbid` beats `permit`. |
| D2 | Policy file | `--auth-policies <file>` (convention: next to the token file, `tokens.toml` + `policies.cedar`), a strict Cedar subset; validated at load (unknown principals only warn, so revocation always works); reloaded with the token file as one pair; a bad file keeps the last good pair. |
| D3 | Engine | An in-house evaluator in `graph-server` (no new crate dependency). Not `cedar-policy` (gate failure, size, latency: see Spike). Policies stay valid Cedar, so operators can check them with Cedar tooling offline. |
| D4 | Backward compatibility | `role = "read"` / `"write"` become membership of `Role::"read"` / `Role::"write"` plus two built-in policies equal to ADR 0012 D5. No policy file means exactly v1 behaviour. Token files are unchanged; `scope` stays refused. |
| D5 | Read filtering | The server derives a `ReadScope` from the principal and pushes it into the store query. `Query`, `SymbolQuery` and the describe-family gain a scope; node-id reads answer out-of-scope ids as "not found". Proven by a scoped differential in `conformance.rs`. |
| D6 | Writes and admin | Per-request resource extraction: each `Index` file, `IndexFile`, `IngestExtraction` and `Prune` names its repo; whole-store operations act on `Cluster::"this"`. |
| D7 | Forwarding and cluster | The forwarding node checks; the leader re-checks with its own policy set; the principal travels as `mg-forwarded-token-id` and the leader resolves role and groups from its own token file. The cluster secret alone never authorises a scoped action, but it is a full-trust credential. Raft-internal RPCs and log apply are not policy-checked. |
| D8 | Snapshot and backup | Whole-store snapshot and backup listing need `snapshot` on `Cluster::"this"`; `UploadSnapshot` (replaces the store) needs `admin`; a read snapshot handle and its `SnapshotStats` are scoped. |
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
| `snapshot` | cluster | `Admin.TriggerSnapshot`, `ListBackups`; `serve --restore` is local, not an RPC | |
| `status` | cluster | `Admin.Status`, `Members`, `Leader`, `SysInfo`, `Metrics`; `ReadIndex` called on its own (not as a read barrier) | |
| `admin` | cluster | `Admin.AddLearner`, `Promote`, `Remove`, `TransferLeader`, `TriggerElect`, `Compact`, `Shutdown`, `UploadSnapshot` (replaces the whole store); `Write.Vacuum` (whole-store) | |

`read` and `search` are separate because "may list what exists" and "may full-text search the source" are different grants in practice (open question Q3). `CountNodes` is `read` but answers the scoped count (D5).

**Resources**
- `Org::"o"` and `Repo::"o/r"`, with `Repo::"o/r" in Org::"o"`. The hierarchy is computed from the name, not stored, so a resource that does not exist yet (the first `Index` of a new repo) can still be matched. `resource in Org::"acme"` matches the org itself and every repo in it.
- `Cluster::"this"`: the one cluster-level resource (whole-store and node operations).
- Org and repo names are compared byte for byte, exactly as the store stores them. A `Repo::` id is split at the **first** `/`: `Repo::"a/b/c"` is org `a`, repo `b/c`. An org name containing `/` cannot be addressed by a `Repo::` policy; it can be granted only through `Org::"..."` (which needs no split) or `resource` (any). At start, `serve` with `--auth-policies` logs a `warn` for every existing org whose name contains `/`, and `auth check` lists them; the indexer is not changed by this ADR (Q8).

**Admission for reads with no repo in the request.** `Get`, `Parent`, `Roots`, `Children(Page)`, `Descendants(Page)`, `Ancestors`, `CountNodes`, `OpenSnapshot`, `CloseSnapshot`, `SnapshotStats`, `DescribeByScan` and an unfiltered `Describe` name no repo. They are **admitted for every authenticated principal whose role covers `read`**, and the answer is computed through the principal's `ReadScope` (D5). A principal with zero read grants gets empty answers (no roots, `None`, count 0), never `PERMISSION_DENIED` and never HTTP 403 over MCP, so a zero-grant token learns nothing, not even that it has no grants. `ReadIndex` sent as the barrier of a linearizable read is part of that read's action (`read`/`search`), not `status` (D7), so scoped readers keep `--read linearizable`.

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
- **Validation at load** (start or reload) refuses the file, naming the line and the `@id` (in the server log and in `auth check`, never in a client-facing error), on: a parse error; an unknown action or entity type; a namespaced type (`Foo::Repo`); a duplicate `@id`; a `Repo::` id with no `/` or an empty org or repo part; more than 10,000 policies or a file over 1 MiB (exactly 10,000 policies and exactly 1 MiB are accepted).
- **Unknown principals are a warning, never an error (revocation must never fail open).** A `Token::` id or `Group::` that no token in the paired token file has is logged at `warn` and the policy simply matches nothing. Otherwise removing a token from `tokens.toml` would make the pair invalid, the reload would keep the last good pair, and the revoked token would keep working. Removing a token from the token file therefore **always** takes effect on the next reload, whatever the policy file says. A file with only `forbid`s is accepted (and logged at `warn`).
- **Edge cases** (each pinned by a test): an empty file or a comments-only file is a valid empty set (deny everything that is not a built-in, D4); a UTF-8 BOM at the start is accepted and skipped; CRLF and LF are equivalent; NUL bytes and invalid UTF-8 are load errors; `//` and `/* */` comments are accepted (Cedar has both); string escapes follow Cedar (`\"`, `\\`, `\n`, `\t`, `\u{...}`), and an invalid escape is an error; names are compared byte for byte with **no** Unicode normalisation, so NFC and NFD spellings are different resources (as in the store), and `auth check` warns when a policy name is not NFC; action names are case-sensitive (`Action::"Read"` is unknown, an error); `action in []` is valid Cedar and matches no action (accepted with a warning); `principal in Token::"x"` (a non-group) is valid Cedar and means `principal == Token::"x"`; annotations other than `@id` are accepted and ignored; a duplicate annotation key on one policy is an error (as in Cedar).
- **Reload:** the token file and the policy file are one `ArcSwap<AuthState { tokens, policies, policy_hash }>`, reloaded by the existing reloader (SIGHUP and the 5 s content-hash poll, one mutex, `reload_now()`). Both files are read and validated together; if either fails to parse, both keep the last good pair. The two files are not written atomically together, so a reload can see a new token file with an old policy file or the reverse. This **fails safe**: a new token with no policy yet gets nothing (default deny, and only its role's built-ins if `--auth-builtins` is on); a removed token is gone whatever the policies say (above); a new policy for a token not yet in the file matches nothing. The next poll picks up the second file.
- **In-flight `Index` streams re-check each file against the current `AuthState`** (loaded per file, not once at stream open), so a revocation or narrowing stops a long stream at the next file. Files already admitted into a batch that has not committed yet still commit if a revocation lands before the commit (the check is at admission, not at commit). On a forwarded stream, **both** the follower and the leader re-check each file against their own current `AuthState`. The per-file check, and the check on every call through a snapshot handle, also re-evaluate token `expires` against the clock, so expiry takes effect without a reload. Other requests use one `AuthState` from start to end.
- **Drift:** `cluster status` and `Admin.Status` report each node's `policy_hash` (SHA-256 of the normalised policy set plus the token-file id/role/groups, never hashes of tokens), so an operator sees which nodes differ.
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
- With `--auth-policies`, the built-ins are **not** loaded unless `serve --auth-builtins` is passed (default off, Q1). This is a CLI flag, not something in the policy file, so every accepted policy file stays plain valid Cedar. This makes "scoped" the meaning of a policy file, instead of every scoped grant being silently widened by `builtin:read`. A token's `role` still bounds it: a `read` token is never allowed `index`, `prune`, `delete`, `snapshot` or `admin`, whatever the policies say (the role is a ceiling, policies grant within it), so a policy typo cannot upgrade a `read` token.
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

**Hide cases** (each a conformance case):
- An org with no visible repo (none permitted, or all forbidden) **vanishes**: it is absent from `roots`, from `ancestors` of anything (which are all hidden anyway), from `describe` and from MCP `list_repos`.
- `children` of an org, and `describe`'s per-org aggregates, count only visible repos; page `total`s count only visible children.
- `extractor_gaps` lists only visible repos.
- **Same content in two repos:** a file with identical bytes indexed into a visible and a hidden repo. The store keeps it per repo (the fingerprint only makes re-indexing an unchanged file a no-op; it does not de-duplicate across repos), so each `(org, repo, path)` occurrence is separate: the visible occurrence is returned with the visible repo's path, the hidden occurrence never appears, and hit counts, ranks and `describe` totals count the visible occurrences only.

**Known residuals (accepted, documented in `docs/security.md`):**
- Node ids are opaque but allocated from shared counters, so gaps in the ids a scoped reader sees hint that other data exists.
- Snapshot handle ids and `ReadMeta`'s applied index and term advance with writes to hidden repos, which reveals write activity (not content).
- Timing side channels (a query over a large hidden org may take longer) are a non-goal.
- Out-of-scope reads produce no audit line (they are not refusals, D9).

**How tests prove "scoped read == unscoped read filtered"** (new cases in `conformance.rs`, run by `run_all` for every `Store`, embedded and `RemoteStore`):
1. `run_scoped_differential(make_store, scope)`: index the conformance corpus into store A; index **only the in-scope repos** of the same inputs into store B, **in the same relative order as A** (A's input sequence with the hidden repos' files removed), so that APIs ordered by node id (`roots`, `describe_by_scan`, node-id pages; v2.rs orders these by node id) see the same relative order in both stores; then every query in the conformance query list (search at every grain, symbol search, describe at each level, file_tokens, outline, children/descendants pages at several offsets and limits, count_nodes per kind) must answer identically on `A.scoped(scope)` and on `B`. **Comparison:** each result is mapped to a stable key with node ids removed: `(org, repo, path, kind, byte_start, byte_end, text)` for nodes and hits, `(org, repo)` plus counts for describe rows. Every list is compared in the order the API returns it. **No query-visible order changes:** node-id-ordered APIs keep their order, and the relative-order rule above is what makes them comparable. Only for `search` and `search_symbols` is there an extra **insertion-order case** (the same inputs indexed in two different orders must give equal answers), because their ranking is meant to be content-defined; story 73a first confirms their tie-break key, and if it turns out to depend on node ids the case is dropped and recorded, not "fixed" by changing the order. Lists without a defined order are sorted by the key before comparing. This is the strong form: a scoped reader cannot tell the hidden repos exist, through counts, page totals, limits, ordering or errors (except the residuals above). It also runs with a hidden repo that shares file content with a visible one (the same-content case).
2. The weak form, for the API that keeps node ids: for every query, `A.scoped(scope).q()` equals `filter(scope, A.q_unlimited())` truncated to the requested limit and offset.
3. Out-of-scope probes: every node id of a hidden repo, every hidden `(org, repo, path)` and every hidden org name gives the same answer (`None` / empty) as an id or name that never existed; the `StoreError` values (and their `Display`) are equal.
4. `run_scoped_differential` runs inside the `run_differential` matrix (every chunk/cache/jobs/memory/compaction setting) and inside `run_crash_rerun_differential` (A is the crashed-and-rerun store), and `ReadScope::All` equals the unscoped store exactly.
5. Scopes are generated by proptest from the corpus's org/repo names (including `forbid` holes inside a permitted org).

### D6. Writes, admin and per-request resources

- **Resource extraction** happens after authentication and before any store work, from the request itself: `IndexFile` and `IngestExtraction` name their `(org, repo)`; each `Index` stream message is checked as it is read, against the **current** `AuthState` (D2). A denied file fails the stream with `PERMISSION_DENIED` before that file is prepared; batches already committed stay committed, exactly like any other mid-stream failure today (Q6 asks whether to require the repo up front instead).
- **`Prune` is decided on policy alone, never on what the store holds.** A prune naming one repo needs `prune` on that `Repo`; an org-wide prune needs `prune` on the `Org` itself (a policy with `resource in Org::"o"` or `resource == Org::"o"`, or `resource` any); a store-wide prune needs `prune` on `Cluster::"this"`. Per-repo grants that happen to cover every repo the catalog lists today do **not** authorise an org-wide prune, so the answer cannot depend on (or reveal) the catalog. A `forbid` on any repo inside the org denies the org-wide prune. To avoid a time-of-check/time-of-use gap, the leader resolves the prune's repo list once, at admission, and writes the explicit `(org, repo)` list into the Raft `LogCommand`; apply prunes exactly that list, so a repo created after admission is never pruned by it.
- **A token that may `index` a repo but not `read` it is an intended state** (a CI indexer that must not read other teams' code back); it is tested.
- Cluster actions (`snapshot`, `status`, `admin`) check `Cluster::"this"`; `Vacuum` is `admin` because it rewrites the whole store.
- The `need()` table keeps failing closed: a method with no action entry is refused for every credential, and `every_rpc_is_classified` now checks the action table.

### D7. Forwarding, cluster and Raft

- **The forwarding node checks first**, with its own `AuthState`, exactly as for a direct call (D6), and refuses there.
- It forwards with the cluster secret and `mg-forwarded-by`, `mg-forwarded-role` and `mg-forwarded-token-id` (as in ADR 0012). No token, hash, group list or policy text travels.
- **The leader re-checks with its own view of the principal.** It looks up `mg-forwarded-token-id` in **its own** token file and takes the role and groups from there; any groups header is ignored. An id the leader does not know (or whose token has expired there) is **denied**. The role ceiling (D4) is the leader's role for that id, intersected with `mg-forwarded-role`. The call proceeds only if both nodes allowed it (the effective permission is the intersection). During a rollout, a grant takes effect once it is on both nodes, and a revocation once it is on either. (This changes ADR 0012's rotation-skew rule when `--auth-policies` is set: a token known only to the follower is refused at the leader.)
- **Header parsing:** each `mg-forwarded-*` header must appear exactly once, be ASCII, at most 128 bytes, and `mg-forwarded-token-id` must match the token file's id syntax. A missing, repeated, empty, oversized or malformed header makes the call `PERMISSION_DENIED`, never a fallback to another principal.
- **The cluster secret alone is never enough for a scoped action:** a cluster-secret call with `mg-forwarded-by` but no valid `mg-forwarded-token-id` is refused for every `read`/`search`/`index`/`prune`/`delete`/`snapshot`/`status`/`admin` action whenever `--auth-policies` is set. (Without a policy file, ADR 0012's `mg-forwarded-role` rule is unchanged.)
- The headers are honoured only on a cluster-secret call (as `mg-forwarded-role` is today); on a client-token call they are ignored, so a client cannot claim another principal.
- **Threat model.** Client tokens are untrusted: everything a client sends is checked against the server's own files. The **cluster secret is a full-trust credential**: whoever holds it can name any `mg-forwarded-token-id` and act as that token at the leader, within that token's grants, and can speak Raft directly (ADR 0012 D3). Policies therefore limit clients, not nodes. Per-node secrets (so a leader can tell which node forwarded and bound what it may claim) or mTLS node identities (#104) are future work.
- **Raft and revocation:** a revocation is not retroactive. Entries the leader admitted before a reload and that are already in the log are applied, on every replica, after the reload.
- **Raft-internal RPCs** (`AppendEntries`, `Vote`, `InstallSnapshot`, `Join`, `UpdateAdvertise`) stay `Node`-only and are **not** policy-checked. The leader authorises a write at admission, and the state machine applies committed entries **without** re-checking: policy files are per node and may differ, and a check at apply would make replicas diverge.
- Forwarded `Admin.ReadIndex` carries the action of the read it serves (`read` or `search`, admitted as in D1 for any principal with a read role), marked by a new `mg-forwarded-action: read|search` header with the same strict parsing as the other `mg-forwarded-*` headers (exactly once, ASCII, one of the two values, else `PERMISSION_DENIED`), so scoped readers keep `--read linearizable`. A `ReadIndex` without the marker is standalone and is `status`. The marker is honoured only on a cluster-secret call: a client token's direct `ReadIndex` carrying `mg-forwarded-action` is treated as standalone (`status`), and a test pins that. Forwarded membership changes are `admin`.

### D8. Snapshot and backup

- `TriggerSnapshot` and `ListBackups` produce or reveal whole-store artefacts, which no repo-scoped grant can safely cover, so they need `snapshot` on `Cluster::"this"`. A scoped token never gets a whole-store snapshot. `UploadSnapshot` **replaces** the store, so it is `admin` on `Cluster::"this"`, not `snapshot`.
- `SnapshotStats` on a handle opened by a scoped principal reports scoped figures (counts over visible repos only).
- `serve --restore` and the `snapshots/` directory are local operations, bounded by filesystem access (out of scope, as in ADR 0012).
- `OpenSnapshot` (a read-consistency handle, ADR 0004) is `read`; the handle stores the `ReadScope` it was opened with as well as the token id (ADR 0012 D5), so a later reload that narrows the policy also applies on the next call through the handle (the scope is recompiled from the current `AuthState`, intersected with the stored one, and the token's `expires` is re-checked on every call).
- `InstallSnapshot` between nodes is Raft-internal (D7).

### D9. Telemetry, audit and errors

- **Audit log:** every denial, and every allowed write or admin action, is logged at `info` on target `mg::audit` with: token id, groups, action, resource (`Repo::"acme/web"`), decision and the matched policy `@id`s. Allowed reads are logged at `debug` only (volume). Denial lines keep ADR 0012's rate limit per peer.
- **Never logged or exported:** the token, its hash, the `authorization` header, the cluster secret, and policy text.
- **Metrics:** `mg_authz_decisions_total{action, decision}` (allow/deny), `mg_authz_policies` (gauge), `mg_authz_reload_total{result}`, and `mg_authz_decision_seconds` (histogram). No token id, group, resource or policy id label (ADR 0012 D9: no per-token labels; also cardinality).
- **OTel spans** (ADR 0009) gain `authz.action` and `authz.decision` only. Resource names and policy ids are not span attributes (they may be customer names), and no policy text appears anywhere. A test enumerates the label set of every `mg_authz_*` metric and the attribute set of exported spans and fails on anything else.
- **Policy `@id`s never appear in client-facing errors** (status messages, details, MCP results, CLI stderr); they are in the server's audit log only. The client sees "permission denied for action `index`".
- **Errors and exit codes:**
  - a denied write, prune, snapshot, status or admin action: `PERMISSION_DENIED` with the `PermissionDenied` detail plus the new `action` field; the CLI exits **9** (unchanged);
  - an out-of-scope read: never an error; it answers like an absent org, repo, file or node (D5), so existence does not leak;
  - a denied `index` of a repo that does not exist yet: `PERMISSION_DENIED` (same answer whether or not it exists);
  - MCP over HTTP: a denied write tool is HTTP 403 (as ADR 0012 D9); an out-of-scope read tool returns an empty result, not an error;
  - an invalid policy file: `serve` refuses to start (exit 1); on reload it keeps the last good pair and logs at `error` with the line and `@id`, never the text.
- **`memory-graph auth check --tokens <f> --policies <f>`** validates a pair offline and `--explain <token-id> <action> <resource>` prints the decision and matched `@id`s, for operators and CI.

### D10. Test plan

**Refusal per action and resource** (table-driven from the action table, so a new RPC or tool cannot be missed)
- For each action x {permitted repo, sibling repo in the same org, other org, cluster}: allow, or `PERMISSION_DENIED` (writes) / absent (reads), embedded and over `TestServer`.
- `forbid` beats `permit` at every level combination (org permit + repo forbid, repo permit + org forbid, group permit + token forbid).
- Default deny: an empty permit set denies everything; a method with no action entry is refused for every credential.
- The role ceiling: a `read` token with a `permit` for `index` is still denied.
- A zero-grant principal gets empty answers from every no-repo read (D1), over gRPC and MCP HTTP (200, not 403).
- An index-but-not-read token can index its repo and sees nothing when it reads (D6).
- Org-wide prune: allowed only with `prune` on the `Org` or `Cluster`; refused with per-repo grants that cover every current repo; the `LogCommand` carries the admission-time repo list, and a repo created between admission and apply survives.
- `UploadSnapshot` needs `admin`; `TriggerSnapshot`/`ListBackups` need `snapshot`.
- Backward compatibility: every ADR 0012 D10 test passes with no policy file, and again with an empty policy file plus `--auth-builtins`.

**Revocation**
- A token that a policy names is removed from `tokens.toml`; after `reload_now()` its next call is `UNAUTHENTICATED`, and the reload succeeded (the policy naming it only warns).
- A policy naming an unknown token or group loads with a warning and matches nothing.

**Scoped reads** (D5): `run_scoped_differential` (strong and weak forms) inside the `run_differential` matrix and `run_crash_rerun_differential`; the search-only insertion-order case; the same-content case; each hide case; out-of-scope probes indistinguishable from missing; counts, page totals and `SnapshotStats` on scoped handles; proptest scopes; the same through `RemoteStore` and MCP; `--read linearizable` with a scoped token, direct and forwarded.

**Forwarded path** (cluster testbed)
- Allowed on both nodes: applied. Denied on the follower: refused before forwarding (a counting leader sees no call). Allowed on the follower, denied on the leader: refused by the leader, store unchanged.
- The leader uses its own role and groups: a follower whose token file puts the id in more groups, or gives it `write`, cannot widen it at the leader. An id unknown at the leader is denied.
- Missing, repeated, empty, oversized and non-ASCII `mg-forwarded-*` headers are each `PERMISSION_DENIED`.
- A cluster-secret call with `mg-forwarded-by` and no `mg-forwarded-token-id`: refused for every scoped action.
- A client token sending `mg-forwarded-token-id`: ignored.
- Policy skew during a rollout (grant on one node only, revoke on one node only).
- Replicas apply identical entries when their policy files differ; an entry admitted before a revoking reload is still applied (not retroactive).
- `cluster status` shows differing `policy_hash`es.

**MCP**: each tool x scope over stdio and `/mcp`; `tools/list` hides write tools with no grant; a denied `index_path` is 403 over HTTP and an `isError` result over stdio; read tools return the scoped differential's answers; a zero-grant token gets empty results.

**Reload races** (all through `reload_now()`)
- Token and policy file changed together, separately, half-written, deleted, invalid: a parse failure keeps the last good pair; a mixed old/new pair fails safe (each of the three mixed cases in D2 is asserted).
- An `Index` stream is stopped at the next file by a revoking reload.
- A widening reload and a narrowing reload against an open snapshot handle (the handle follows the intersection rule of D8).
- A reload during a forwarded call (between the follower's check and the leader's).
- A SIGHUP and a poll together give one state.
- A ~1,000-cycle loop alternating good and bad files under concurrent load: no request ever sees a state that was not a validated pair, no panic, and the final state is the last good pair.

**Fuzzed policy parser**
- Proptest over arbitrary bytes and over grammar-generated policies: never panics, never accepts outside the grammar, print -> parse round-trips.
- The D2 edge-case list, one test each: empty file, comments-only, BOM, CRLF, NUL, invalid UTF-8, `/* */` comments (including unterminated), NFC vs NFD names, every escape and invalid escapes, namespaced types, action-name case, `action in []`, `principal in` a non-group, limits at exactly N and N+1 (policies, bytes), annotations (unknown, duplicate key, duplicate `@id`).
- **Cedar oracle (dev-only CI job, recommended in Q5):** a separate workflow, outside the workspace and the gate (like `xtask`), builds a tiny tool on `cedar-policy` and compares **decisions** with our evaluator on generated (policy set, principal, action, resource) triples, using a checked-in Cedar schema and entities file. Every disagreement found is saved as a replay fixture under `testdata/authz/` that the normal test suite runs without Cedar.

**Mutation testing** (`cargo mutants`; every surviving mutant is killed by a new test or listed in the PR with a reason). Targets:
- the evaluator: the forbid-beats-permit combination, the default-deny return, the action bitset test, `in` versus `==` matching, the `Org` ⊃ `Repo` split at the first `/`;
- the role ceiling and the built-ins switch;
- the parser's limit checks and the unknown-principal warning path;
- `ReadScope` compilation and the store's per-candidate scope bit test, the scoped count and page-total code;
- the forwarded-header parser, the leader-side token-id lookup and the unknown-id deny;
- the org-wide prune rule and the `LogCommand` repo list;
- the paired reload's keep-last-good branch.

**Leak and performance**
- The ADR 0012 D8 leak scan extended to policy text, policy `@id`s in client-facing output, and the forwarded headers.
- **CI gate (deterministic):** the number of policies visited per decision, from a counter in `decide`, is asserted on fixed fixtures: at most the principal's token bucket plus its groups' buckets plus the wildcard bucket. The worst case, 500 policies all with `principal` (wildcard), is asserted to visit exactly 500 and nothing more.
- **Recorded, not asserted:** `--ignored` benches record decision p50/p99 (including the 500-wildcard case) and scoped versus unscoped search on the bench corpus; the numbers go into `docs/spikes/` and the PR, not into CI assertions.
- The size gate is unchanged.

**Gate**: `check-no-c-deps.py` and `cargo tree -i ring` / `-i aws-lc-sys` stay clean (no new runtime dependency).

## Rollout

**PRs** (each with docs in the same PR):
1. **PR 1, policy engine and file** (stories 71 and 71a): `authz.rs` parser and evaluator, `groups` in the token file, `--auth-policies`, `--auth-builtins`, the paired reload, `policy_hash`, `auth check --explain`, the parser fuzz and mutation tests. Enforcement is wired for writes and cluster actions on direct calls. **Interim rule, until PR 3:** a policy file is refused at load if any `permit` **or `forbid`** whose action set includes `read` or `search` constrains `resource` (anything but plain `resource`); otherwise a repo-scoped forbid would be accepted and silently not enforced on reads. So in PR 1 reads are decided **all-or-nothing per principal**, on `Cluster::"this"` / any resource: a principal either reads everything or nothing. Tests: a file with a resource-constrained read/search permit, and one with a resource-constrained read/search forbid, each fail to load naming the `@id`; with an unconstrained read permit a principal reads every repo, and with none (or an unconstrained read forbid) every read answers empty. **Forwarded writes in PR 1:** only the follower evaluates policies; the leader still applies ADR 0012's `mg-forwarded-role` check, because the leader re-check (D7) lands in PR 2. Until PR 2, a cluster with `--auth-policies` must run the same policy file on every node, and the PR 1 docs say so.
2. **PR 2, enforcement on writes, forwarding and MCP writes** (story 72): per-request resource extraction and the prune rule (D6), the leader re-check (D7), snapshot/backup (D8), MCP write tools, audit log and metrics (D9), the forwarded-path tests.
3. **PR 3, scoped reads** (stories 73a, 73b, 73c; may land as three PRs if the reviews prefer): `ReadScope` in `graph-store` (D5), the scoped view, `run_scoped_differential`, MCP read tools; lifts the PR 1 interim rule.

**Proposed epic stories** (for the owner to approve; not added to the epic by this PR):

| Story | Title | Points |
|---|---|---|
| 71 | Policy engine, policy file and paired reload | 8 |
| 71a | Cedar decision oracle (dev-only CI job, fixtures) | 2 |
| 72 | Scoped writes, forwarding re-check, snapshot/admin actions, MCP write tools, audit | 8 |
| 73a | Query push-down (`search`, `search_symbols`, describe family) and the strong differential | 8 |
| 73b | Node-id scoped view, counts, page totals, snapshot handles | 5 |
| 73c | MCP read tools on the scoped view | 2 |
| 74 | Operator docs: `docs/security.md` policy guide, examples, `auth check` in CI | 2 |

Acceptance per story:
- **71:** the D2 grammar and every edge case pass; unknown principals warn and revocation takes effect; the paired reload's keep-last-good and fail-safe mixed cases pass; built-ins off by default and on with `--auth-builtins`; no policy file means every ADR 0012 test passes unchanged; the role ceiling holds; the interim read rule is enforced; `policy_hash` in `cluster status`; parser fuzz and the evaluator/parser mutation targets pass; the policies-visited gate passes.
- **71a:** the oracle job runs on generated triples with the checked-in schema and entities; zero disagreements, or each one fixed and kept as a replay fixture run by `cargo test`.
- **72:** the refusal table for every write and cluster action; the org-wide prune rule and the `LogCommand` repo list; every forwarded-path test; `UploadSnapshot` is `admin`; audit lines and metrics present, with the label and leak checks passing; MCP write tools give 403 / `isError`.
- **73a:** `ReadScope` on `Query`, `SymbolQuery` and the describe family; `run_scoped_differential` strong and weak forms, under the `run_differential` matrix and crash-rerun, with B indexed in A's relative order, the search/symbol-search insertion-order case and the same-content case; `--read linearizable` with a scoped token.
- **73b:** `StoreRead::scoped` for every node-id API; the hide cases; zero-grant empty answers; scoped `count_nodes`, page totals and `SnapshotStats`; out-of-scope probes indistinguishable from missing.
- **73c:** every MCP read tool over stdio and HTTP answers like the scoped differential; a zero-grant token gets empty results (200), not 403; `tools/list` hides ungranted write tools.
- **74:** `docs/security.md` covers policies, the threat model (cluster secret as full trust), the residuals and the `/` rule, with worked examples checked by `auth check` in CI.

## Consequences

- A token can be limited to orgs and repos for reads and writes, with deny rules that cannot be overridden, and v1 deployments change nothing.
- `StoreRead` grows a scope parameter and a scoped view; every store implementation and the conformance suite pick them up. This is the largest part of the work (stories 73a-73c).
- The policy language is a subset of Cedar without conditions; anything richer needs a new ADR (or `cedar-policy` once it is gate-clean).
- Policies are per node like tokens; an operator must roll them to every node, and D7's intersection rule makes a partial rollout fail safe. `policy_hash` in `cluster status` shows drift.
- **The cluster secret is a full-trust credential.** Its holder can impersonate any token at the leader (within that token's grants there) and speak Raft directly. Policies limit clients, not nodes. Per-node secrets or mTLS (#104) are future work.
- Scoped readers can still infer that hidden data exists from node-id gaps, snapshot handle ids, `ReadMeta` index/term and timing (D5 residuals).
- A revocation does not undo entries already in the Raft log.
- The audit log carries resource names; operators who ship logs off-host should know that.

## Open questions for the owner

- **Q1.** With a policy file, should the built-in `role` policies be **off** by default and turned on only by `serve --auth-builtins` (recommended, D4), or on by default?
- **Q2.** Should an out-of-scope read, and every no-repo read by a zero-grant principal, look exactly like an absent resource (recommended, D1/D5/D9), or return `PERMISSION_DENIED` so operators see misconfigurations more easily (at the cost of leaking that the repo exists)?
- **Q3.** Keep `read` and `search` as separate actions (recommended), or merge them into one `read`?
- **Q4.** Is the scope-only Cedar subset (no `when`/`unless` conditions, no attributes) enough for v1, or is there a concrete need for conditions (for example, time windows or path prefixes inside a repo)?
- **Q5.** May a dev-only CI job use `cedar-policy` as a decision oracle on generated triples (outside the workspace and the gate, with a checked-in schema, entities and replay fixtures; story 71a)? Recommended: yes.
- **Q6.** For `Write.Index` streams, check each file as it arrives against the current policy set (recommended, D6; batches committed before a denied file stay), or require the stream to declare its `(org, repo)` up front and refuse any file outside it?
- **Q7.** With `--auth-policies`, a token known only to the follower is refused at the leader (D7), which narrows ADR 0012's rotation-skew rule. Accept that (recommended), or keep honouring follower-only tokens?
- **Q8.** Existing org names containing `/` can be granted only through `Org::` or wildcard policies (D1). Is a startup warning enough (recommended), or should the indexer refuse `/` in org names from now on?
- **Q9.** Is the interim PR 1 rule (no resource-constrained read permits until PR 3) acceptable, or should PR 1 wait for PR 3?
- **Q10.** Approve stories 71, 71a, 72, 73a, 73b, 73c and 74, their points and acceptance lists, for the epic, and the PR breakdown?

