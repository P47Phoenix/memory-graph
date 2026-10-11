## Epic: Language-Agnostic Code Memory Graph (pure Rust)

**Epic Goal:** An AI agent or developer stores many codebases, in any programming language, in one embedded graph database. The hierarchy is org → repo → file → symbol → syntax token. They can then search and traverse it across repos and languages. Built-in extractors add structure for some languages. Any other language still gets File and Token nodes through a generic tokenizer, and agents can supply structure for any language through a language-neutral ingest format.

**Success Metrics:**
- (a) A repo in a language with no built-in extractor is indexed to File and Token nodes without code changes.
- (b) At least 3 repos across at least 2 built-in languages plus 1 fallback language sit in one database, and a single query returns cross-repo, cross-language results filtered by language.
- (c) Adding a built-in language touches only extractor code, with 0 schema, storage or query changes.
- (d) `cargo tree` shows 0 `-sys` or C/C++ build dependencies, enforced in CI.
- (e) p95 search latency is under 100 ms on 1,000+ files. This target is a placeholder until story 18.
- (f) Re-indexing an unchanged repo re-parses 0 files.

**Out of Scope:** auth, TLS on the wire, semantic or embedding search, cross-file reference resolution (call graphs, type resolution), GUI, git history, a query language (fixed query functions only), and bundled extractors beyond Rust and Python in this epic. *Amended 2026-09-28 ([ADR 0004](adr/0004-client-server-and-replication.md), Accepted):* a network server (`memory-graph serve` over gRPC) and replicated storage (a Raft cluster with durable writes, reads on every node) are now in scope as stories 20-25; "distributed storage" in the sense of sharding a database across nodes stays out of scope (ADR 0003 Q4, build deferred). *Amended 2026-09-29 at the owner's request ([ADR 0005](adr/0005-mcp.md) and [ADR 0006](adr/0006-snapshots-object-storage.md), both Accepted by the owner on 2026-09-29):* a read-only MCP interface (stories 31-34) and snapshot backups to a directory or S3-compatible object storage over plain HTTP (stories 35-39) are now in scope; MCP write tools (33) and native HTTPS to object storage (39) are deferred.

### Story Map

