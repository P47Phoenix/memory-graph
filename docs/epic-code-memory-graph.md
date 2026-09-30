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
| 33 | Opt-in MCP write tools (**Deferred** until auth, #105) | Medium | 3 | P4 | 32, #105 |
| 34 | MCP real-client e2e and hardening | Medium | 3 | P3 | 31, 32 |
| 35 | Snapshot backups: `file://` sink and verified restore | High | 5 | P2 | 22 |
| 36 | S3 client over plain HTTP (SigV4, multipart) | High | 8 | P2 | 35 |
| 37 | Restore from `s3://` and `latest`, `cluster snapshot --upload`, `cluster backups` | Medium | 3 | P2 | 36 |
| 38 | MinIO e2e CI job and backup docs | Medium | 3 | P3 | 37 |
| 39 | Spike: native HTTPS for backups behind `backup-tls` (**Deferred**, shared with #104) | Low | 5 | P4 | 36, #104 |

Total: 39 stories, 203 pts (average about 5.2); 195 pts excluding the deferred stories 33 and 39. Stories 20-25 (37 pts) were added on 2026-09-28 by [ADR 0004](adr/0004-client-server-and-replication.md), accepted by the user the same day. Stories 26-30 (37 pts) were added on 2026-09-29 at the user's request: symbols for 17 more languages. Stories 31-39 (40 pts; 33 and 39 deferred) were added on 2026-09-29 at the owner's request by [ADR 0005](adr/0005-mcp.md) (MCP) and [ADR 0006](adr/0006-snapshots-object-storage.md) (snapshots to object storage), both Accepted by the owner on 2026-09-29.

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

*Status: the skip-unchanged part is implemented.* File nodes carry a fingerprint (SHA-256 of the content + lowercased language + `Extractor::version()` (includes the tokenizer version) + store index format version); an identical fingerprint skips the file and is reported as `unchanged` (`index`, `index-file`, `--json`, `IngestStats`, `index_batch`). `--reindex` re-indexes regardless (`--force` is only the `--prune` empty-run override). The tokenizer version is part of the extractor versions. Known issue (pre-existing, out of scope): `index-file` stores its path as given rather than repo-relative. Pre-fingerprint files re-index once. Deleted-file removal is `--prune` (story 10).

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
Status: Delivered in PR #121; see the README's Observability section and [docs/deploy/](deploy/kubernetes.md).
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
Status: **Deferred** until authentication (#105); the owner decided on 2026-09-29 that an unauthenticated endpoint stays read-only.
As an AI-agent integrator
I want `index_path` and `prune` tools
So that an assistant can keep the graph current without a separate CLI call.
Design: [ADR 0005](adr/0005-mcp.md) D2 (Accepted).
- Given no `--mcp-allow-writes`, When a client lists tools, Then the write tools must not appear.
- Given `--mcp-index-root`, When `index_path` names a path outside it after canonicalisation, Then the call must be refused.
- Given `prune` with no `dry_run` argument, When called, Then it must run as a dry run.

**34. MCP real-client e2e and hardening (3 pts)**
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

**38. MinIO e2e CI job and backup docs (3 pts)**
Status: Delivered in PR #PRNUM (CI job `s3-e2e`, `crates/graph-cli/tests/s3_e2e.rs`; docs in `docs/deploy/data-dir.md` and the README). With it stories 35-38 are delivered; 39 stays deferred with #104.
As a database operator
I want backups tested against a real S3-compatible server and documented
So that I can set them up with confidence.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) testing (Accepted).
- Given a MinIO service container over plain HTTP on Linux, When the CI job runs upload, retention and restore, Then it must be green.
- Given the docs, When read, Then they must cover the sidecar TLS recipe, bucket lifecycle rules and a minimal IAM policy, and the README walkthrough must work.

**39. Spike: native HTTPS for backups behind `backup-tls` (5 pts)**
Status: **Deferred**, shared with #104 (TLS on the wire); it waits for a pure-Rust rustls crypto provider that passes the gate.
As a database operator
I want backups sent to AWS over HTTPS without a sidecar
So that the deployment has one less moving part.
Design: [ADR 0006](adr/0006-snapshots-object-storage.md) E5 (Accepted).
- Given candidate providers (rustls-rustcrypto, graviola), When evaluated, Then the spike must record whether one passes `scripts/check-no-c-deps.py` and works against AWS S3.
- Given a passing provider, When the `backup-tls` feature is enabled, Then `https://` endpoints must be accepted, and the default build must stay unchanged.

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
