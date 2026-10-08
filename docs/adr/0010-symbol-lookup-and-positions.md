# ADR 0010: Enum members, case-insensitive symbol lookup and declaration position

**Status:** Proposed on 2026-10-08. The owner decides. The owner made the three scope decisions in D1 on 2026-10-08, in answer to issue [#269](https://github.com/P47Phoenix/memory-graph/issues/269). Builds on [ADR 0003](0003-data-model.md) (the v2 store, `sym_idx`, the `derived_version` self-heal) and [ADR 0002](0002-parsing-and-crate-layout.md) (token-stream extractors). Epic amendment, proposed: stories [55](../epic-code-memory-graph.md#story-55), [56](../epic-code-memory-graph.md#story-56) and [57](../epic-code-memory-graph.md#story-57). They are not counted in the epic totals until this ADR is accepted.

## In plain words

1. Issue #269 reports three things: C# enum members are not symbols, `symbols widget` does not find `Widget`, and a class with an attribute on the line above is reported at the attribute's line.
2. **Enum members** become `Constant` symbols, nested under their enum, in C#, Java, TypeScript, C/C++ and Rust. Scala already does this.
3. **Declaration position.** A symbol's span stays as it is (it still includes its attributes). Each symbol hit also reports where its *name* is, worked out at query time from the tokens already stored. The CLI shows that line. Nothing on disk changes.
4. **Case-insensitive lookup** becomes the default, with ASCII folding only. A new folded index sits beside the existing one and is rebuilt automatically on open for existing databases. `--exact-case` (and `exact_case` in the API, MCP and gRPC) keeps today's behaviour.
5. Items 2 and 3 only add data and fields. Item 4 changes what existing queries return, so it merges only after the owner accepts this ADR.

## Context

References are to `origin/main` on 2026-10-08.

- **Enum members are skipped on purpose.** The C# extractor skips enum bodies (`crates/graph-lang-csharp/src/lib.rs` ~767-771). Java, TypeScript, C/C++ and Rust do the same. Only Scala indexes members, as `Constant` with lang_kind `"case"` (`crates/graph-lang-scala/src/lib.rs` ~343-362). ASPX reuses the C# `member_symbols`.
- **Case-sensitivity was never decided.** `sym_idx` (the `SYMBOLS` multimap, `crates/graph-store/src/v2.rs` ~587) is keyed by the exact name text. It is written at ~4018 and removed at ~2333. An exact lookup is a `get`; a prefix lookup (`name*`) is a byte-order `range` scan (~1627-1673). `SymbolQuery` (`graph-store/src/lib.rs` ~289; `common.proto` ~136) has no case field. The language and kind filters are already ASCII-case-insensitive.
- **Spans include attributes on purpose** in C#, Java and Python; TypeScript leaves decorators out. A symbol has exactly one `span` (`graph-core/src/extractor.rs` ~6-10, `schema.rs` ~135), and no name or declaration position.
- **Self-heal precedent.** `refs`/`content_files` carry a `derived_version` in `meta` (`DERIVED_VERSION_REFS_KEY`, `v2.rs` ~100). `V2Store::open` rebuilds them when the stamp is missing or stale (~2840-2860), silently and without refusing the open. A file already current is not written, so a plain reopen stays byte-identical.

## Decision

### D1. Owner decisions (2026-10-08)

1. **Index enum members in every language** that has them, consistent with Scala.
2. **Case-insensitive lookup by default,** with an exact-case opt-out.
3. **Keep spans as they are,** and add a declaration (name) position that results report.

### D2. Enum members

- **Rule:** each enum member is a symbol with generic kind `Constant`, whose parent is its enum's scope. Its span covers the member declaration: `Red`, `Red = 1`, or a variant with its payload. Attributes on a member are inside its span, as for any other C#/Java symbol.

| Language | lang_kind | Notes |
|---|---|---|
| C# | `enum_member` | Comma lists, initializers, attributes on members; ASPX gets it through `member_symbols` |
| Java | `enum_constant` | Constants before the first top-level `;`. The bodies of constant-specific classes are not symbols |
| TypeScript | `enum_member` | Includes `const enum`; string and computed initializers |
| C / C++ | `enumerator` | Includes `enum class` and C++11 typed enums (`enum E : int`) |
| Rust (`syn`) | `variant` | Through `visit_variant`; unit, tuple and struct variants |
| Scala | `case` | Unchanged (the precedent) |

- **Out of scope:** Go (constants with `iota` are ordinary `const` declarations, already indexed as such), F# and Haskell (discriminated unions and data constructors are not enums in the same sense). Other languages can follow under this rule without a new ADR.
- **Nesting:** a member's enclosing type for the `class` grain is its enum. An enum nested in a class nests its members one level deeper; nothing else moves.
- **Versioning:** each changed extractor bumps its version (pinned in `crates/graph-cli/tests/extractor_versions.rs`), so affected files re-index on the next run. The corpus differential must show only new `Constant` members, and the corpus hash in `non_aspx_corpus_symbols_are_unchanged` is re-pinned.

### D3. Declaration (name) position, computed at query time

- **Definition:** for a symbol hit, the declaration position is the first identifier token inside the symbol's span whose text equals the symbol's name. Tokens inside attribute or decorator groups are skipped where the extractor's tokens mark them (for example a bracketed group before the declaration in C#, or `@Name(...)` in Java). If none matches, it is the span start.
- **Why query time:** the tokens are already in the file's stream, so it needs no extractor change, no stored field and no format change. It is language-agnostic: the walk compares text, nothing else.
- **Surfaces (all additive):**
  - `Hit` / symbol results in `graph-store` gain the name position (line, column, byte offset);
  - the proto hit message gains an optional `name_pos` (regenerated with the xtask; old clients ignore it, old servers leave it unset and clients fall back to the span start);
  - `graph-client` conversions and MCP output carry it;
  - the CLI text output of `symbols` and `search` prints `path:line:col` of the declaration; `--json` keeps the full span unchanged and adds `name_line` and `name_col`.
- **Spans are unchanged**, so `--grain` roll-ups, `show` and every span test behave as today.
- **Edge case:** when the name also appears inside an attribute argument that the extractor does not mark (`[Alias("Shape")] class Shape` has a string, not an identifier, so it is fine; `[Foo(Shape)] class Shape` is the rare bad case), the reported line may be the attribute's. That is no worse than today, and it is documented rather than handled with language knowledge.
- **Cost:** one bounded token walk per returned hit, over the symbol's leading tokens only, using the checkpointed stream reader. It is measured with readbench (`crates/graph-cli/tests/readbench.rs`) and reported in story 56's PR.

### D4. Case-insensitive lookup by default

- **Folding: ASCII only.** Names are folded with ASCII lowercasing (`A-Z` to `a-z`); every other byte is unchanged, so non-ASCII letters match exactly. Reasons:
  - **determinism:** the result depends on no Unicode tables or library versions, across builds, platforms and cluster nodes;
  - **prefix-range soundness:** ASCII folding maps bytes one to one and keeps lengths, so a folded prefix is a byte prefix of every folded name it should match, and a `range` scan stays correct (full Unicode folding changes lengths, for example `ß` to `ss`);
  - **parity:** the language and kind filters already fold ASCII only.
- **Index:** a new multimap table `sym_fold`, keyed by the folded name, with the same values (sub-ids) as `sym_idx`. It is written and removed beside `sym_idx` on every write path: insert (~4018), remove (~2333), replace, prune, vacuum and rebuild-into-a-new-file. The conformance suite checks the two stay consistent.
- **Upgrade: a `derived_version` self-heal, not a schema bump.** `meta` gains a `derived_version_sym_fold` key. On open, a missing or stale stamp rebuilds `sym_fold` from `sym_idx` in one write transaction and stamps it, following the refs/content_files pattern. There is no `V2_SCHEMA_VERSION` bump and no `LegacyFormat`; a current file is not written on open.
  - **Read-only opens:** a path that cannot write (if one exists, or is added) must not rebuild. It either falls back to a full scan of `sym_idx` with folding for that session, or refuses case-insensitive queries with a clear error naming `--exact-case`. Story 57 picks one and tests it; it must never return silently wrong (empty) results.
  - **Raft followers and snapshots:** `sym_fold` is derived and local; it is never in the Raft log. A follower opens its store writable, so it heals on open like any node. A snapshot from an older leader has no `sym_fold`; installing it reopens the store, which heals it. Mixed-version clusters are therefore safe: older nodes simply answer case-sensitively until upgraded.
- **Query:** `SymbolQuery` gains `exact_case: bool`, default `false`.
  - Default: exact and prefix lookups fold the pattern and go through `sym_fold`. Results are deduplicated by sub-id. Ordering stays deterministic: the existing order, with ties broken by the name's bytes.
  - `exact_case = true`: today's `sym_idx` path, unchanged.
  - The literal `name\*` form is honoured in both modes.
- **Surfaces (additive):** CLI `memory-graph symbols --exact-case`; MCP `find_symbols` gains an `exact_case` parameter; gRPC `SymbolQuery.exact_case` (proto regen; an old server ignores it and answers case-sensitively); `graph-client`.
- **Obligations:**
  - conformance cases: `widget` finds `Widget`; `wid*` finds `Widget` and `widGet`; `exact_case` keeps the old answers; non-ASCII names match exactly; `sym_fold` agrees with `sym_idx` after re-index, replace, prune and vacuum;
  - `run_differential` and `run_crash_rerun_differential` cover the new path;
  - a self-heal test (a store written without `sym_fold` is reopened, rebuilt and queried);
  - the size gate (`crates/graph-cli/tests/size_gate.rs`) still passes, and the size cost is measured and recorded;
  - a remote conformance run and a cluster replication test.

### D5. Owner gate and order

- Stories 55 (enum members) and 56 (declaration position) only add symbols and fields; they may merge before this ADR is accepted.
- Story 57 changes default query semantics and adds an on-disk table, so it merges **only after the owner accepts this ADR**.
- Issue #269 closes when the C# part of 55, 56 and 57 have merged.

## Open questions (owner)

1. Accept this ADR (Proposed to Accepted).
2. For read-only opens of a store without `sym_fold` (D4): fall back to a folded full scan, or refuse case-insensitive queries with an error? The proposal leans to the fallback if it is rare and bounded.

## Alternatives considered

| Alternative | Why not |
|---|---|
| A full-scan fallback instead of a folded index (fold and compare every name in `sym_idx`) | No on-disk change, but every default lookup becomes O(distinct names) instead of a `get` or a short range scan. On large indexes that turns the most common query into the slowest. It survives only as a possible read-only fallback (D4). |
| Unicode full case folding | It changes lengths (`ß` to `ss`), breaks the byte-prefix property the range scan needs, depends on Unicode table versions (a cluster could disagree across builds), and no other filter does it. Non-ASCII identifiers still match exactly. |
| A schema field for the name span (stored by every extractor) | Exact and cheap to read, but it bumps the stored format, needs every extractor changed, and re-indexes every database, to get what the token stream already gives at query time. |
| Excluding attributes and decorators from spans | Fixes the reported line, but changes spans (and every span test, the corpus pins and `show` output), and loses the attribute from the symbol's text, which callers use. The owner chose to keep spans. |

## Consequences

- **Positive:**
  - Enum members are findable in six languages, matching Scala.
  - `symbols widget` finds `Widget`, which is what users and agents expect.
  - Results point at the declaration's line without losing the attribute from the span.
- **Negative:**
  - **Query semantics change for existing users:** default symbol lookups return case-insensitive matches, so scripts that relied on case-sensitive results may see more hits. `--exact-case` restores the old answers.
  - **Size:** `sym_fold` roughly duplicates `sym_idx`'s keys (less where names are already lowercase but still a second key per name). It must stay inside the size gate; the measured cost is recorded in story 57.
  - **Re-indexing:** extractor version bumps re-index affected files on the next run, and the corpus pins are re-pinned in each extractor PR.
  - The first open of an existing database after upgrading pays a one-time `sym_fold` rebuild.
  - The declaration position can be wrong in the rare unmarked-attribute case (D3).
- **Neutral:**
  - No `V2_SCHEMA_VERSION` bump, no `LegacyFormat`, no Raft log change and no `PROTOCOL_VERSION` bump. Proto changes are additive fields.