| # | Story Title | Value | Effort | Priority | Dependencies |
|---|---|---|---|---|---|
| 1 | Spike: pure-Rust parsing (syn, ruff parser, logos) | Risk reduction | 3 | P1 | none |
| 2 | Spike: pure-Rust embedded storage (redb, fjall) | Risk reduction | 5 | P1 | none |
| 3 | CI gate banning C/-sys dependencies | Constraint | 2 | P1 | none |
| 4 | Language-agnostic graph schema and Extractor trait | Foundation | 5 | P1 | 1, 2 |
| 5 | Persist graph to disk and reopen | Core | 3 | P1 | 2, 4 |
| 6 | Generic fallback tokenizer (any language) | Core | 5 | P1 | 4 |
| 7 | Ingest one file into the graph (CLI and library) | Core | 5 | P1 | 5, 6 |
| 8 | Search tokens by text, with language filter | Core | 5 | P1 | 7 |
| 9 | Rust extractor (symbols and token linking) | High | 8 | P2 | 4, 7 |
| 10 | Ingest a directory as a repo (walk, ignore, language detection) | High | 5 | P2 | 7 |
| 11 | Symbol search by name, kind and language, scoped | High | 5 | P2 | 9, 10 |
| 12 | Hierarchy traversal queries | High | 5 | P2 | 9 |
| 13 | Language-neutral NDJSON ingest format | High | 5 | P2 | 4, 7 |
| 14 | Multi-org, multi-repo and cross-language queries | High | 3 | P2 | 10, 11 |
| 15 | Incremental re-index | High | 5 | P3 | 10 |
| 16 | Python extractor via the Extractor trait | High | 5 | P3 | 9 |
| 17 | Error tolerance and JSON output with stable API | Medium | 5 | P3 | 10, 8 |
| 18 | Remove data and benchmark at scale | Medium | 5 | P3 | 15 |
| 19 | Spike: WASM-hosted extractor plugins (wasmi) | Risk reduction | 3 | P4 | 4 |
| 20 | Single-node server and client (gRPC, `serve`, `--server`) | High | 8 | P2 | ADR 0004 (accepted 2026-09-28) |
| 21 | Replication: Raft log, snapshots, `--bootstrap` | High | 8 | P2 | 20 |
| 22 | Membership and write forwarding | High | 8 | P2 | 21 |
| 23 | Linearizable reads and crash tests | High | 5 | P3 | 22 |
| 24 | Observability and packaging (metrics, health, Compose, Kubernetes) | Medium | 5 | P3 | 22 |
| 25 | Cluster hardening (benchmarks, soak, `--update-advertise`) | Medium | 3 | P4 | 23, 24 |
| 26 | Tokenizer dialects and scan helpers for more languages | Foundation | 5 | P2 | 6 |
| 27 | TypeScript, Python and Java symbol extractors | High | 8 | P2 | 26, 16 |
| 28 | C, C++, Go and Scala symbol extractors | High | 8 | P2 | 26 |
| 29 | SQL, shell, R, F#, Haskell, Elixir and GDScript symbol extractors | High | 8 | P3 | 26 |
| 30 | COBOL, RPG and assembly symbol extractors | Medium | 8 | P3 | 26 |
| 31 | `graph-mcp` core and `memory-graph mcp` over stdio, read-only | High | 5 | P2 | 20 |
| 32 | Streamable HTTP MCP endpoint in `serve` with security guards | High | 5 | P2 | 31, 24 |
| 33 | Opt-in MCP write tools (**Deferred** until auth, #105; unblocked by story 69) | Medium | 3 | P4 | 32, 69 |
| 34 | MCP real-client e2e and hardening | Medium | 3 | P3 | 31, 32 |
| 35 | Snapshot backups: `file://` sink and verified restore | High | 5 | P2 | 22 |
| 36 | S3 client over plain HTTP (SigV4, multipart) | High | 8 | P2 | 35 |
| 37 | Restore from `s3://` and `latest`, `cluster snapshot --upload`, `cluster backups` | Medium | 3 | P2 | 36 |
| 38 | S3 e2e CI job and backup docs | Medium | 3 | P3 | 37 |
| 39 | Spike: native HTTPS for backups behind `backup-tls` (**Deferred**, shared with #104) | Low | 5 | P4 | 36, #104 |
| 40 | Decode any source encoding in `graph_core::encoding` (ADR 0007) | High | 5 | P2 | 6 |
| 41 | Store integration: decoded spans, binary rejection, schema 11 restamp, fingerprint rule | High | 8 | P2 | 40 |
| 42 | `--encoding`, `--strict-encoding` and `.memory-graph.toml` per-glob overrides | Medium | 3 | P2 | 41 |
| 43 | Encodings on the wire, in `describe`/`--stats` and in MCP | Medium | 5 | P3 | 41, 20, 31 |
| 44 | Encoding fixtures, cross-encoding search tests and docs | Medium | 3 | P3 | 42, 43 |
| 45 | Read-path measurement and benchmark (ADR 0008 phase 0, partly delivered; #233) | Risk reduction | 3 | P3 | 24 |
| 46 | Dict lookup without full decode; page-cache sizing (ADR 0008 phase 1, delivered) | Medium | 3 | P3 | 45 |
| 47 | Decoded-object cache core and MVCC-safe invalidation (ADR 0008 phase 2, gated) | High | 8 | P3 | 45 (gate), 46 |
| 48 | Read-cache tests, metrics and flags (ADR 0008 phase 2, gated) | High | 5 | P3 | 47 |
| 49 | Optional query-result cache (ADR 0008 phase 3, optional) | Low | 3 | P4 | 45, 48 |
| 50 | OpenTelemetry groundwork: dependencies, telemetry module, `MetricsSnapshot`, test seams (ADR 0009; groundwork in #248) | Medium | 8 | P3 | 24 |
| 51 | Traces and W3C propagation across client, forward and leader apply (ADR 0009) | High | 8 | P3 | 50 |
| 52 | Metrics over OTLP with the name mapping (ADR 0009) | Medium | 5 | P3 | 50 |
| 53 | Logs over OTLP; trace ids in JSON logs when traces are on (ADR 0009) | Medium | 3 | P3 | 51 |
| 54 | OpenTelemetry compose demo, CI, overhead numbers and docs (ADR 0009) | Low | 3 | P3 | 52, 53 |
| 55 | Enum members as `Constant` symbols in C#, Java, TypeScript, C/C++ and Rust (ADR 0010, delivered; #271-#273) | High | 5 | P2 | 27, 28 |
| 56 | Declaration (name) position on symbol hits, computed at query time (ADR 0010, delivered; #274) | High | 3 | P2 | 16 |
| 57 | Case-insensitive symbol lookup by default: `sym_fold` index, self-heal, `--exact-case` (ADR 0010, delivered; #275) | High | 5 | P2 | 16 |
| 58 | Derived tables self-heal after an older binary writes to the file (#276; delivered in #286) | High | 3 | P2 | 57 |
| 59 | Document today's substring matching and answer #278 (delivered in #282) | Medium | 1 | P2 | none |
| 60 | Rust extractor: exact spans on shebang and raw-string files (#253; delivered in #287) | High | 3 | P2 | 9 |
| 61 | Compact per-file sort key on the read path (#262; format change, approved 2026-10-09) | High | 5 | P2 | 58 (preferably) |
| 62 | Membership changes keep in-flight tracking until Raft answers (#260) | Medium | 3 | P3 | 22 |
| 63 | Rust parse stack counted in the ingest memory budget (#254) | Medium | 3 | P3 | 60 (preferably) |
| 64 | TypeScript decorated methods are found (#266; delivered in #283) | Medium | 2 | P2 | 27 |
| 65 | asm extractor: exact spans on libunwind `UnwindRegistersSave.S` (#252) | Low | 2 | P3 | 30 |
| 66 | COBOL digit-led `SECTION` names such as `100A SECTION.` (#268; delivered in #284) | Low | 1 | P3 | 30 |
| 67 | Leader or apply-time disk full answers RESOURCE_EXHAUSTED without stopping the node (#115) | Medium | 5 | P3 | 22 |
| 68 | Infix (substring) symbol search (#278; [ADR 0011](adr/0011-infix-search.md), Accepted 2026-10-10) | High | 5 | P3 | 61 (preferably) |
| 69 | Authentication for `serve` (#105; [ADR 0012](adr/0012-authentication.md), Accepted 2026-10-10) | High | 8 | P3 | 20 |

Total: 69 stories, 328 pts (average about 4.8); 320 pts excluding the deferred stories 33 and 39. (Correction, 2026-10-09: the previous totals line said 262 pts, but its table summed to 260; the totals now follow the table.) Stories 20-25 (37 pts) were added on 2026-09-28 by [ADR 0004](adr/0004-client-server-and-replication.md), accepted by the user the same day. Stories 26-30 (37 pts) were added on 2026-09-29 at the user's request: symbols for 17 more languages. Stories 31-39 (40 pts; 33 and 39 deferred) were added on 2026-09-29 at the owner's request by [ADR 0005](adr/0005-mcp.md) (MCP) and [ADR 0006](adr/0006-snapshots-object-storage.md) (snapshots to object storage), both Accepted by the owner on 2026-09-29. Stories 40-44 (24 pts) were added on 2026-09-30 at the owner's request by [ADR 0007](adr/0007-source-encodings.md) (indexing files in any source encoding), Accepted by the owner on 2026-09-30. Stories 45-49 (22 pts) were added on 2026-10-05 at the owner's request by [ADR 0008](adr/0008-read-cache.md) (read cache pool), Accepted by the owner on 2026-10-05. Of these, 47-48 are gated on the decode-share measurement, and 49 is optional. Stories 50-54 (27 pts) were proposed on 2026-10-06 by [ADR 0009](adr/0009-opentelemetry.md) (OpenTelemetry), Accepted by the owner on 2026-10-09, and are counted since then. Stories 55-57 (13 pts) were added on 2026-10-08 at the owner's request by [ADR 0010](adr/0010-symbol-lookup-and-positions.md) (enum members, case-insensitive symbol lookup, declaration position; issue #269), Accepted by the owner on 2026-10-09. Stories 58-69 (41 pts) were added on 2026-10-09 from the backlog triage in [plan-next-phase.md](plan-next-phase.md): the owner approved the #262 format change (story 61), put infix search (#278, story 68) in scope and asked for auth (#105, story 69) to be scheduled, and delegated adding the stories to team consensus (architect, developer and QA all voted yes). Stories 68 and 69 each needed an ADR before any code: [ADR 0011](adr/0011-infix-search.md) and [ADR 0012](adr/0012-authentication.md), both Accepted by the owner on 2026-10-10.

**Next phase (2026-10-09).** Waves, from [plan-next-phase.md](plan-next-phase.md):
- **W1:** stories 58, 59, 60, 64 and 66, plus drafting ADRs 0011 (infix search) and 0012 (authentication). Delivered: #282-#287; ADRs 0011 and 0012 accepted on 2026-10-10.
- **W2:** story 61 after 58, then 63 and 65; OpenTelemetry stories 51-54 in parallel.
- **W3:** stories 62 and 67.
- **W4:** story 68 after ADR 0011 and story 61; story 69 after ADR 0012; then story 33.

### MVP Slice
Stories 1–8 (33 pts). Any file in any language goes into a persisted graph as File and Token nodes under org/repo, and is searchable by token text with a language filter, through the library and the CLI. The C-dependency gate is active from the start.

Rationale: this proves the "any language" claim on day one, since the fallback tokenizer needs no language knowledge. It also retires the parser and storage unknowns first and locks the pure-Rust rule in CI before dependencies pile up.

Later slices:
- **Structure:** stories 9–12
- **Agent integration:** stories 13 and 14
- **Operational and reach:** stories 15–19
- **Client/server and cluster (ADR 0004):** stories 20–25

### Full Story Definitions

**1. Spike: pure-Rust parsing (3 pts)**
As a solo developer building the indexer
I want to evaluate `syn`/`proc-macro2`, `ruff_python_parser` and `logos` for extracting tokens, spans and symbols
So that we can estimate the extractor stories with confidence.
Time-boxed to 1 day. Output: documented findings + story estimate.
- Given a sample Rust file and a sample Python file, When each candidate runs, Then the findings record whether byte, line and column spans are available for every token.
- Given a file with a syntax error, When each candidate runs, Then the findings record the error-recovery behavior, including whether tokens are still produced.
- Given the candidates, When compared, Then the findings confirm each is pure Rust (no `-sys` in `cargo tree`) and record its license and compile time.
- Given the recommendation, When the spike closes, Then the estimates for stories 6, 9 and 16 are confirmed or revised.

**2. Spike: pure-Rust embedded storage (5 pts)**
As a solo developer building the indexer
I want to compare `redb`, `fjall` and other pure-Rust stores for holding a graph
So that we can commit to a persistence design.
Time-boxed to 2 days. Output: documented findings + story estimate.
- Given 100k synthetic token nodes with containment edges, When each option is loaded and queried by token text and by parent-children, Then the findings record write time, query latency and on-disk size.
- Given each option, When assessed, Then the findings record whether it is pure Rust, in-process, transactional, and its maintenance status and license.
- Given the recommendation, When the spike closes, Then the estimates for stories 4, 5 and 18 are confirmed or revised.

**3. CI gate banning C/-sys dependencies (2 pts)**
As a project maintainer
I want CI to fail when a C or C++ dependency enters the build
So that the pure-Rust rule holds as dependencies change.
- Given a clean workspace, When CI runs the gate, Then it must pass and print the checked dependency count.
- Given a dependency that links a native library (a `links` key, typically a `-sys` crate) or has a `build.rs` using a C/C++ build tool (`cc`, `cmake`, `bindgen`, ...); pure-Rust `-sys` crates such as `windows-sys` and `linux-raw-sys` are allowed, When CI runs the gate, Then it must fail and name the offending crate.
- Given an approved exception file (initially empty), When an exception is listed, Then the gate must pass for that crate only and print the exception.
- Given the gate, When run locally with one documented command, Then it must produce the same result as CI.

**4. Language-agnostic graph schema and Extractor trait (5 pts)**
As an AI-agent integrator
I want one documented node and edge model with a language tag and a generic kind vocabulary
So that code in any language is stored and queried the same way.
- Given the schema, When a File node is created, Then it must carry a language string (e.g. `rust`, `python`, or `unknown`) and no language-specific type.
- Given the schema, When a Symbol node is created, Then it must carry a generic kind from a documented vocabulary (`module`, `type`, `function`, `method`, `variable`, `constant`, `other`), an optional language-specific kind string (e.g. `impl`, `trait`), a name and a span.
- Given the schema, When a Token node is created, Then it must carry text, token class (`identifier`, `keyword`, `literal`, `operator`, `punctuation`, `comment`, `other`), byte range, and start and end line and column.
- Given a parent and child, When a CONTAINS edge is created, Then only the valid hierarchy Org > Repo > File > Symbol* > Token must be accepted and other pairings must be rejected.
- Given the Extractor trait, When a language implements it, Then it must take file bytes and return symbols and tokens using only the schema types, with no dependency on storage or query code.
- Given every node and edge type, When unit tests run, Then round-trip serialization must pass.
- Given any node, When its parent is requested, Then the store returns it in one lookup (parent pointer per node), so results can roll up to symbol, file, repo or org. The full traversal API stays in story 12.

**5. Persist graph to disk and reopen (3 pts)**
As a developer or agent
I want the graph stored in a database file I choose
So that I index once and query in later sessions.
- Given a graph written with `--db ./g`, When the process exits and `./g` (a single file) is reopened, Then all nodes and edges must be present and unchanged.
- Given a database from an incompatible schema version, When opened, Then the tool must report the mismatch and must not modify the data.
- Given a second process opens a database that is already locked, When it starts, Then it must fail with a clear message and must not corrupt data.

**6. Generic fallback tokenizer (5 pts)**
As an AI-agent integrator
I want any text source file tokenized without a language-specific extractor
So that every language is storable and searchable, even without symbols.
- Given a file in a language with no extractor (e.g. Zig), When indexed, Then the file must get language `unknown` (or the detected name) and Token nodes for identifiers, numbers, strings, comments and punctuation.
- Given that file, When indexed, Then no Symbol nodes must be created, and Tokens must be contained directly by the File.
- Given any token, When stored, Then its text, byte range, line and column must match the source exactly.
- Given a file that is not valid UTF-8, When indexed, Then it must be rejected with a reason and not stored partially.
- Given an unterminated string or comment, When tokenized, Then the tokenizer must complete without panicking and must emit the remainder as a token.

**7. Ingest one file (CLI and library) (5 pts)**
As an AI-agent integrator or developer
I want to index a single file under an org and repo
So that its tokens land in the correct place in the hierarchy.
- Given `index-file --org O --repo R <path>`, When run, Then Org, Repo and File nodes must exist and every token must be contained by the File, directly or via symbols.
- Given a file, When indexed, Then the registered extractor for its language must be used, and the fallback tokenizer must be used when none is registered.
- Given the same file indexed twice, When the second run completes, Then no duplicate nodes exist.
- Given a nonexistent path, When run, Then it must exit non-zero with a message naming the path.
- Given the library API, When called with bytes, org, repo, path and an optional language override, Then it must produce the same graph as the CLI.

**8. Search tokens by text, with language filter (5 pts)**
As a developer or agent
I want to find every token matching given text across everything indexed
So that I can locate identifiers and literals in any language.
- Given indexed files, When searching `foo` exactly, Then every Token with text `foo` must be returned with org, repo, file path, language, line and column.
- Given `--language rust`, When searched, Then only tokens from files tagged `rust` must be returned.
- Given `--kind identifier`, When searched, Then only tokens of that class must be returned.
- Given no matches, When searched, Then the result must be empty and the exit code 0.
- Given results, When printed, Then they must be ordered by org, repo, path and byte offset.
- Given `--json`, When searched, Then stdout must be one JSON document `{"query":…,"grain":…,"results":[…]}` and nothing else, so an agent can consume it.
- Given `--grain token|symbol|method|class|file|repo|org` (default `token`), When searched, Then results are the distinct nodes of that grain containing a match, each with its containment path and a hit count. `symbol` is the nearest enclosing symbol of any kind, `method` the nearest enclosing method or free function, `class` the nearest enclosing type or a Rust `impl` block (amended 2026-09-27, PR #99); these rows carry the definition's full span (start and end byte, line and column). `--symbol-kind` restricts a symbol, method or class grain to that kind, and a generic kind the grain can never hold is refused.
- Given a match in a file with no enclosing symbol satisfying the requested grain and kind, When searched at symbol, method or class grain, Then it rolls up to its File (the row keeps the requested grain, with no symbol) and is flagged `no_matching_symbol` (`no_symbols` when the file has no symbols at all).

**9. Rust extractor (8 pts)**
As an AI-agent integrator
I want Rust structs, enums, traits, impls, functions, methods and variables stored as Symbols with their tokens
So that Rust code is navigable by structure.
- Given a struct with an impl containing two methods, When indexed, Then Symbols with generic kinds `type`, `other` (kind string `impl`) and `method` must exist, with the methods contained by the impl.
- Given a function with `let` bindings, When indexed, Then each binding must be a `variable` Symbol contained by the function and its tokens must be contained by that Symbol.
- Given any token, When its ancestors are queried, Then the chain must end at the File and include the innermost symbol.
- Given a token outside any symbol (e.g. a `use` line), When indexed, Then it must be contained directly by the File.
- Given a Rust file with a syntax error, When indexed, Then the extractor must fall back to the generic tokenizer for that file, and must flag the File `has_errors`.
- Given the extractor, When compiled, Then it must not add any schema, storage or query changes.

**10. Ingest a directory as a repo (5 pts)**
As a developer or agent
I want to index a directory as a repo under an org
So that a codebase is searchable in one step.
- Given `index --org O --repo R <dir>`, When run, Then every text file must be ingested using its detected language, and the summary must list counts per language.
- Given a `.gitignore`, When indexing, Then ignored paths must not be indexed.
- Given a binary file, When encountered, Then it must be skipped and counted with a reason.
- Given a file extension with no known language, When indexed, Then it must go through the fallback tokenizer and not be skipped.
- Given completion, When the summary prints, Then it must show files, symbols, tokens, skipped files and elapsed time.
- Given `--prune`, When the run completes cleanly, Then files of that repo that a previous directory run indexed but this run did not must be removed and listed; files added with `index-file` must be kept; and a run that indexed nothing must be refused unless `--force`.

**11. Symbol search by name, kind and language (5 pts)**
As an AI-agent integrator
I want to find symbols by name, generic kind and language within an org, repo or file
So that my agent retrieves definitions without scanning files.
- Given symbols named `parse` of kinds `function` and `method`, When searching name=`parse` kind=`method`, Then only methods must be returned.
- Given `--language python`, When searched, Then only Symbols in Python files must be returned.
- Given two repos containing `parse`, When scoped to one repo, Then only that repo's matches must be returned.
- Given a prefix pattern `pars*`, When searched, Then all names with that prefix must be returned.
- Given results, When returned, Then each must include its containment path and its language-specific kind string.

**12. Hierarchy traversal queries (5 pts)**
As an AI-agent integrator
I want children, descendants and ancestors of any node
So that I can ask "all methods in type X".
- Given a type with three methods, When I query children filtered to kind=`method`, Then exactly those three must be returned.
- Given a node and depth N, When I query descendants, Then results must be limited to N levels.
- Given a Token, When I query ancestors, Then the ordered chain up to Org must be returned.
- Given a nonexistent node id, When queried, Then a not-found error must be returned, not an empty success.
- Given a File in the fallback language, When its descendants are queried, Then only Tokens must be returned, without error.

**13. Language-neutral NDJSON ingest format (5 pts)**
As an AI-agent integrator or external tool author
I want to supply symbols, tokens and spans for any language as NDJSON
So that an agent can store structure for a language with no built-in extractor.
- Given a documented, versioned NDJSON schema, When a file record with language, path and a list of symbol and token records is submitted, Then the graph must store them exactly as given.
- Given a record whose parent reference does not exist, When ingested, Then the whole file must be rejected with the line number and reason, and nothing from that file is stored.
- Given a record with a span outside the file's byte length, When ingested, Then it must be rejected with the line number.
- Given records for a language string the database has not seen before, When ingested, Then they must be stored and be searchable by that language.
- Given NDJSON ingest of a file that already exists, When ingested, Then the old subtree must be replaced.
- Given the format, When published, Then a JSON Schema document and an example must be included in the repo docs.

**14. Multi-org, multi-repo and cross-language queries (3 pts)**
As an AI-agent integrator
I want many orgs and repos in one database, queried together or scoped
So that my agent analyzes several codebases at once.
- Given 3 repos across 2 orgs in 3 languages, When I search a token without scope, Then results must come from all repos and carry org, repo and language.
- Given `--org` or `--repo` scope flags, When searched, Then results must be limited accordingly.
- Given `list-repos`, When run, Then it must print each org, repo, file count and per-language counts.
- Given two repos with the same relative file path, When indexed, Then they must remain separate File nodes.

**15. Incremental re-index (5 pts)**
As a developer or agent
I want re-indexing to process only changed files
So that keeping the graph current is fast.
- Given an unchanged repo, When `index` runs again, Then 0 files must be re-parsed and the summary must say so.
- Given one modified file, When re-indexed, Then only that file's symbols and tokens must be replaced and no stale nodes remain.
- Given a file deleted from disk, When re-indexed, Then its File node and all descendants must be removed.
- Given a file whose language extractor version changed, When re-indexed, Then that file must be re-parsed.

*Status: the skip-unchanged part is implemented.* File nodes carry a fingerprint (SHA-256 of the content + lowercased language + `Extractor::version()` (includes the tokenizer version) + store index format version); an identical fingerprint skips the file and is reported as `unchanged` (`index`, `index-file`, `--json`, `IngestStats`, `index_batch`). The tokenizer version is part of the extractor versions. Known issue (pre-existing, out of scope): `index-file` stores its path as given rather than repo-relative. Pre-fingerprint files re-index once. Deleted-file removal is `--prune` (story 10).

`--reindex` re-indexes regardless; `--force` does not. `--force` overrides two refusals:

- the missing-extractor refusal (since #183), on both `index` and `index-file`: a run is refused when the repo holds symbols from an extractor that the parsing store (this build, or the cluster leader with `--server`) lacks, because those files would be stored tokens-only;
- with `--prune`, the empty-run guard: a run that indexed nothing but would remove files.

**16. Python extractor via the Extractor trait (5 pts)**
Status: Delivered in PR #132 (story 27), as a token-stream scanner rather than `ruff_python_parser` (blocked on MSRV, see ADR 0002); a file with unbalanced brackets or broken indentation is flagged `has_errors` and keeps tokens only.
As an AI-agent integrator
I want Python files indexed with symbols
So that mixed-language repos are navigable.
- Given a Python class with methods, When indexed, Then `type` and `method` Symbols must exist with correct containment and tokens.
- Given a repo with `.rs` and `.py` files, When searched by token text, Then results from both must be returned with a language field.
- Given the Python extractor, When implemented, Then the change must touch only extractor code and registration, with no schema, storage or query changes.
- Given a Python file with a syntax error, When indexed, Then it must fall back to the generic tokenizer and be flagged `has_errors`.

**17. Error tolerance and JSON output with stable API (5 pts)**
As an AI-agent integrator
I want indexing to survive bad files and every query to have machine-readable output
So that agents can run unattended and parse results.
- Given a file that fails to extract, When indexing a directory, Then indexing must continue, the file must be listed with a reason, and the exit code must signal partial failure.
- Given any CLI query with `--json`, When run, Then stdout must be valid JSON with the same fields as human output and diagnostics must go to stderr.
- Given the library crate, When documented, Then every public query function must have rustdoc with an example that compiles as a doc test.
- Given a change to a result shape, When released, Then the crate version must bump per semver.

**18. Remove data and benchmark at scale (5 pts)**
As a database operator
I want to delete indexed data and know how the store performs on large inputs
So that I can manage disk use and trust performance.
- Given `remove --repo R`, When run, Then the Repo and all descendants must be deleted and other repos must be unaffected.
- Given a corpus of at least 1,000 files across at least 3 languages, When benchmarked, Then index time, on-disk size and p95 latency of story 8 and 11 queries must be recorded in the docs.
- Given results that miss the epic targets, When recorded, Then follow-up stories must be filed before the epic closes.

**19. Spike: WASM-hosted extractor plugins (3 pts)**
As a solo developer building the indexer
I want to test loading extractors as WASM modules through a pure-Rust runtime such as `wasmi`
So that new languages can be added without recompiling the database.
Time-boxed to 1 day. Output: documented findings + story estimate.
- Given a minimal extractor compiled to WASM, When loaded by the runtime, Then the findings record whether it can take file bytes and return NDJSON records matching story 13.
- Given the runtime, When assessed, Then the findings confirm it is pure Rust and record its speed against a native extractor on a 10k-line file.
- Given the recommendation, When the spike closes, Then it must state go or no-go and the follow-up stories needed.

**20. Single-node server and client (8 pts)**
Status: Delivered in PR #113 (squash 0780415). RPC overhead: [spikes/rpc-overhead.md](spikes/rpc-overhead.md).
As a developer or agent
I want a `memory-graph serve` process to own the database and the command line to talk to it over gRPC
So that many readers and one index run share a database without fighting over the file lock, from any machine.
Design: [ADR 0004](adr/0004-client-server-and-replication.md) D1-D4; delivers ADR 0003 story 12a (gRPC over TCP instead of a Unix socket).
- Given `memory-graph serve --db ./g --listen 127.0.0.1:7000`, When a second process runs `search --server 127.0.0.1:7000` during an index run through the server, Then it must get repeatable results and must never see a partially applied group commit.
- Given the `.proto` contract under `crates/graph-proto/proto/`, When `cargo run -p xtask -- proto` regenerates the checked-in code, Then CI must fail on any diff, and no `protoc` binary may be required anywhere.
- Given the client `RemoteStore`, When the store conformance suite (`run_all`) runs against it over an in-process test server, Then every case must pass, and `run_differential(embedded, remote)` must find no difference on the same inputs.
- Given `index`, `search`, `describe` and `export` run with `--server` against the vendored corpus, When their output is diffed against an embedded run, Then the outputs must be identical, and the served file reopened embedded must answer identically.
- Given two `serve` processes on one file, When the second starts, Then it must fail with `Locked`, and an embedded `--db` open of a served file must retry with jittered back-off for 5 s and then fail with a message naming `serve` and the holder from the `LOCK` sidecar.
- Given a client with an unknown `protocol_version`, When it calls `Hello`, Then the server must refuse with `FAILED_PRECONDITION` and the CLI must exit with code 5.
- Given `--db` and `--server` together (flag or `MEMORY_GRAPH_SERVER`), When any command runs, Then it must be refused with a message naming both; given `--chunk-bytes` with `--server`, Then it must be refused (entries are cut at 8 MiB by the leader); given `--cache-bytes` with `--server`, Then it must be refused with "pass to serve".
- Given the RPC-overhead benchmark at 10 M tokens, When measured, Then p50 overhead over embedded must stay under the 5 ms trigger recorded in ADR 0003 Q5.
- Given a `search` with no `--limit` and more than 1000 hits through `--server`, When it runs, Then all hits must be returned from one frozen view (the server's default limit and `applied_default_limit` paging must be invisible to the caller).
- Given a server-side snapshot handle older than 15 minutes, When it is used, Then the server must answer `SnapshotExpired` (`FAILED_PRECONDITION`); given 64 open handles on one connection, Then the 65th must be refused.
- Given every `StoreError` variant, When mapped to a gRPC `Status` and back (property test), Then it must round-trip; given a `PreparedFile::remote` handed to the embedded store, Then it must be rejected with `Rejected` (conformance case).
- Given `cluster leader` with no leader, Then the exit code must be 3; given a write whose deadline expires without a leader, Then the exit code must be 4.
- Given a stale `LOCK` sidecar whose pid is dead, When an embedded open succeeds, Then the sidecar must be ignored and replaced, and the "busy" message must never name a dead holder.
- Given the workspace after this story, When `scripts/check-no-c-deps.py` runs, Then it must pass, and it must name the crate when a deny-listed crate (`ring`, `aws-lc-sys`, `openssl-sys`, `libz-sys`) enters the tree.

**21. Replication: Raft log, snapshots, `--bootstrap` (8 pts)**
Status: Delivered in PR #114. Measurements: [spikes/raft-replication.md](spikes/raft-replication.md).
As a database operator
I want several `serve` nodes to replicate one database through Raft
So that the data survives the loss of a node and every node can answer reads.
Design: ADR 0004 D5-D7.
- Given `serve --data-dir <dir> --bootstrap --node-id 1` and two more nodes added as learners and promoted, When files are indexed through the leader, Then every node must apply the same entries and `run_differential(remote n1, remote n3)` and the embedded oracle must find no difference.
- Given a write, When the leader acknowledges it, Then the entry must already be fsynced in the Raft log on a majority and applied on the leader in one fsynced store transaction that records `last_applied`. Tested with an instrumented redb `StorageBackend` per node that records write and fsync order and can "power-cut" (discard every byte written since the last fsync): after an acknowledged write, power-cut a majority and restart, the write must be present; power-cut before the acknowledgement was returned and restart, then no torn state and `last_applied` <= committed; the `LogFlushed` callback must never be invoked before the redb commit returns; an acknowledged write must survive SIGKILL of all three nodes at once.
- Given a node killed after the log commit and before apply, When it restarts, Then it must replay the entry exactly once (`crash_after_log_before_apply_replays_once`, `kill_during_apply_reapplies_exactly_once`): afterwards `last_applied` in `RAFT_SM` must equal the log's committed index, and `count_nodes` and `describe` must equal a fresh embedded oracle.
- Given a 20 MiB single file and a 100-file batch, When indexed through the server, Then entries must be cut per the 8 MiB rule (the large file alone in its entry) and every replica must answer identically; given `prune` (not dry-run) and `vacuum` through the server, Then they must replicate; given `vacuum --compact`, Then no log entry may be written.
- Given a follower whose disk fills during apply, When writes continue, Then no acknowledgement may be lost (the majority is elsewhere), the follower must report `RESOURCE_EXHAUSTED`, and it must catch up once space is freed; given the leader's disk fills after the log commit and before apply, Then the write must not be acknowledged, the marker must be unchanged, and the entry must apply on the next attempt, never be skipped.
- Given Windows, When the CI `probes` matrix runs, Then the kill test must use `TerminateProcess`, rename-on-install must succeed with an open snapshot handle on that node (the handle ends with `SnapshotExpired`), and `Ctrl-C` must remove the `LOCK` sidecar.
- Given the leader stops, When a new leader is elected, Then writes must resume, and `LOCAL` reads on every remaining node must have succeeded throughout.
- Given a follower that fell behind the log purge point, When it reconnects, Then it must catch up through `InstallSnapshot` and then match the leader.
- Given a snapshot taken with `cluster snapshot --out`, When it is restored with `serve --bootstrap --restore` into an empty directory, Then the new cluster must answer every query as the original did and must carry a new cluster id.
- Given a corpus index plus a snapshot, When the size gate runs, Then Raft log bytes must be at most 1.5x the source, and bytes per entry and fsync throughput must be recorded in `docs/spikes/raft-replication.md`.

**22. Membership and write forwarding (8 pts)**
Status: Delivered in PR #118. Settled details: [ADR 0004](adr/0004-client-server-and-replication.md) D9 ("Settled in stage C").
As a database operator or agent
I want to add, promote and remove nodes safely and to write through any node
So that the cluster grows and shrinks without downtime and clients need not find the leader.
Design: ADR 0004 D6, D8, D9.
- Given `serve --join <peer> --auto-promote` on an empty directory, When it starts, Then it must copy the cluster id, join as a learner, catch up, and become a voter once its lag is zero; with `--standby` it must stay a learner.
- Given `--bootstrap` or `--join` on a directory that already belongs to the same cluster, When the node restarts with the same command line, Then it must resume from persisted state; on a directory from another cluster it must refuse with `WrongCluster`.
- Given an `index` sent to a follower, When it completes, Then the response must carry `forwarded_to_leader` and the applied index must equal the leader's.
- Given a partition that isolates a minority, When clients use the minority, Then `LOCAL` reads must succeed, writes must fail with `NoLeader`, and after the partition heals every node must converge.
- Given `cluster remove`, When it targets the leader, 3 voters down to 2 without `--force`, or anything below quorum, Then it must be refused with the reason.
- Given a node whose extractor version set hash differs, When promotion is requested, Then it must be refused.
- Given membership changes under an index load, When they complete, Then no acknowledged write may be missing on any voter.
- Given a client retry that re-sends an `IndexChunk` already committed under the previous leader, When both entries apply, Then the second must apply as unchanged (fingerprint skip), `count_nodes(File)` and `describe` must be unchanged, and `run_differential` against the embedded oracle must pass.
- Given `--join` on a directory whose store is not empty, When it starts, Then it must be refused without `--accept-snapshot-overwrite`.

**23. Linearizable reads and crash tests (5 pts)**
Status: Delivered in PR #119. Settled details: [ADR 0004](adr/0004-client-server-and-replication.md) D8 ("Settled in stage D").
As an AI-agent integrator
I want a read mode that is guaranteed to see every acknowledged write
So that an agent can index and then query without a race.
Design: ADR 0004 D7, D8.
- Given node 3 held behind by the fault-injecting `RaftNetwork` (its `AppendEntries` dropped) and a write acknowledged on node 1, When `--read linearizable` runs on node 3, Then it must include that write (the read waits for or forwards to the leader), while `--read local` on node 3 must not and must carry `stale_possible: true`.
- Given the old leader stopped (SIGSTOP or partitioned) with a connected client, a new leader elected and a write acknowledged there, When the old leader resumes and the client runs `--read linearizable` on it, Then it must answer `NotLeader`/`NoLeader` or the new data, never the pre-write answer; a history checker over concurrent writers and linearizable readers must see the applied index monotonic per client.
- Given `--read local` (the default), When the node has no known leader or lags the leader, Then the JSON output must carry `stale_possible: true`.
- Given a linearizable read on a minority partition, When it runs, Then it must fail with `NoLeader` rather than answer stale data.
- Given the CI `cluster` job, When it spawns three binaries, kills the leader with SIGKILL mid-batch and restarts it, Then every acknowledged batch must be present on all nodes.

**24. Observability and packaging (5 pts)**
Status: Delivered in PR #121; see [docs/guide/observability.md](guide/observability.md) (formerly the README's Observability section) and [docs/deploy/](deploy/kubernetes.md).
As a database operator
I want logs, metrics, health probes and ready-made deployments
So that I can run the cluster in Docker Compose or Kubernetes and see what it is doing.
Design: ADR 0004 D10.
- Given `--log-format json`, When the server logs, Then every line must be one JSON object with level, target and message.
- Given `--metrics-listen`, When scraped, Then the Prometheus text must parse and must include the Raft term, leader id, role, log, committed, applied and snapshot indexes, per-peer replication lag, store and log bytes, RPC durations and forwarded write counts; `cluster status --json` must show the same numbers.
- Given the gRPC health service, When the store is open, Then the default service must be `SERVING`; the `memory-graph.ready` service must be `SERVING` only while a leader is known; `memory-graph health [--ready] --server` must exit non-zero otherwise.
- Given `deploy/compose/cluster.yml` (the first place the container image from story 20, with `EXPOSE` and the health check, is exercised as a cluster), When CI runs `up --wait`, indexes via node 2, queries node 3, stops node 1, writes and reads again, restarts node 1 and waits for sync, Then every step must succeed and `down -v` must leave nothing behind.
- Given `docs/deploy/kubernetes.md`, When followed, Then it must describe a StatefulSet with a headless service for `--advertise`, node ids from the ordinal, gRPC readiness on `memory-graph.ready`, a PodDisruptionBudget of `minAvailable: 2` and `cluster remove` before scale-down.

**25. Cluster hardening (3 pts)**
Status: Delivered in PR #124: the benchmark at the corpus and at 10 M tokens and the 60-minute soak in [spikes/raft-replication.md](spikes/raft-replication.md) ("Stage F at scale"), `scripts/cluster_soak.py` (weekly in the `cluster` CI workflow), `serve --update-advertise` (issue #107) and `serve --config <file.toml>` (issue #106). This closes the epic's client/server stories 20-25.
As a database operator
I want measured replication performance, a soak run and the remaining operator knobs
So that the cluster can be trusted at the epic's scale target.
Design: ADR 0004 revisit triggers.
- Given the replication benchmark, When run at the story 18 corpus and at 10 M tokens, Then replicated ingest throughput relative to embedded, snapshot install time and linearizable read latency must be recorded in the docs, and any number past a revisit trigger must open an issue.
- Given a soak run of at least one hour with continuous indexing and a node restarted every five minutes, When it ends, Then no acknowledged write may be missing and the Raft log must stay within the purge policy.
- Given `serve --update-advertise <addr>` on an existing member, When it restarts, Then the cluster must learn the new address without a re-join.
- Given the open configuration question (ADR 0004 Q2), When this story closes, Then either a TOML configuration file is delivered or an issue records why not.

**26. Tokenizer dialects and scan helpers for more languages (5 pts)**
Status: Delivered in PR #126. Follow-ups: #127.
As an extractor author
I want the generic tokenizer to know each language's comments, strings and column layout, plus shared scan helpers
So that a new language's symbol scanner stays small and its token spans stay exact.
- Given the new dialect flags (`#`, `--`, `{- -}`, `(* *)` and `;` comments, triple-quoted, backtick, SQL and shell strings, primes, hyphenated names, COBOL and RPG fixed columns), When any input is tokenized, Then every token's text, bytes, line and column must match the source, and the existing dialects' output must be byte-identical (goldens unchanged, `TOKENIZER_VERSION` unchanged).
- Given `code_index`, `keyword_block`, `indent_block` and `line_iter`, When used by an extractor, Then they must be documented in `docs/adding-a-language.md` and unit-tested.

**27. TypeScript, Python and Java symbol extractors (8 pts)**
Status: Delivered in PR #132 (closes #73 and delivers story 16).
**28. C, C++, Go and Scala symbol extractors (8 pts)**
Status: Delivered in PR #130.
**29. SQL, shell, R, F#, Haskell, Elixir and GDScript symbol extractors (8 pts)**
Status: Delivered in PRs #128 (SQL, shell, R) and #129 (F#, Haskell, Elixir, GDScript).
**30. COBOL, RPG and assembly symbol extractors (8 pts)**
Status: Delivered in PR #131 (RPG IV free, mixed and fixed form; NASM, MASM and GNU as for x86 and ARM). Follow-ups: #133, #134.

Stories 27-30 share one story:
As an AI-agent integrator
I want symbols (types, callables, modules, variables and constants) for each of these languages
So that the `symbol`, `method` and `class` grains and symbol search work across a polyglot codebase.
- Given each language, When indexed, Then its documented symbol kinds must exist with exact span text, containers must be `type` so the class grain rolls up, and namespaces or packages must be `module`.
- Given each extractor, When implemented, Then the change must touch only its own crate plus registration (one `lang-*` feature, on by default), with no schema, storage or query changes.
- Given malformed, BOM-prefixed, CRLF, non-ASCII or 20000-level nested input, When indexed, Then there must be no panic or stack overflow and every span must stay valid.
- Given a suitable MIT or Apache-2.0 public repo, When the corpus test runs, Then a pinned slice must be indexed with spot-checked symbols; languages without one must have a hand-written fixture.

**31. `graph-mcp` core and `memory-graph mcp` over stdio, read-only (5 pts)**
Status: Delivered in PR #160.
As an AI-agent integrator
I want my assistant to start `memory-graph mcp` and call read-only tools over stdio
So that it can search the graph, find symbols, describe repos and outline files with no network setup.
Design: [ADR 0005](adr/0005-mcp.md) D1, D2, D3, D5 (Accepted).
- Given `memory-graph mcp` with `--db` or with `--server`, When a client calls each of the seven tools (`describe`, `list_repos`, `search`, `find_symbols`, `file_outline`, `file_tokens`, `list_files`), Then every tool must work against both targets.
- Given the same query, When run through MCP and through `--json` / `StoreRead`, Then the results must be equal (differential test).
- Given every tool, When it answers, Then its `structuredContent` must validate against its declared `outputSchema`, and its input must validate against `inputSchema`.
- Given a running `memory-graph mcp`, When it logs, Then stdout must carry only protocol messages; logs go to stderr.
- Given the change, When CI runs, Then `scripts/check-no-c-deps.py` must pass and `cargo tree -i ring` must print nothing.
- Given `docs/mcp.md`, When followed, Then it must give working client configuration for stdio.

**32. Streamable HTTP MCP endpoint in `serve` with security guards (5 pts)**
Status: Delivered in PR #166.
As a database operator
I want an opt-in MCP endpoint inside `serve`
So that assistants can query a running cluster without starting a local process, safely without authentication.
Design: [ADR 0005](adr/0005-mcp.md) D1, D3, D4 (Accepted).
- Given `serve` without `--mcp-listen`, When it starts, Then no MCP endpoint must be listening.
- Given a non-loopback `--mcp-listen`, When `serve` starts without `--mcp-allow-remote`, Then it must refuse to start; with the flag, it must start and log a warning at every start.
- Given a request with an Origin not in `--mcp-allow-origin` or a Host that is not the bound loopback name or address, When received, Then the response must be 403.
- Given the D4 limits, When exceeded, Then a body over 1 MiB must get 413, a result over 4 MiB must be truncated with `next_offset`, requests past `--mcp-max-inflight` must be refused, and a call past 30 s must time out.
- Given the in-process service adapter, When a tool runs, Then `stale_possible` and linearizable reads must match what a gRPC client sees.
- Given `memory-graph health`, When MCP is enabled, Then it must report that and the address.
- Given the three backends (embedded, `RemoteStore` over `TestServer`, the in-serve adapter over a 3-node `ClusterTestbed`), When the tool conformance suite runs, Then all three must pass.

**33. Opt-in MCP write tools (3 pts)**
Status: **Deferred** until authentication (#105); the owner decided on 2026-09-29 that an unauthenticated endpoint stays read-only. Depends on story 69 (authentication, scheduled 2026-10-09), which delivers #105.
As an AI-agent integrator
I want `index_path` and `prune` tools
So that an assistant can keep the graph current without a separate CLI call.
Design: [ADR 0005](adr/0005-mcp.md) D2 (Accepted).
- Given no `--mcp-allow-writes`, When a client lists tools, Then the write tools must not appear.
- Given `--mcp-index-root`, When `index_path` names a path outside it after canonicalisation, Then the call must be refused.
- Given `prune` with no `dry_run` argument, When called, Then it must run as a dry run.

**34. MCP real-client e2e and hardening (3 pts)**
Status: Delivered in PR #168, with `rmcp` (dev-dependency only, no TLS; it passes the gate) as the real client. This completes v1 of ADR 0005 (stories 31, 32 and 34; 33 stays deferred) and closes #109.
As a database operator
I want MCP proven with a real client, against a cluster under failure, and under fuzzing
So that the tool surface can be trusted.
Design: [ADR 0005](adr/0005-mcp.md) test plan (Accepted).
- Given the real binary over stdio and over HTTP on `127.0.0.1:0`, When a real client (`rmcp` as a dev-dependency if it passes the gate, otherwise our own client plus a recorded MCP Inspector transcript) runs initialize, initialized, `tools/list` and `tools/call search` on the corpus, Then the handshake must succeed and the spans must be exact.
- Given MCP on a follower of a 3-node cluster, When the leader is killed, Then reads must keep working and report `stale_possible: true`.
- Given the JSON-RPC framing, When fuzzed, Then there must be no panic.
- Given metrics, When tools are called, Then per-tool call and error counters must be exported.
- Given `docs/mcp.md` and `serve --help`, When read, Then both must carry the boxed no-authentication warning.

**35. Snapshot backups: `file://` sink and verified restore (5 pts)**
Status: Delivered in PR #159.
As a database operator
I want the leader to copy each snapshot to a backup directory and restore from it with verification
So that a cluster can be rebuilt after losing every node.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) E1-E3, E7-E10 (Accepted).
- Given `--backup-url file://<dir>`, When the leader builds a snapshot, Then it must write the data file first and the `.meta` last, under `<cluster_id>/`.
- Given a kill mid-copy, When the backups are listed, Then nothing from the interrupted copy may be restorable.
- Given a backup with one corrupt byte, When restored, Then it must be refused before `restore_into`, with nothing left behind.
- Given `--backup-keep N`, When more than N backups exist, Then only the newest N must remain.
- Given a failing sink, When snapshots and purges run, Then they must not be blocked or delayed.
- Given a local `--restore <file>` with a sibling `.meta`, When restored, Then the `.meta` must be verified; without one, a warning must be logged.
- Given a cluster restored from a backup, When `run_differential` runs against the source, Then the answers must be equal.
- Given the flags, When configured through TOML, Then they must behave the same; metrics and `cluster status` must report the last backup and the last error.

**36. S3 client over plain HTTP (8 pts)**
Status: Delivered in PR #167.
As a database operator
I want backups written to S3-compatible object storage
So that they live off the cluster's own disks.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) E3-E6 (Accepted).
- Given the AWS SigV4 test vectors, When signed, Then every vector must match.
- Given `graph-server::testing::FakeS3` with fault injection (drop after N bytes, 500 on part k, wrong ETag, 403, slow), When the fault matrix runs, Then every case must pass, and uploads over 64 MiB must use multipart and abort it on error.
- Given credentials in the environment and in a credentials file, When resolved, Then the environment must win, and secrets must never appear in logs or `Debug` output; a secret in TOML must be refused.
- Given an `https://` endpoint, When configured, Then it must be refused with guidance to a sidecar and #104.
- Given the change, When CI runs, Then the gate must be clean and every new crate must be MIT or Apache licensed.

**37. Restore from `s3://` and `latest`, `cluster snapshot --upload`, `cluster backups` (3 pts)**
Status: Delivered in PR #170.
As a database operator
I want to restore from object storage by URL and see what backups exist
So that recovery is one command.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) E1, E8, E10 (Accepted).
- Given `serve --bootstrap --restore s3://…/snap-T-I.redb`, When it completes, Then the restored cluster must answer like the source.
- Given a size, sha256, `store_format_version` or `extractors_hash` mismatch, When restoring, Then it must be refused with nothing left behind.
- Given `…/latest`, When resolved, Then it must pick the highest committed index and ignore orphans without a `.meta`.
- Given `cluster snapshot --upload` and `cluster backups [--json]`, When run, Then a snapshot must be uploaded on demand and the backups listed.

**38. S3 e2e CI job and backup docs (3 pts)**
Status: Delivered in PR #171 (CI job `s3-e2e` against SeaweedFS rather than MinIO, whose images can no longer be pulled from Docker Hub; `crates/graph-cli/tests/s3_e2e.rs`; docs in `docs/deploy/data-dir.md` and the README, whose walkthrough now lives in [docs/guide/cluster.md](guide/cluster.md#walkthrough-backups-to-s3-compatible-storage)). With it stories 35-38 are delivered; 39 stays deferred with #104.
As a database operator
I want backups tested against a real S3-compatible server and documented
So that I can set them up with confidence.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) testing (Accepted).
- Given an S3-compatible server (SeaweedFS in CI; MinIO's image is no longer pullable) over plain HTTP on Linux, When the CI job runs upload, retention and restore, Then it must be green.
- Given the docs, When read, Then they must cover the sidecar TLS recipe, bucket lifecycle rules and a minimal IAM policy, and the README walkthrough (now in docs/guide/cluster.md) must work.

**39. Spike: native HTTPS for backups behind `backup-tls` (5 pts)**
Status: **Deferred**, shared with #104 (TLS on the wire); it waits for a pure-Rust rustls crypto provider that passes the gate.
As a database operator
I want backups sent to AWS over HTTPS without a sidecar
So that the deployment has one less moving part.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) E5 (Accepted).
- Given candidate providers (rustls-rustcrypto, graviola), When evaluated, Then the spike must record whether one passes `scripts/check-no-c-deps.py` and works against AWS S3.
- Given a passing provider, When the `backup-tls` feature is enabled, Then `https://` endpoints must be accepted, and the default build must stay unchanged.

**40. Decode any source encoding in `graph_core::encoding` (5 pts)**
Status: Delivered in PR #176 ([ADR 0007](adr/0007-source-encodings.md), Accepted 2026-09-30).
As a developer indexing a legacy or Windows codebase
I want each file decoded to UTF-8 whatever its encoding
So that UTF-16 and code-page files are indexed instead of skipped.
Design: [ADR 0007](adr/0007-source-encodings.md) C2-C5, C10.
- Given a file with a UTF-8, UTF-16LE or UTF-16BE BOM, When decoded, Then that encoding must be chosen from the BOM and decoded with `decode_without_bom_handling`, so U+FEFF stays at the start of the text, even when a different hint is given.
- Given valid UTF-8 without a BOM, When decoded, Then the text must be the input borrowed unchanged, with encoding `UTF-8` and `lossy` false.
- Given BOM-less UTF-16LE or BE source made only of ASCII (which is also valid UTF-8 as bytes), When decoded, Then the NUL-pattern sniff must run before the UTF-8 check and detect UTF-16, not UTF-8; BOM-less CJK UTF-16 must be detected too, not skipped as binary (owner amendment, 2026-09-30).
- Given windows-1252, Shift_JIS, GBK, EUC-KR or Big5 source, When decoded, Then `chardetng` must pick that encoding, falling back to windows-1252; given 7-bit ISO-2022-JP source, Then it must be detected by a JIS X 0208 designation (`ESC $ B` / `ESC $ @`) before the UTF-8 check (owner amendment, 2026-09-30); and an auto-detected file must never be `lossy`.
- Given a hint that does not fit the bytes, or a BOM followed by invalid sequences, When decoded, Then invalid sequences must become U+FFFD and `lossy` must be true (the only two ways to be lossy); the `replacement` encoding must be refused as a hint; and given random bytes (proptest), Then decoding must never panic and `lossy` must be set iff a replacement was inserted.
- Given a PNG, When checked, Then `is_binary` must be true; given a BOM-less UTF-16 file, Then it must be false.
- Given `ansi`, When resolved, Then it must be the system code page on Windows (a mapped `GetACP`) and windows-1252 elsewhere; and `encoding_rs` and `chardetng` must be `=`-pinned and pass `check-no-c-deps.py`, with `DECODER_VERSION` exported for the fingerprint and the cluster hash.

**41. Store integration: decoded spans, binary rejection, schema 11 restamp, fingerprint rule (8 pts)**
Status: Delivered in PR #177 ([ADR 0007](adr/0007-source-encodings.md), Accepted 2026-09-30).
As a user of any store (embedded, `--server`, a Raft cluster)
I want every writer to decode and store files the same way
So that encoded files are searchable and all replicas agree.
Design: [ADR 0007](adr/0007-source-encodings.md) C1, C2, C5-C8, C10.
- Given a non-UTF-8 file, When indexed, Then every token's and symbol's text, byte range, line and column must be exact against the decoded text, the language must be detected from the decoded text, and the File node must record its `encoding` and `lossy` (both omitted for UTF-8).
- Given the same encoded file through `index_batch`, `index_bytes_opts` (`index-file`), `--server` and a raw `Index` RPC, When indexed, Then each must go through `prepare_file` and give the same encoding, spans and fingerprint; `index_bytes_opts` must no longer have its own `from_utf8` or fingerprint.
- Given `BatchFile` and the `index_bytes*` options with the new `encoding` and `strict_encoding` inputs, When the conformance suite runs against embedded and `RemoteStore`, Then the hint and strict flag must behave identically on both.
- Given the UTF-8 corpus, When indexed, Then the output and the stored stream bytes must be identical to `main`, and `size_gate` must pass unchanged.
- Given a v10 or v9 database, When opened, Then it must be restamped straight to v11 and read identically, and re-indexing it must re-parse 0 files; given a newer version, Then it must be refused without writing; and a v10 node and a v11 node must refuse to share a cluster (`extractors_hash`, which now includes `DECODER_VERSION`). *(Superseded by the schema-12 amendment in ADR 0007 C6: the target is now v12, the restamp set is {9, 10, 11}, and a v11 file also gets its encoding counts recounted.)*
- Given a non-UTF-8 file re-indexed with a different `--encoding` that changes the decode, When indexed, Then only that file must be re-indexed (its fingerprint suffix `enc=<name>[+lossy]@<DECODER_VERSION>` changed); a UTF-8 file's fingerprint must be unchanged.
- Given a binary file sent through `index-file`, `--server` or a raw `Index` RPC, When indexed, Then it must be rejected as binary.
- Given encoded inputs, When `run_all`, `run_differential` and `run_crash_rerun_differential` run, Then embedded, remote and Raft stores must answer identically; golden-byte tests must pin UTF-8 (unchanged), UTF-16LE and lossy File nodes, and an unknown stored encoding name must be a `StoreError::Corrupt`, not a panic.
- Given an encoded file, When `export_ndjson_round_trips_node_counts` runs, Then it must round-trip, with `encoding` and `lossy` in the NDJSON only for non-UTF-8 files; vacuum and snapshot export must need no change.

**42. `--encoding`, `--strict-encoding` and `.memory-graph.toml` per-glob overrides (3 pts)**
Status: Delivered in PR #178 ([ADR 0007](adr/0007-source-encodings.md), Accepted 2026-09-30).
As a developer whose tree mixes encodings
I want to override detection per run or per glob
So that a mis-detected code page can be fixed.
Design: [ADR 0007](adr/0007-source-encodings.md) C3, C4, C8.
- Given `index --encoding <label>` or `index-file --encoding <label>` with any `encoding_rs` label or `ansi`, When run, Then files must decode with it (a BOM still wins); an unknown label must be a usage error.
- Given a `.memory-graph.toml` at the indexed root with an `[encoding]` table of globs, When a directory is indexed, Then precedence must be BOM > `--encoding` > the first matching glob > auto, resolved on the client and sent per file; an edit to the file must re-index exactly the affected files without `--reindex`.
- Given `--strict-encoding` and a file that would decode lossily, When indexed, Then that file must be refused and reported, and the rest indexed; and `--encoding utf-8 --strict-encoding` must reproduce today's `NotUtf8` refusal.
- Given a directory run, When it finishes, Then the tallies must no longer skip UTF-16 files as binary. (The transcoded and lossy counts moved to story 43, owner, 2026-09-30.)

**43. Encodings on the wire, in `describe`/`--stats` and in MCP (5 pts)**
Status: Delivered in PR #179 ([ADR 0007](adr/0007-source-encodings.md), Accepted 2026-09-30).
As an agent or operator reading the graph
I want to see which files were transcoded or decoded lossily
So that I can trust, or fix, what a search returns.
Design: [ADR 0007](adr/0007-source-encodings.md) C8.
- Given `--server`, When a client sends `FileBytes.encoding_hint` (with `ansi` resolved on the client) and `strict_encoding`, Then the server must honour them, and the proto change must be additive and regenerated with the xtask.
- Given a repo with non-UTF-8 files, When `describe` (text and `--json`) runs, Then it must list the encodings and the lossy count per repo, from the catalog, in O(repos).
- Given `symbols` and `search --json`, When a hit is in a non-UTF-8 file, Then its `encoding` (and `lossy` when set) must be shown; and `--stats` must show transcoded and lossy counts.
- Given a directory run, When it finishes, Then its tallies must count transcoded and lossy files (moved from story 42, owner, 2026-09-30).
- Given the MCP `describe` and `list_files` tools, When called, Then they must include the encoding where it is not UTF-8.

**44. Encoding fixtures, cross-encoding search tests and docs (3 pts)**
Status: Delivered in PR #181 ([ADR 0007](adr/0007-source-encodings.md), Accepted 2026-09-30).
As a maintainer
I want every supported encoding covered end to end and documented
So that the feature does not regress and users know how to use it.
Design: [ADR 0007](adr/0007-source-encodings.md) test plan.
- Given one fixture per encoding (UTF-16LE/BE with and without a BOM, windows-1252, Shift_JIS, GBK, EUC-KR, Big5) and one invalid-bytes file, When indexed, Then each must record the expected encoding, exact spans against the decoded text, and its symbols (e.g. a C# class in a UTF-16 file).
- Given the same identifiers (e.g. `CustomerId`, `café`, `日本`) in UTF-8, UTF-16LE, UTF-16BE, windows-1252 and Shift_JIS files, When searched, Then one search must return hits from every file and the dictionary must hold one term per identifier.
- Given the docs, When read, Then `docs/guide/indexing.md` must have an Encodings section (detection order, overrides, limits of cross-encoding matching), the glossary must define decoded source and lossy, ADR 0003 must point to ADR 0007, and the CLAUDE.md exact-spans invariant must read as in ADR 0007 C1.

<a id="story-45"></a>
**45. Read-path measurement and benchmark (3 pts)**
Status: Partly delivered in PR #230 (counters, read benchmark, spike doc). Not yet done, tracked in #233: export on the metrics endpoint, decode byte counters, query wall time, the bypass rate, a fresh-process cold run, the `#[ignore]` benchmark form, and the large-index re-run.
As a maintainer deciding whether to build a read cache
I want decode costs and cold, warm and concurrent read numbers
So that the cache is built only if it pays.
Design: [ADR 0008](adr/0008-read-cache.md) phase 0.
- Given a running `serve`, When queries run, Then the metrics endpoint must export count, bytes and nanoseconds for dictionary-block, symbol-section and node decodes, plus query wall time; a test must check the counters move by the expected counts for a fixed query.
- Given the vendored corpus and the agent-like mix, When the `#[ignore]` benchmark is run by hand, Then it must report p50/p95 latency and decode share (sum of a reader's decode timers / sum of its query wall time, median across readers) for cold (fresh process, empty redb cache), warm, and 8, 16 and 32 readers, plus the cache-bypass rate with a concurrent indexer running.
- Given the results, When recorded in `docs/spikes/read-cache.md`, Then the doc must state go or no-go against the gate (>= 25% warm single-reader, or >= 25% at any of 8/16/32 readers) and the owner's sign-off.
- Given CI, When it runs, Then no timing may be asserted; and `run_differential` answers must be unchanged by the counters.

<a id="story-46"></a>
**46. Dict lookup without full decode; page-cache sizing (3 pts)**
Status: Delivered in PR #235. The dictionary scan allocates only the hit, and the CLI and `serve` derive the page cache size. After phase 1 the gate is not met on the vendored corpus (decode share 11-24%). Follow-up #236: the ingest memory budget versus the larger page cache.
As a user querying a large store
I want a term lookup to decode only the string it needs, and a page cache sized for my machine
So that queries do less work and the documented cache size is true.
Design: [ADR 0008](adr/0008-read-cache.md) phase 1.
- Given an id in a reverse dictionary block, When `dict_rev_lookup` reads it, Then the `dict_strings_decoded` counter must rise by exactly 1, and the string must equal a full decode's (property-tested over random blocks, including empty, one-byte, truncated, last-id and full 64 KiB blocks; a truncated block must be `StoreError::Corrupt`, not a panic).
- Given injected values, When `derive_cache_bytes(avail)` runs, Then it must return 25% of `avail` clamped to [64 MiB, 4 GiB] (unit-tested at 0, 128 MiB, 1 GiB, 64 GiB).
- Given `--cache-bytes`, When set, Then it must override the derived size.
- Given ADR 0003, When read, Then it must carry a dated note pointing to ADR 0008 for the new default.
- Given phase 1 is merged, When story 45's benchmark is re-run, Then the gate must be re-evaluated in the spike doc.

<a id="story-47"></a>
**47. Decoded-object cache core and MVCC-safe invalidation (8 pts)**
Status: Not started; gated. The gate was not met after phase 1 (story 46). Start only if the large-index re-run (#233) shows a decode share of 25% or more.
As an agent sending many queries to `serve`
I want decoded dictionary blocks, symbol sections and file context reused across queries
So that warm queries skip repeated decoding without ever seeing stale data.
Design: [ADR 0008](adr/0008-read-cache.md) phase 2 (generations).
- Given any write path in the ADR's coverage table, When it commits, Then it must go through `RecordingWriteTxn::commit(touched)`, the seqlock generation must be odd during the commit and published (even) only after a successful commit, and an aborted transaction must not bump; marker-only Raft entries must bump with no keys, `commit_each_counted` must publish one generation per chunk, `rebuild_refs` and the test hooks must declare `TouchedKeys::All`.
- Given a failpoint between commit and bump, When it fires, Then the floor must be raised and no reader may be served a stale entry.
- Given a reader, When it begins, Then it must never block on a writer (seqlock: read `gen`, `begin_read`, re-read, register, re-read) and must bypass if `gen` was odd or changed, with the Acquire/Release ordering in the ADR pinned by a `loom` model and a stress test checking each snapshot's `next_id` and marker against its `G`; a call opening several read transactions must check and register each; given `--read-cache-bytes 0`, Then readers must not touch the generation machinery; and a manual benchmark (not CI) must show reader p99 under a concurrent indexer no worse with the cache on than off.
- Given entries tagged with the populating reader's `Ge`, When looked up at `G`, Then they must be used iff `G >= F_now && Ge >= F_now && last_mod(K) <= min(G, Ge)` with `F_now` read at lookup time and an absent record counting as below `F_now` (the safety check); given a slow old reader that inserts after a newer write, Then the insert must be skipped (the optimisation) and, if forced in, still never served.
- Given the deterministic interleavings (reader begins / writer commits / reader looks up; a newer reader's entry not served to an older reader), When run, Then each must read what an uncached store returns.
- Given vacuum or `vacuum_marked`, When committed, Then `F` must be raised; given a reader active while the `max_mod_records` cap fires, When it then looks up a key re-populated at or above the new `F`, Then it must bypass; given a held snapshot handle, When writers run, Then pruning and the cap must run only in the writer's publish step, `F` must be raised (Release) before records are dropped (`F = max(F, max_dropped)` for a prune, the published generation for the cap), interleaving (h) must hold (a reader looking up between the F raise and the record drop is never served stale data. For a cap it bypasses (F = N+2 is above every active reader); for a prune (F = max_dropped <= Gmin) it either sees the record or rejects an entry with Ge < F.), an orphaned gRPC handle must release `Gmin` at the snapshot-handle TTL, a panicking reader must deregister in `Drop`, and `read_cache_floor_raises` must count each raise by cause.
- Given a cached dictionary block, When a reader with a lower high-water mark reads it, Then only ids below its mark may be served, with no `last_mod` check (dictionary blocks are exempt, being append-only).
- Given a Raft follower, When it applies entries, Then it must invalidate through the same path; given a snapshot install or compact, Then the `StoreSlot` swap must drop the old cache with the old store.
- Given cache values, When compiled, Then they must be `Arc<T: 'static + Send + Sync>` (no `ReadTransaction` held, by type); and the engine must pass `check-no-c-deps.py`, with unit tests for scan resistance (a one-pass scan does not evict the hot set), weight accounting and shard count.

<a id="story-48"></a>
**48. Read-cache tests, metrics and flags (5 pts)**
Status: Not started; gated with story 47.
As an operator and a maintainer
I want the read cache sized, observable, switchable and proven equivalent
So that it can be tuned or turned off and never changes an answer.
Design: [ADR 0008](adr/0008-read-cache.md) phase 2 (sizing, tests).
- Given neither flag, When a store opens, Then the page cache and the read cache must each get 50% of `derive_cache_bytes`; given `--read-cache-bytes N`, Then resident bytes must stay <= N plus one maximum entry per shard under the concurrent eviction stress test, with no stale answers; and the manual benchmark must report the bypass rate under a concurrent indexer.
- Given `--read-cache-bytes 0`, When queries run, Then every read-cache counter must stay 0 and no cache is allocated; writes must still bump the generation.
- Given the metrics endpoint, When queries run, Then it must export lookups, hits, misses, bypasses, inserts, rejected inserts, evictions and resident bytes, and `hits + misses + bypasses == lookups` must hold.
- Given `run_differential`, When run with the cache off, one entry, a mid-size eviction-heavy budget, a budget smaller than the smallest entry, and large, Then every answer must be identical; and `run_crash_rerun_differential` must pass with the cache on.
- Given concurrent writers and older readers and snapshot handles, When the consistency differential runs, Then each answer must equal an uncached store's at the same generation; snapshot handles held across install, compact and vacuum must keep answering their snapshot.
- Given `graph-client --test conformance` and `serve_e2e` with `--read-cache-bytes`, and `ClusterTestbed` follower reads after applies, a follower snapshot install and linearizable reads, When run, Then answers must match an uncached run.
- Given the size gate and the `codec.rs` golden bytes, When run, Then they must pass unchanged.

<a id="story-49"></a>
**49. Optional query-result cache (3 pts)**
Status: Not started; optional. Start only if a request log shows at least 20% of queries are exact repeats within 60 seconds at an unchanged generation. That log is tracked in #233.
As an agent that repeats the same query
I want the server to return a cached result when nothing has changed
So that repeated queries cost almost nothing.
Design: [ADR 0008](adr/0008-read-cache.md) phase 3.
- Given story 45's request log, When reviewed, Then this story must start only if at least 20% of queries are exact repeats within 60 seconds at an unchanged generation.
- Given a query at generation G, When the same query arrives at G, Then `serve` must return the cached result (a hit counter rises); given any write, Then the result cache must be cleared by raising its floor.
- Given the result cache on and off, When `run_differential` and the consistency differential run, Then answers must be identical.

<a id="story-50"></a>
**50. OpenTelemetry groundwork: dependencies, telemetry module, `MetricsSnapshot`, test seams (8 pts)**
Status: Accepted ([ADR 0009](adr/0009-opentelemetry.md), Accepted by the owner on 2026-10-09). Counted in the totals. The groundwork (no export) shipped early in PR #248; the new dependencies need the owner's approval in the PR that adds them.
As a maintainer adding OpenTelemetry
I want the pinned crates, a telemetry module, one metrics snapshot and a fake collector in place, with no change in behaviour
So that traces, metrics and logs can be added and tested without touching the Prometheus contract.
Design: [ADR 0009](adr/0009-opentelemetry.md) D2, D3, D4, D6, D8 and D10.
- Given the crates in ADR 0009 D2, When they are added to `[workspace.dependencies]` with exact pins and the listed features only, Then:
  - `cargo tree -i ring` and `cargo tree -i aws-lc-sys` must print nothing;
  - `check-no-c-deps.py` (all six targets), `test_gate.py` and `docker build` must pass;
  - `cargo tree -d` must show no second tonic or prost;
  - the PR must record that `tracing-opentelemetry` targets `opentelemetry 0.33`.
- Given each pair of sources, When `--otlp-endpoint`, `--otlp-signals`, `--otel-service-name` and `--otlp-metrics-interval` are set by flag, config key or environment variable, Then `TelemetryConfig` must follow the precedence flag, then key, then environment (unit-tested for each pair).
- Given environment variables only, When `serve` starts with just `OTEL_EXPORTER_OTLP_ENDPOINT`, Then OpenTelemetry must be enabled.
- Given `OTEL_SDK_DISABLED=true`, When an endpoint is also set, Then OpenTelemetry must be off.
- Given `--otlp-signals`, When it names a subset, Then only those providers may be built.
- Given headers, When they are set, Then they must come only from `OTEL_EXPORTER_OTLP_HEADERS`, and their values must never appear in logs.
- Given an `https://` endpoint from a flag or config key, When `serve` starts, Then it must exit with code 7 (`TELEMETRY_CONFIG`) and a message naming the setting (an e2e test).
- Given an `https://` endpoint, a per-signal endpoint variable or a non-`grpc` `OTEL_EXPORTER_OTLP_PROTOCOL` from the environment, When `serve` starts, Then it must log one error, start, serve, and have OpenTelemetry off.
- Given a valid endpoint that nothing listens on, When `serve` starts, Then it must start and serve.
- Given no endpoint from any source, When `serve` starts, Then `telemetry::init` must return `None` and `telemetry::is_active()` must be false. This is a structural check, not a wait for nothing to arrive.
- Given the `MetricsSnapshot` refactor, When `/metrics` is scraped, Then the output must be byte-identical to a golden file. The golden file is committed before the refactor, with volatile series normalised. The existing observability tests must also pass.
- Given the resource, When it is built, Then:
  - it must carry `service.name`, `service.version`, `service.instance.id`, `memory_graph.cluster` and `host.name`;
  - `OTEL_SERVICE_NAME` must win over `service.name` in `OTEL_RESOURCE_ATTRIBUTES`;
  - `OTEL_RESOURCE_ATTRIBUTES` must not override the instance id, the cluster or the version.
- Given the test seams, When tests run, Then:
  - `graph_server::testing::FakeCollector` must listen on 127.0.0.1:0, record traces, metrics and logs, and be able to stall, refuse, and stop and restart;
  - the batch delay, queue size and export timeout must be settable in tests;
  - no test may use port 4317 or a fixed sleep (they wait on collector counts with a deadline).
- Given SDK 0.33's batch processors, When this story merges, Then the follow-up issue for queue-full drop counting must be filed.

<a id="story-51"></a>
**51. Traces and W3C propagation across client, forward and leader apply (8 pts)**
Status: Accepted ([ADR 0009](adr/0009-opentelemetry.md), Accepted by the owner on 2026-10-09). Counted in the totals.
As an operator debugging a slow or failed request
I want one trace from the client through the follower and the forward to the leader and its apply
So that I can see where the time went across nodes.
Design: [ADR 0009](adr/0009-opentelemetry.md) D5, D8 and D9.
- Given a test that sets it up, When `cluster_e2e` starts three real `serve` processes that export to one in-test `FakeCollector` (its port passed with `--otlp-endpoint`) and a client writes to a follower inside a test span, Then the collector must receive one trace in which, checked by span id:
  - the follower's `rpc` has the client's span as its parent;
  - `forward` has the follower's `rpc` as its parent;
  - the leader's `rpc` has `forward` as its parent;
  - the leader's `apply` for that write is linked to the leader's `rpc` (or is its child, whichever the ADR records);
  - each span's `service.instance.id` is its node.
- Given a node's first start (bootstrap, or `--join` without `--node-id`), When it exports its first spans, Then their resource must already carry `memory_graph.cluster` and `service.instance.id`, with no restart needed: the providers are built, or the resource rebuilt, after `start` has resolved the node's identity (#249).
- Given an rpc span, When it is exported, Then it must carry `rpc.system=grpc`, `rpc.service`, `rpc.method`, `rpc.grpc.status_code` and `server.address`, and `forward` must carry `memory_graph.forwarded_by`.
- Given an incoming `traceparent`, When `RpcService::call` handles the request, Then the server span must be its child.
- Given each outgoing call, When it is sent, Then `SendVersion` (with the sync facade instrumenting the call with the caller's span), `ForwardHeaders` and `RaftHeaders` (for snapshot install) must inject `traceparent` and `tracestate`.
- Given an idle cluster with traces on, When heartbeats and AppendEntries run, Then the collector must receive no Raft-service spans.
- Given a snapshot install, When it runs, Then exactly one `install_snapshot` span must be exported on each side, with no per-chunk spans.
- Given MCP `tools/call`, an index batch and a `RemoteStore` call, When each runs with traces on, Then each must produce its span.
- Given a sentinel query string in a search and an MCP call, When the spans are exported, Then the sentinel must appear in no attribute.
- Given a stalled or stopped collector, When N RPCs run, Then all must succeed within a generous timeout (no timing assertion), and `mg_otel_export_failures_total{signal="traces"}` and `mg_otel_dropped_total{signal="traces"}` must rise.
- Given a collector that dies mid-run and comes back, When RPCs continue, Then trace export must resume with no restart.
- Given a shutdown under load, When `serve` stops, Then:
  - with a responsive collector, the spans still queued must arrive;
  - with a stalled collector, `serve` must exit within the shutdown bound.

<a id="story-52"></a>
**52. Metrics over OTLP with the name mapping (5 pts)**
Status: Accepted ([ADR 0009](adr/0009-opentelemetry.md), Accepted by the owner on 2026-10-09). Counted in the totals. Depends only on story 50, and can run in parallel with 51.
As an operator whose metrics pipeline is OpenTelemetry
I want every `/metrics` family exported over OTLP under an OTel-style name
So that I get the same numbers without scraping.
Design: [ADR 0009](adr/0009-opentelemetry.md) D6 and D8.
- Given the const mapping table in `telemetry.rs`, When the contract tests run, Then:
  - it must equal the table parsed from ADR 0009;
  - every `METRIC_NAMES` family must have exactly one OTLP twin, except the named exception `mg_rpc_total`;
  - every OTLP metric must map back.
- Given a frozen `MetricsSnapshot`, When it is converted to OTLP, Then every snapshot family must have exactly the expected instrument kind, unit, attributes and values. That includes one `memory_graph.raft.role` point per role (1 for the current role, 0 for the others) and `memory_graph.build.info`=1 with its three attributes.
- Given the fake collector and a short `--otlp-metrics-interval`, When an interval passes, Then:
  - every mapped metric must arrive;
  - snapshot families must match a `/metrics` scrape coarsely (counters that keep moving get a tolerance);
  - `rpc.server.call.duration` and `memory_graph.raft.apply.duration` must use the `DURATION_BUCKETS` bounds;
  - the histograms' counts must approximately match `mg_rpc_total` and the apply histogram's count.
- Given one collection, When several callbacks run, Then the snapshot must be built once (a counter shows it), and no callback may open a redb transaction.
- Given the e2e workload, When it runs, Then no instrument may reach the SDK overflow stream (`otel.metric.overflow`).
- Given a tiny `BatchTuning::max_queue_size` and `FakeCollector::stall`, When more spans or log records are queued than fit, Then the items queued minus the items exported must equal the rise in `mg_otel_dropped_total{signal}`, counted through a wrapping exporter or processor, or a narrowly filtered internal-logs path, since SDK 0.33 exposes no public drop counter (#250). Stories 51 and 53 rely on this counter.
- Given this story, When it merges, Then the Prometheus golden file must change by exactly the two new families, `mg_otel_export_failures_total{signal}` and `mg_otel_dropped_total{signal}`. Both must be in `METRIC_NAMES` and stay 0 with OpenTelemetry off.
- Given a stalled or stopped collector, When intervals pass, Then serving must be unaffected, the metrics failure counters must rise, and export must resume when the collector comes back.
- Given a shutdown, When `serve` stops, Then a final metrics export must reach a responsive collector.

<a id="story-53"></a>
**53. Logs over OTLP; trace ids in JSON logs when traces are on (3 pts)**
Status: Accepted ([ADR 0009](adr/0009-opentelemetry.md), Accepted by the owner on 2026-10-09). Counted in the totals.
As an operator reading logs next to traces
I want `serve`'s log events exported over OTLP and tagged with trace ids
So that a log line leads to its trace and back.
Design: [ADR 0009](adr/0009-opentelemetry.md) D7, D8 and D9.
- Given `--otlp-signals traces,logs` and a fixed set of events emitted inside and outside an rpc span, When they are logged, Then:
  - the collector must receive each record with the mapped severity, the message and only allow-listed fields;
  - the records inside the span must carry its trace and span ids;
  - the ids for each event must match in stdout JSON, the OTLP record and the exported span;
  - `trace_id` must be 32 and `span_id` 16 lowercase hex characters;
  - events outside a span must carry no id fields, and zero ids must never appear.
- Given `--log-level` or `MEMORY_GRAPH_LOG`, When it is set, Then OTLP must receive the same events as stdout.
- Given `--log-format json` with OpenTelemetry off, or with traces not among `--otlp-signals`, When events are logged inside a span, Then the JSON output must be unchanged from today. Text logs must be unchanged in every case.
- Given a sentinel query string, When the request is logged, Then the sentinel must appear in no exported log body or field.
- Given a refusing collector, When export errors happen, Then no record from the `opentelemetry*` targets may reach the collector after it recovers. This is a concrete test of the exporter never exporting its own errors.
- Given a stalled or stopped collector, When events are logged, Then logging and serving must not block, and `mg_otel_export_failures_total{signal="logs"}` and `mg_otel_dropped_total{signal="logs"}` must rise.
- Given a shutdown, When `serve` stops, Then queued log records must reach a responsive collector, and with a stalled collector `serve` must exit within the bound.

<a id="story-54"></a>
**54. OpenTelemetry compose demo, CI, overhead numbers and docs (3 pts)**
Status: Accepted ([ADR 0009](adr/0009-opentelemetry.md), Accepted by the owner on 2026-10-09). Counted in the totals.
As an operator trying OpenTelemetry for the first time
I want a working compose recipe, measured overhead and a guide
So that I can see traces, metrics and logs in minutes and know what they cost and how to run a collector with TLS.
Design: [ADR 0009](adr/0009-opentelemetry.md) D3, D4, D5, D6, D9 and D11.
- Given `deploy/compose/` with an `otel` profile, When `docker compose --profile otel up` runs, Then the collector's debug output must show traces, metrics and logs from all three nodes, started with `--otlp-endpoint http://otel-collector:4317`.
  - Either a CI step runs the profile and greps the collector's debug log for all three signals,
  - or the demo is labelled manual in `docs/deploy/compose.md` (the owner's call, ADR 0009 open question 7).
- Given the existing compose CI job and `check.sh`, When they run, Then they must pass unchanged.
- Given `rpc_bench`, When it is run by hand with OTLP on and off, Then the numbers must be recorded in `docs/spikes/rpc-overhead.md`. There is no CI assertion.
- Given `docs/guide/observability.md`, When it is read, Then it must have an OpenTelemetry section covering:
  - enabling it;
  - the flags, keys and environment variables, with their precedence, `OTEL_SDK_DISABLED` and the unsupported settings;
  - the resource attributes;
  - the signals and the trace scope;
  - the privacy allow-list;
  - the metric name mapping;
  - the failure behaviour, including the queue-full gap;
  - a collector recipe that handles TLS at the collector.
- Given the other docs, When they are read, Then `docs/deploy/compose.md`, the README, the CLAUDE.md crate notes (including exit code 7) and the release notes must point to that section, including the environment-variable configuration of the Docker image.

<a id="story-55"></a>
**55. Enum members as `Constant` symbols (5 pts)**
Status: Delivered in PRs #271, #272 and #273 ([ADR 0010](adr/0010-symbol-lookup-and-positions.md), Accepted 2026-10-09).
As a developer looking up an enum value
I want enum members indexed as symbols under their enum
So that `symbols Red` finds `Color.Red`, as it already does in Scala.
Design: [ADR 0010](adr/0010-symbol-lookup-and-positions.md) D2.
- Given `public enum Color { Red, Green, Blue }` in C#, When it is indexed, Then `symbols` must return Red, Green and Blue as kind `Constant`, lang_kind `enum_member`, each with parent `Color` and an exact span (#269's first sample, as an e2e test).
- Given each language in ADR 0010 D2, When enums with simple, initialized, trailing-comma and attributed members, nested enums and empty enums are indexed, Then each member must be a `Constant` with that language's lang_kind (`enum_member`, `enum_constant`, `enum_member`, `enumerator`, `variant`), and its span must cover exactly the member declaration.
- Given a Java enum with constant-specific class bodies, When indexed, Then the constants must be symbols and the members of their bodies must not become enum members.
- Given TypeScript `const enum` and C++ `enum class E : int`, When indexed, Then their members must be indexed like any other enum's.
- Given each changed extractor, When the PR merges, Then its version must be bumped and pinned in `extractor_versions.rs` (ASPX too, in the C# PR).
- Given the corpus, When a differential runs before and after, Then the only change must be new `Constant` members; `non_aspx_corpus_symbols_are_unchanged` must be re-pinned and a spot check added (for example a C# enum in `testdata/corpus/rebus-*`).
- Given an enum member, When `search --grain class` runs on it, Then the result must be its enum; given an enum nested in a class, Then the member's parent chain must be member, enum, class.
- Given C# `enum E : byte`, a `[Flags]` enum, and members that differ only by case (`A`, `a`), When indexed, Then every member must be its own symbol with an exact span.
- Given `[Obsolete] Red` in a C# enum, When returned (after story 56), Then its span must start at `[Obsolete]` and its name position must be `Red`.
- Given Go, F#, Haskell and Python, When indexed, Then nothing must change.

<a id="story-56"></a>
**56. Declaration (name) position on symbol hits (3 pts)**
Status: Delivered in PR #274 ([ADR 0010](adr/0010-symbol-lookup-and-positions.md), Accepted 2026-10-09).
As a developer jumping to a symbol
I want results to point at the line where its name is declared, not at the attribute above it
So that `[Serializable] class Shape` takes me to `class Shape`.
Design: [ADR 0010](adr/0010-symbol-lookup-and-positions.md) D3.
- Given `[Serializable]` on line 1 and `public class Shape { }` on line 2, When `symbols Shape` runs, Then the text output must report line 2, and `--json` must report `name_line` 2 with the span still starting at line 1 (#269's third sample, as an e2e test).
- Given a symbol with no attribute, When it is returned, Then its name position must be the name token's line and column.
- Given a symbol whose name is not found in its span, When it is returned, Then the name position must be the span start.
- Given `[Alias("Shape")]` on line 1 and `class Shape` on line 2, When returned, Then the name position must be line 2; given `[Shape] class Shape` and `[Foo(Shape)] class Shape` with the attribute on line 1, Then it must be line 1 (pinning ADR 0010 D3's documented first-match rule).
- Given an attributed symbol whose span straddles a stream checkpoint boundary, When returned, Then its name position must be correct, and `run_differential` across chunk and cache settings must give identical name positions.
- Given the conformance suite, When it runs, Then an attributed class must report its declaration line and an unchanged span, embedded and remote.
- Given the proto, When regenerated with the xtask, Then `Hit` and `SymbolHit` must gain an optional `name_pos`; when an old server leaves it unset (simulated with a test double that strips the field), the client must use the span start; `run_differential` must pass embedded against remote.
- Given MCP `find_symbols` and the symbolic search grains, When they return hits, Then they must carry the name position.
- Given readbench, When symbol lookups run before and after, Then the latency change must be recorded in the PR.
- Given the store, When this story merges, Then no stored byte may change (golden-byte tests unchanged).

<a id="story-57"></a>
**57. Case-insensitive symbol lookup by default (5 pts)**
Status: Delivered in PR #275 ([ADR 0010](adr/0010-symbol-lookup-and-positions.md), Accepted 2026-10-09).
As a developer or agent searching for a symbol
I want `widget` to find `Widget`, with an opt-out for exact case
So that I do not need to know a name's casing to find it.
Design: [ADR 0010](adr/0010-symbol-lookup-and-positions.md) D4.
- Given `public class Widget { }`, When `symbols widget` runs, Then it must return `Widget` (#269's second sample, as an e2e test); so must `WIDGET` and `WID*`; with `--exact-case`, Then `widget` must return nothing, embedded and remote.
- Given `Widget` and `widGet`, When `wid*` runs, Then both must be returned in today's order (org, repo, path, then stream position); a symbol reachable through more than one folded key must appear once (a concrete dedup and order test).
- Given a non-ASCII name such as `Größe`, When it is looked up with different non-ASCII casing, Then it must not match; ASCII letters in it must still fold; and `grö*` must match `Größe` while `GRÖ*` must not.
- Given the literal `name\*` form, When it runs with and without `exact_case`, Then it must be honoured in both modes.
- Given re-index, replace, prune, vacuum and rebuild-into-a-new-file, When each runs, Then `sym_fold` must agree with `sym_idx` (a conformance check).
- Given random names, When a property test runs exact and prefix lookups, Then `sym_fold` results must equal a brute-force fold-and-filter over `sym_idx`.
- Given a store written without `sym_fold`, When it is opened, Then `sym_fold` must be rebuilt, `derived_version_sym_fold` stamped, and queries must work; given a store with a stale stamp, or with the stamp but no `sym_fold` table, Then it must be rebuilt too; given a current store, Then the open must write nothing, and a second reopen must leave the file byte-identical.
- Given a failpoint that crashes during the rebuild, When the store is reopened, Then the rebuild must run again and queries must be correct.
- Given that no read-only open exists today, When one is ever added, Then it must follow ADR 0010 open question 2 and never return silently empty results (no test until then).
- Given a follower, and a snapshot installed from an older leader, When the store is opened, Then `sym_fold` must be healed locally; given a snapshot from a new leader installed on a follower, Then case-insensitive queries must still work afterwards; and no `LogCommand` variant may reference `sym_fold` (a cluster replication test).
- Given `export_snapshot` and compaction, When they run, Then `sym_fold` must be in their table-copy lists.
- Given `run_differential`, `run_crash_rerun_differential` and a remote conformance run, When they run, Then they must pass with the new default, and after a crash-rerun `sym_fold` must agree with `sym_idx` by query results, not only counts.
- Given the CLI `--exact-case`, the MCP `find_symbols` `exact_case` parameter and the gRPC `SymbolQuery.exact_case` field, When each is set, Then the old answers must be returned; an old server receiving the field must answer case-sensitively.
- Given the size gate, When it runs, Then it must pass with an explicit upper bound on `sym_fold`'s share of the file, and the measured size of `sym_fold` and the rebuild time on a large existing index must be recorded in the PR.
- Given the docs, When read, Then `docs/guide/querying.md`, `docs/mcp.md` and the README must describe the new default and `--exact-case`.

<a id="story-58"></a>
**58. Derived tables self-heal after an older binary writes to the file (3 pts)**
Status: Delivered in PR #286. Added 2026-10-09 (#276). Counted in the totals. Implemented: detection keys on each repo's `r\0{org}\0{repo}` catalog row, which every v2 binary inserts as 0 on every ingest and story 58's binaries write as 1, plus the `sym_idx`/`sym_fold` and `stream`/`refs`/`content_files` lengths that an older binary's removal leaves apart (`old_writer_detected` in `graph-store/src/v2.rs`).
As an operator who rolls back to an older binary and forward again
I want derived tables rebuilt when a writer that does not maintain them touched the file
So that symbol lookup never returns silently stale or missing results.
The detection must work against real pre-fix binaries, which cannot bump a stamp they do not know. So it must key on state that those binaries already change on every write, not on a new stamp that only fixed binaries maintain. The PR records which state it keys on.
- Given a file written by the current binary, When a pre-fix binary (one that maintains neither `sym_fold` nor `refs`/`content_files`) writes to it and the current binary reopens it, Then the store must detect that write and rebuild `sym_fold` and `refs`/`content_files` before answering a query.
- Given that old-writer write followed by a reopen, When `run_differential` compares it with a fresh index of the same inputs, Then every query must answer identically (a conformance case).
- Given a store that no older binary has written, When it is reopened, Then no derived table may be rebuilt and the open must write nothing (a test asserts no rebuild).
- Given a failpoint that crashes during the rebuild, When the store is reopened, Then the rebuild must run again and queries must be correct.
- Given the detection needs stored state, When it is added, Then it must follow the on-disk versioning rule in CLAUDE.md, with golden-byte tests updated.

<a id="story-59"></a>
**59. Document today's substring matching and answer #278 (1 pt)**
Status: Delivered in PR #282. Added 2026-10-09 (#278). Counted in the totals.
As a user searching for part of a name
I want the docs to say exactly what `search` and `symbols` match
So that I do not mistake "no results" for "not indexed".
- Given `docs/guide/querying.md` and the README, When read, Then they must state that `search` matches whole tokens exactly, that `symbols` takes a trailing `*` prefix, that symbol lookup is case-insensitive by default with `--exact-case` (story 57), and that there is no infix match yet (story 68).
- Given `public class Gadget { }`, When the docs show a worked example, Then `symbols Gad*` must find `Gadget` and `symbols Get` must not, and an e2e test must pin both answers.
- Given #278, When this story merges, Then the issue must have an answer that points to these docs and to story 68.

<a id="story-60"></a>
**60. Rust extractor: exact spans on shebang and raw-string files (3 pts)**
Status: Delivered in PR #287. Added 2026-10-09 (#253). Counted in the totals. Note (2026-10-10, approved by the owner): the corpus differential changed more than the fixed files. Symbols changed in 10 shebang files: the 6 named in #253, plus 4 that had passed the span check with shifted spans. Tokens changed in about 100 files with C strings or nested comments, all from the same bug classes. The criterion below that only the fixed files change is met in that sense.
As a developer indexing the Rust toolchain
I want symbols for every valid Rust file
So that `symbols` finds definitions in files that start with `#!` or contain `r#"..."#`.
The six file paths were not recorded in #253 or PR #251. The first task is to list them, from an index run's span warnings on rust-lang/rust at the pinned commit `db23a2d392783030c008a5fafbe6cb139d1f7707` (`scripts/fetch-bench-corpus.py`). They go into the PR and into #253.
- Given those six rustc files at that commit, When they are indexed, Then each must have symbols and no span warning.
- Given a reduced fixture for each case (a shebang first line, `#![attr]` on the first line, which is not a shebang, and raw strings with 0 to 3 hashes), When indexed, Then every symbol's text, byte range, line and column must match the source exactly (exact-span tests).
- Given the change, When it merges, Then the Rust extractor version must be bumped and pinned, a corpus differential must show only the fixed files changing, and the corpus test and size gate must stay green.

<a id="story-61"></a>
**61. Compact per-file sort key on the read path (5 pts)**
Status: Added 2026-10-09 (#262). Counted in the totals. The on-disk format change was approved by the owner on 2026-10-09. Preferably after story 58: both touch the derived-version self-heal on open, and 58 changes how its stamps are trusted, so landing 61 first would mean reworking its upgrade path.
As a user of a large (about 1B-token) index
I want hot-term queries to answer fast
So that common words like `return` do not stall an agent.
- Given the A5 index used in PR #261 (877M tokens, built from the bench corpus of `scripts/fetch-bench-corpus.py`) on the same host as PR #261's numbers, When `crates/graph-cli/tests/readbench.rs` runs with `MG_READBENCH_REUSE=1`, one warm-up pass discarded and then 5 measured runs, Then:
  - the hot-term warm `p95 ms` (the median of the 5 runs' p95) must be below 60 ms (from 93 ms);
  - the `ctx ms/q` column of the search phase-timer row (`search_ctx_nanos`; 11.9 ms after PR #261) must drop by at least 50%.
- Given the 924 searches of PR #261, When they run with `--json` before and after, Then the output must be byte-identical.
- Given the new side table, When it ships, Then it must follow the versioning rule, with golden-byte tests, and an older file must be upgraded on open. The PR must record which option it took: a `derived_version` (preferred) or a `V2_SCHEMA_VERSION` bump.
- Given `run_differential` and `run_crash_rerun_differential`, When they run, Then they must pass with the side table.
- Given the size gate, When it runs, Then it must stay at or below 15x and 105 B/token.
- Given the result, When it is measured, Then it must be recorded against GPU trigger 2 in `docs/spikes/gpu-acceleration.md`.

<a id="story-62"></a>
**62. Membership changes keep in-flight tracking until Raft answers (3 pts)**
Status: Added 2026-10-09 (#260). Counted in the totals.
As an operator transferring leadership during a membership change
I want the drain to wait for every appended entry
So that the target never has a shorter log.
- Given `add_learner` and `change_membership` in `crates/graph-server/src/raft/node.rs`, When the caller stops waiting (cancellation, or a quorum-loss `NoLeader`), Then the in-flight count must be held until openraft answers the entry, as `propose` does since #259.
- Given one partition test per path, modelled on `an_abandoned_write_stays_in_flight_until_raft_answers_it`, When an `add_learner` (first test) or a `change_membership` (second test) is abandoned mid-way and a leader transfer follows, Then the drain must wait for it (ClusterTestbed).
- Given the transfer drain, When its bound is set, Then it must be a fixed multiple of the configured `election_timeout_max` (the multiple is recorded in the PR) instead of a fixed 10 s, and a test must assert that the bound changes when the election config changes.
- Given 1-CPU starvation with 8 CPU burners (the #260 setup), When `transfer_leader_moves_leadership_five_nodes`, `remove_needs_a_quorum_of_the_old_voter_set_too` and `a_transfer_waits_for_a_write_in_flight` are run, Then each failure must be reproduced with the state dump and classified as test timing or a real bug, and after the fix each must pass 50 consecutive runs (documented in the PR).

<a id="story-63"></a>
**63. Rust parse stack counted in the ingest memory budget (3 pts)**
Status: Added 2026-10-09 (#254). Counted in the totals. Preferably after story 60: both change the Rust extractor and its pinned version, so landing them in order avoids two version bumps racing.
As an operator indexing untrusted repos
I want the Rust parse stack counted against `--memory`
So that parallel adversarial files cannot exceed the budget.
- Given `--memory`, When a parse job needs a big stack, Then admission must reserve its planned touched stack before parsing (or limit big-stack parses to one at a time).
- Given two near-cap adversarial files made by a new fixture generator, `near_cap_rust_source` (added by this story, beside the `scanner_robustness` fixtures), indexed in parallel with `--memory 1GiB`, When indexing runs, Then the budget assertion `parse_stack_reservations_stay_within_memory_budget` must pass: the reserved parse stacks plus the other budgeted memory must never exceed 1 GiB. The PR must report the measured peak private bytes (it was 1.69 GB before the fix, per #254).
- Given a small and a large `--memory` and different `--jobs`, When `run_differential` compares the resulting stores, Then every query must answer identically.
- Given the docs, When read, Then they must state that in-place parses (bound under 256 KiB) need 256 KiB of free stack on the caller's thread, and `stack.rs` must explain why not tracking angle brackets is sound.

<a id="story-64"></a>
**64. TypeScript decorated methods are found (2 pts)**
Status: Delivered in PR #283. Added 2026-10-09 (#266). Counted in the totals.
As a TypeScript developer
I want decorated methods reported wherever they appear in a class body
So that `symbols` finds them after a field without a semicolon or after a method body.
- Given the reduced fixtures `class I { f = 1\n  @dec m() {} }` and `class C { @dec() m(...) {} @log n() {} }`, When indexed, Then `m` and `n` must be methods with exact spans (exact-span tests), and no two symbols may overlap in either fixture.
- Given `class I { f = 1\n  @dec m() {} }`, When indexed, Then `f` must stay a field that ends before `@dec` (a regression for the #265 fix).
- Given the change, When it merges, Then the TS extractor version must be bumped and pinned, the fuzz no-overlap test must stay green, and a corpus differential must show only new methods.

<a id="story-65"></a>
**65. asm extractor: exact spans on libunwind `UnwindRegistersSave.S` (2 pts)**
Status: Added 2026-10-09 (#252). Counted in the totals.
As a C and assembly user
I want `UnwindRegistersSave.S` indexed with symbols
So that its functions are found by `symbols`.
- Given `libunwind/src/UnwindRegistersSave.S` from llvm/llvm-project at the pinned commit `75cc30c7b35ce8d5ddd311b67872ed498997d58f` (`scripts/fetch-bench-corpus.py`), When indexed, Then it must have symbols and no span warning.
- Given a reduced fixture with the symbols that failed `validate_spans`, When indexed, Then every symbol must have an exact span, with no partial overlap (proper nesting allowed) (exact-span tests).
- Given the change, When it merges, Then the asm extractor version must be bumped and pinned, and a corpus differential must show only the fixed file changing.

<a id="story-66"></a>
**66. COBOL digit-led `SECTION` names (1 pt)**
Status: Delivered in PR #284. Added 2026-10-09 (#268). Counted in the totals.
As a COBOL user
I want digit-led section names recognised
So that `100A SECTION.` is a section symbol.
- Given reduced fixtures with `100A SECTION.` and `100A SECTION .` in Area A at sentence start, When indexed, Then each must emit a section symbol with an exact span.
- Given the same text outside Area A or not at sentence start, When indexed, Then no section symbol may be emitted (negative tests).
- Given the change, When it merges, Then the COBOL extractor version must be bumped and pinned. A corpus differential must then show changes only in COBOL files with digit-led section names without a hyphen, or no change if the corpus has none.

<a id="story-67"></a>
**67. Leader or apply-time disk full answers RESOURCE_EXHAUSTED without stopping the node (5 pts)**
Status: Added 2026-10-09 (#115). Counted in the totals.
As an operator
I want a full disk during append or apply to refuse the write without stopping the node
So that the cluster keeps serving and recovers when space is freed.
- Given a ClusterTestbed disk-full failpoint that fails the leader's Raft log append with an I/O error after the pre-propose disk check passed, When a write runs, Then:
  - the client must get RESOURCE_EXHAUSTED;
  - the write must stay unacknowledged and the store marker unchanged;
  - the node must not exit or restart, and must keep serving reads.
- Given a ClusterTestbed disk-full failpoint that fails the state-machine apply with an I/O error (on the leader, and separately on a follower), When a write commits, Then the client must get RESOURCE_EXHAUSTED from a failing leader, the store marker must be unchanged, and the node must not exit or restart. A failing follower must catch up after space is freed.
- Given space is freed (the failpoint cleared), When the client retries, Then the write must succeed and be applied exactly once on every node.
- Given the CLI, When a write fails with RESOURCE_EXHAUSTED, Then it must exit with code 1. RESOURCE_EXHAUSTED maps to `StoreError::Storage`, which `exit_code` in `crates/graph-cli/src/target.rs` does not single out, and the message must say the disk is full.
- Given the design, When it is chosen (non-fatal apply errors in openraft 0.9, or a pre-apply space reservation), Then it must be recorded in a dated note in [ADR 0004](adr/0004-client-server-and-replication.md).

<a id="story-68"></a>
**68. Infix (substring) symbol search (5 pts)**
Status: Added 2026-10-09 (#278). Counted in the totals. Design: [ADR 0011](adr/0011-infix-search.md), Accepted by the owner on 2026-10-10; it sets the infix p95 threshold at 50 ms. The ADR's hard prerequisite, story 58, applies to step 2 (the `sym_tri` side table) only and was delivered in #286. Preferably after story 61: both change the read path on the same bench, and measuring infix p95 on top of 61's numbers keeps the two effects apart.
As a developer or agent who remembers part of a name
I want `symbols` to match inside a name
So that `get` can find `widgetGetter`.
Resolved by ADR 0011 (originally an open question): the developer recommended symbol names only, scanning the interned name dictionary first and adding a trigram side table over distinct names only if p95 is above about 50 ms. A token-level n-gram index would break the 105 B/token size gate.
- Given ADR 0011's syntax, When an infix query runs, Then it must return every symbol whose name contains the substring, with case folding as in story 57, embedded and remote (a `run_all` case for infix semantics).
- Given random names and queries, When a property test runs, Then infix results must equal a brute-force substring filter over all symbol names (a brute-force oracle proptest).
- Given the query parser, When a fuzz test feeds it arbitrary input, Then it must never panic and must reject bad input with an error.
- Given the size gate and readbench, When they run, Then the size gate must stay at or below 15x and 105 B/token, and infix p95 must be within ADR 0011's threshold and recorded in the PR.

<a id="story-69"></a>
**69. Authentication for `serve` (8 pts)**
Status: Added 2026-10-09 (#105). Counted in the totals. Design: [ADR 0012](adr/0012-authentication.md), Accepted by the owner on 2026-10-10. Its concrete criteria replace the placeholder criteria below when the story starts. Unblocks story 33.
As an operator running a shared server
I want clients to authenticate before they read or write
So that only trusted callers can reach the data.
Resolved by ADR 0012 (originally an open question): token auth through a tonic interceptor with constant-time comparison (`subtle`), and no TLS in v1, because rustls's default providers are aws-lc-rs and ring, which are deny-listed. Per-org and per-repo write permissions (raised in #105) are out of scope for v1; ADR 0012 may plan them as a follow-up.
- Given auth on, When `RemoteStore` conformance runs with valid credentials, Then it must pass; without credentials or with wrong ones, Then every call must fail with UNAUTHENTICATED and return no data.
- Given auth off, When `RemoteStore` conformance runs without credentials, Then it must pass unchanged.
- Given a write sent to a follower, When it is forwarded to the leader, Then the forward must authenticate, and a forwarded write without valid credentials must be refused (the forwarded-write path).
- Given Raft peer traffic, the health service and MCP over HTTP, When auth is on, Then each must behave as ADR 0012 decides.
- Given logs captured at trace level during the authenticated conformance run, When they are searched, Then the token must not appear in them.
- Given the pure-Rust gate, When the story merges, Then `check-no-c-deps.py` must pass and `cargo tree -i ring` and `cargo tree -i aws-lc-sys` must print nothing.

### Rationale
- **Order:** the three P1 items with no dependencies (both spikes and the CI gate) come first because they fix the parser, storage and pure-Rust constraints. The fallback tokenizer is in the MVP because it proves the any-language claim without any language knowledge.
- **Language-agnostic by construction:** the schema uses a language tag plus a generic kind vocabulary with an optional language-specific kind string. Only extractors know about a language, and story 16 checks this by requiring zero schema, storage or query changes.
- **Two paths for a new language:** a compiled extractor (story 9 and 16 pattern) or externally supplied NDJSON (story 13). The WASM spike (story 19) is optional and comes last.
- **Sizing:** no story is over 8 points. Story 9 was the only 8 until stories 20-22 (ADR 0004) joined it; 9 is split-ready by symbol kind, 20 by service (Store, Write, Admin), 21 and 22 by their sub-bullets, if any overruns.

### Assumptions
- You are working solo, so no sprint mapping is given. Take a velocity baseline after the MVP slice (0.8 x available time).
- The library is the primary product and the CLI is a thin wrapper.
- Token means a leaf syntax element. The fallback tokenizer approximates this lexically, so quality varies by language.
- Language is detected from the file extension, with an explicit override available.
- Org and repo are user-supplied labels.
- `syn` and `ruff_python_parser` are the leading candidates for Rust and Python, pending story 1.
- `redb` and `fjall` are the leading storage candidates, pending story 2.

### Open Questions (human decision needed)
1. Are punctuation, whitespace and comments stored as tokens? This changes storage size by roughly 2–5x. Story 6 currently stores comments and punctuation, but not whitespace.
2. Is org and repo ever derived from a git remote, or always supplied by the user?
3. Is Python the second built-in language, or would you prefer TypeScript or Go? Pure-Rust parsers for those are less mature.
4. Should the fixed generic kind vocabulary in story 4 be extended, for example with `enum`, `interface` or `field`?
5. Is an MCP or server interface wanted soon? Story 17 is the seam. *Server: answered 2026-09-28 by [ADR 0004](adr/0004-client-server-and-replication.md) (stories 20-25); MCP: answered 2026-09-29 by [ADR 0005](adr/0005-mcp.md) (Accepted; stories 31-34): read-only MCP over stdio (`memory-graph mcp`) and opt-in HTTP inside `serve`.*
6. What are your scale targets (largest repo, number of repos)? Story 18 targets are placeholders.
7. Will the crate be published, and under which license? That constrains dependency licenses. *Resolved 2026-09-29: the owner chose Apache-2.0 (issue #96, PR #139).*

### Next Steps
1. Answer open questions 1, 2 and 4, which affect story 4 (schema) before it starts.
2. Run stories 1, 2 and 3 in parallel, then re-estimate stories 4–9 and 16.
3. Create the Cargo workspace (library plus CLI) with story 3 or 4, since no code exists yet.
4. Take a velocity baseline after the MVP slice, then plan the later slices.

**downstream_ready:** true for stories 1, 2 and 3 (fully defined, unblocked). Stories 4–19 have provisional estimates until the spikes close, and open questions 1, 2 and 4 should be answered before story 4.
