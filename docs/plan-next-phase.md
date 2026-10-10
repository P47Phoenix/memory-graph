# memory-graph: backlog triage and next-phase plan

Prepared 2026-10-09 by the product owner (read-only triage; nothing was changed on GitHub). Updated the same day with the owner's decisions and the team vote (section 7). The stories are now epic stories 58-69 in [epic-code-memory-graph.md](epic-code-memory-graph.md#story-58).

**Outcome (2026-10-09).** The owner accepted [ADR 0003](adr/0003-data-model.md) and [ADR 0009](adr/0009-opentelemetry.md), approved the #262 format change, put infix search (#278) in scope and asked for auth (#105) to be scheduled. Adding the stories to the epic was delegated to team consensus; the architect, developer and QA all voted to add them as stories 58-69. Infix search and auth each get an ADR (0011 and 0012) before any code.

| Plan story | Epic story | Points |
|---|---|---|
| S1 (#276) | 58 | 3 |
| S2 (#278 docs) | 59 | 1 |
| S3 (#253) | 60 | 3 |
| S4 (#262) | 61 | 5 |
| S5 (#260) | 62 | 3 |
| S6 (#254) | 63 | 3 |
| S7 (#266) | 64 | 2 |
| S8 (#252) | 65 | 2 |
| S9 (#268) | 66 | 1 |
| S10 (#115) | 67 | 5 |
| S11 (#278 infix) | 68 | 5 (re-sized from 8: symbol names only, per the developer) |
| S12 (#105 auth) | 69 | 8 |

The sections below are the triage as written, with dated notes where a decision changed them.
Inputs: 21 open issues, `origin/main` epic (stories 1-57) and ADRs 0001-0010, the last 40 merged PRs (#215-#277), and `docs/spikes/gpu-acceleration.md` (branch `spike/gpu-acceleration`, PR #277).

## 1. Where things stand

- ADRs **Accepted**: 0001 (provisional), 0002, 0004, 0005, 0006, 0007, 0008, 0010. **Proposed**: 0003 (data model; the v2 store it describes is the only format in production) and 0009 (OpenTelemetry; stories 50-54 not counted in the totals). *Both accepted by the owner on 2026-10-09.*
- Epic: stories 1-46 and 55-57 delivered, except 33 (MCP write tools) and 39 (TLS for S3), deferred behind #105/#104. Stories 47-49 (read cache phase 2/3) are gated, and their gates were not met (#233 closed; ADR 0008 says the walks, not decode, dominate). Stories 50-54 wait on ADR 0009; story 50 groundwork shipped early in PR #248 (no export).
- Hot-term search after #246 (PR #261): warm p95 went from 212 ms to **93 ms** on 877M tokens; 32-thread p95 is 341 ms. About 12 ms/query is still spent on candidate file rows (#262).
- GPU spike: **no-go**. Trigger 2 (p95 > 100 ms at >= 1B tokens after #246) is **not met today** (93 ms at 877M), but there is little margin. Trigger 1 (semantic/embedding search in scope) is not in the epic. So #262, a CPU fix, is the right next step; GPU work stays parked.

## 2. Themes and triage of the 21 open issues

| Theme | Issues | Notes |
|---|---|---|
| A. Search UX / query semantics | #278 | User question: infix ("part of a word") matching. Today: exact `search`, trailing-`*` prefix and (since #275) case-insensitive `symbols`. Infix is not supported. Needs an answer plus a docs fix; the feature itself is a scope decision. |
| B. Store integrity / format | #276, #262 | #276: derived tables (`sym_fold`, `refs`) go stale after a binary rollback, so results are silently wrong. That is a correctness bug. #262: format change for read performance; needs an owner decision. |
| C. Extractor correctness | #253 (Rust shebang/raw strings), #252 (asm libunwind), #266 (TS decorators), #268 (COBOL `100A SECTION.`) | All are "symbols lost, tokens kept" bugs. Each is small and test-first. #253 hits real rustc files. |
| D. Ingest resource safety | #254 | Rust parse stack not counted in the memory budget (1.69 GB peak on adversarial input). |
| E. Raft / cluster robustness | #260, #115, #123, #226 | #260 follows the #232 fix pattern (in-flight tracking). #115: leader disk-full stops the node. #123: one bench run at 43% of embedded under load. #226: waiting on an upstream openraft release. |
| F. Observability (ADR 0009) | #249, #250 | Both depend on ADR 0009 being accepted. #250 also depends on an opentelemetry_sdk feature. |
| G. Security / scale (deferred by design) | #104 (TLS), #105 (auth), #108 (sharding) | Blocked on an external trigger (#104), or on owner scope (#105, #108). |

### Flags (recommendations only)

| Issue | Flag | Reason |
|---|---|---|
| #123 | **Stale / closable** | Not updated since 2026-09-29. A single run under concurrent build load; the soak and the at-scale benchmark were accepted in PR #124. Recommend closing it as "not reproduced", or re-running `measure_replication_at_scale` once on an idle host and closing it if the run is >= 50%. |
| #226 | **Parked (external)** | Cannot move until openraft ships a parking tick. Keep it open, maybe with a `blocked-upstream` label. |
| #278 | **Answerable now** | A question, not a defect. Answer it (infix: not supported; prefix `Gad*`; case-insensitive by default since #275). Then close it, or turn it into a feature story if the owner wants infix search. |
| #104, #108 | **Parked by design** | These are the revisit triggers of ADRs 0004/0006. Keep them open. |
| #249, #250 | **Blocked on ADR 0009** | No work until the ADR is accepted. |
| Duplicates | None found | #260 continues the closed #232. #262 continues the closed #246. #276 is new (found in the #275 QA review). None duplicates an open issue. |
| Already fixed | None of the open issues | Checked against #251, #258, #259, #261, #265, #267 and #275. Each fixed the parent issue; these are the explicit leftovers. |

## 3. Phase goal

**Make search results trustworthy and fast at the 1B-token scale: no silently stale or missing symbols, hot-term p95 well under 100 ms, and a clear answer on substring search.**

## 4. Prioritized backlog (RICE + MoSCoW)

RICE = Reach x Impact x Confidence / Effort. Reach is on a 1-10 scale (share of users or queries affected). Impact: 3 massive, 2 high, 1 medium, 0.5 low. Effort is in person-weeks. Sizes are story points (Fibonacci).

| Rank | Story | Issues | R | I | C | E | RICE | MoSCoW | Size |
|---|---|---|---|---|---|---|---|---|---|
| 1 | S1 Derived tables self-heal after a binary rollback | #276 | 6 | 3 | 0.9 | 1 | 16.2 | Must | 3 |
| 2 | S2 Answer and document substring matching | #278 | 8 | 1 | 1.0 | 0.25 | 32.0* | Must | 1 |
| 3 | S3 Rust extractor spans on shebang and raw-string files | #253 | 7 | 2 | 0.8 | 1 | 11.2 | Must | 3 |
| 4 | S4 Compact per-file sort key on the read path | #262 | 6 | 2 | 0.7 | 2 | 4.2 | Should (needs owner decision) | 5 |
| 5 | S5 Membership-change in-flight tracking | #260 | 3 | 2 | 0.8 | 1 | 4.8 | Should | 3 |
| 6 | S6 Rust parse stack in the memory budget | #254 | 4 | 1 | 0.8 | 1 | 3.2 | Should | 3 |
| 7 | S7 TypeScript decorated methods | #266 | 5 | 1 | 0.9 | 1 | 4.5 | Should | 2 |
| 8 | S8 asm libunwind spans | #252 | 2 | 1 | 0.8 | 0.5 | 3.2 | Could | 2 |
| 9 | S9 COBOL digit-led SECTION names | #268 | 1 | 1 | 0.9 | 0.25 | 3.6 | Could | 1 |
| 10 | S10 Leader disk-full answers RESOURCE_EXHAUSTED | #115 | 2 | 2 | 0.6 | 2 | 1.2 | Could | 5 |
| 11 | S11 Infix (substring) symbol search | #278 (if the owner opts in) | 7 | 2 | 0.5 | 3 | 2.3 | Could (needs owner decision) | 8 |
| - | OTel export stories 51-54 + #249, #250 | #249, #250 | - | - | - | - | - | Won't (this phase), unless ADR 0009 is accepted | - |
| - | TLS, auth, sharding, openraft tick, read cache 47-49 | #104, #105, #108, #226 | - | - | - | - | - | Won't (this phase) | - |
| - | GPU acceleration | spike | - | - | - | - | - | Won't (no-go; triggers not met) | - |

\* S2 tops the raw RICE score because its effort is tiny. It is ranked second because S1 is a silent-wrong-results bug. The order mixes RICE with risk.

S5 outscores S3 to S7 on raw RICE, but it ranks below them: its reach is limited to cluster users doing membership changes.

### Stories

**S1. Derived tables self-heal after a binary rollback** (#276). Size 3. Deps: none.
As an operator who rolls back to an older binary and forward again, I want derived indexes to be rebuilt when an older writer touched the file, so that symbol lookup never returns stale results.
- The store must detect a write by a binary that does not maintain `sym_fold`/`refs` (for example, a writer-generation stamp that older binaries do not bump, or a check that rebuilds on mismatch). It must then rebuild the derived table on the next open.
- A conformance case must simulate an "old writer" write after a new-binary write, and `symbols` must then match a fresh index (`run_differential`).
- A store with no rollback must not be rebuilt on open; a test asserts no rebuild.
- If the detection needs a stamp change, the on-disk version rules in CLAUDE.md must be followed (golden-byte test updated).

**S2. Answer and document substring matching** (#278). Size 1. Deps: none.
As a user searching for part of a name, I want the docs to tell me exactly what `search` and `symbols` match, so that I do not mistake "no results" for "not indexed".
- The README/guide must state: exact-token `search`; `symbols` with a trailing-`*` prefix; case-insensitive by default with `--exact-case` (#275); no infix match.
- The docs must give a worked example (`symbols Gad*` finds `Gadget`; `symbols Get` does not).
- The issue must get an answer. The owner decides whether S11 is opened.

**S3. Rust extractor: correct spans on shebang and raw-string files** (#253). Size 3. Deps: none.
As a developer indexing the Rust toolchain, I want symbols for every valid Rust file, so that `symbols` finds definitions in files that start with `#!` or contain `r#"..."#`.
- The 6 rustc files named in #253 must index with symbols and no span warning.
- Property/unit tests must cover a shebang first line, `#![attr]` (not a shebang), and raw strings with 0-3 hashes. The exact-span property must hold.
- The corpus test and size gate must stay green.

**S4. Compact per-file sort key on the read path** (#262). Size 5. Deps: owner decision D2. Optional: S1 (same derived-version machinery).
As a user of a large (~1B-token) index, I want hot-term queries to answer fast, so that common words like `return` do not stall an agent.
- On the 877M-token bench (`readbench.rs`), warm p95 for hot terms must drop below 60 ms (from 93 ms), and ctx ms/q must drop by at least 50%.
- `--json` output must be byte-identical to `main` on the corpus differential (the same 924 searches as PR #261).
- The format change must follow the versioning rule (upgrade on open via `derived_version` preferred; golden bytes). The size gate must stay <= 15x / <= 105 B/token.
- The result must be recorded against GPU trigger 2 in `docs/spikes/gpu-acceleration.md`.

**S5. Membership-change paths keep in-flight tracking** (#260). Size 3. Deps: none.
As an operator transferring leadership during a membership change, I want the drain to wait for every appended entry, so that the target never has a shorter log.
- `add_learner` and `change_membership` must hold the in-flight count until openraft resolves the entry, as `propose` does since #259.
- A failpoint test must cancel the caller mid-change and assert that the drain waits.
- The 1-core starvation flakes in #260 must pass 50 consecutive runs under `taskset`/1-CPU in CI or locally (documented).

**S6. Rust parse stack counts in the ingest memory budget** (#254). Size 3. Deps: none.
As an operator indexing untrusted repos, I want the parse stack counted against `--memory`, so that parallel adversarial files cannot exceed the budget.
- The admission must reserve the projected touched stack per parse job; two near-cap adversarial files must stay within the budget (test).
- The in-place 256 KiB stack requirement must be documented.

**S7. TypeScript decorated methods are found** (#266). Size 2. Deps: none.
As a TypeScript developer, I want decorated methods reported after a field without a semicolon and after a method body.
- Both repros in #266 must report `m` and `n` as methods with exact spans; the fuzz no-overlap test must stay green.

**S8. asm libunwind spans** (#252). Size 2. As a C/asm user, I want `UnwindRegistersSave.S` indexed with symbols. The file must index without a span warning, and a reduced fixture must be added.

**S9. COBOL `100A SECTION.`** (#268). Size 1. As a COBOL user, I want digit-led section names recognised. `100A SECTION.` and `100A SECTION .` must emit a section symbol; the Area A rule must be kept.

**S10. Leader disk-full answers RESOURCE_EXHAUSTED** (#115). Size 5. As an operator, I want a full disk on the leader to refuse writes without stopping the node. An apply-time I/O error from disk full must surface as RESOURCE_EXHAUSTED; the node must stay up and resume once space is freed (ClusterTestbed fault test).

**S11. Infix symbol search** (#278 follow-on). Size 8. Deps: owner decision D3; probably an ADR (index format, e.g. n-gram or suffix side table). As a user, I want `symbols *get*` to match `widgetGetter`. Only if the owner puts it in scope.

## 5. Sequencing

- **Wave 1 (correctness and quick wins):** S1, S2, S3, S7, S9. These are independent and can run in parallel worktrees. S1 is first because it is a silent-wrong-results bug.
- **Wave 2 (performance and resource safety):** S4 (after D2), S6, S8. S4 gets the bench host; S6 and S8 are independent.
- **Wave 3 (cluster hardening):** S5, S10, plus a re-run of the #123 benchmark to close it. S11 joins here only if D3 says yes (an ADR comes first).

**Waves as adopted (2026-10-09),** replacing the three waves above:
- **W1:** stories 58, 59, 60, 64 and 66, plus drafting ADRs 0011 (infix search) and 0012 (authentication).
- **W2:** story 61 after 58, then 63 and 65; OpenTelemetry stories 51-54 (with #249 and #250 folded in) in parallel.
- **W3:** stories 62 and 67.
- **W4:** story 68 after ADR 0011 and story 61; story 69 after ADR 0012; then story 33 (MCP write tools).

**S12. Authentication for `serve`** (#105, epic story 69). Size 8. Added 2026-10-09 after decision D7. Gated on ADR 0012. Open question for the ADR: token auth through a tonic interceptor with `subtle`, and no TLS in v1, because rustls's default providers (aws-lc-rs, ring) are deny-listed. Unblocks story 33.

S11 (epic story 68) is gated on ADR 0011. Open question for the ADR: symbol names only, scanning the interned name dictionary first and adding a trigram side table over distinct names only if p95 is above about 50 ms; a token-level n-gram index would break the 105 B/token size gate.

## 6. Trade-offs and deferrals

*Note, 2026-10-09:* the OTel and auth deferrals below no longer hold. ADR 0009 is accepted (stories 51-54 run in W2), and auth is scheduled as story 69 (W4).


- **Correctness over throughput.** S1 and the extractor fixes come before S4, because a silently wrong answer costs more trust than a slower one.
- **S4 over GPU.** The CPU fix attacks the measured 12 ms/q directly and keeps the pure-Rust gate. GPU stays no-go: trigger 2 is not met (93 ms < 100 ms at 877M), and trigger 1 (embeddings) is not in scope. If S4 lands and a >= 1B-token index still shows p95 > 100 ms, revisit the spike.
- **OTel (stories 51-54, #249, #250) is deferred.** It is blocked on accepting ADR 0009. It is worth doing, but it is not on the phase goal.
- **Read cache phase 2/3 (47-49) is deferred.** Its gates were not met, and the spike showed that decode is not the cost.
- **TLS/auth/sharding (#104, #105, #108) and the openraft tick (#226) are deferred.** They wait on external triggers or owner scope. #105 also blocks story 33 (MCP write tools).

## 7. Decisions for the owner

| # | Decision | Outcome (2026-10-09) |
|---|---|---|
| D1 | Accept ADR 0009 (OpenTelemetry), or keep it Proposed. This unblocks stories 51-54 and #249/#250. | **Accepted by the owner.** #249 and #250 are folded into stories 51 and 52. |
| D2 | Approve the on-disk format change for #262 (S4): `derived_version` side table vs a `V2_SCHEMA_VERSION` bump. | **Approved by the owner** (story 61; `derived_version` preferred). |
| D3 | Is infix/substring symbol search (S11) in scope? It would be an epic amendment and probably a new ADR. | **In scope** (story 68), gated on ADR 0011. |
| D4 | Add stories S1-S10 to the epic as a new section, or track them as issues only. | **Delegated to team consensus.** The architect, developer and QA all voted to add them, with S11 and S12, as stories 58-69. |
| D5 | ADR 0003 has been Proposed for a long time while v2 is the only format. Accept it, or mark it superseded. | **Accepted by the owner.** |
| D6 | Close #123 as stale (optionally after one idle re-run). | Open (owner preference). |
| D7 | Should auth (#105) be scheduled? It unblocks MCP write tools (story 33) and is needed before any shared deployment. | **Scheduled** (story 69), gated on ADR 0012. |

## 8. Assumptions and unverified items

- RICE inputs are PO estimates; the repo has no usage data.
- The post-#261 p95 (93 ms) is from the PR #261 body and was not re-measured. The >= 1B-token condition of the GPU trigger has not been tested.
- The 6 rustc files (#253) and the "older binary" rollback scenario (#276) were not reproduced.
- The main checkout's working tree is behind `origin/main` (its `docs/adr` stops at 0007), so all doc facts were read from `origin/main` and from the `spike/gpu-acceleration` branch.
