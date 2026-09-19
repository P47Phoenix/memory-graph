## Epic: Language-Agnostic Code Memory Graph (pure Rust)

**Epic Goal:** An AI agent or developer stores many codebases, in any programming language, in one embedded graph database. The hierarchy is org → repo → file → symbol → syntax token. They can then search and traverse it across repos and languages. Built-in extractors add structure for some languages. Any other language still gets File and Token nodes through a generic tokenizer, and agents can supply structure for any language through a language-neutral ingest format.

**Success Metrics:**
- (a) A repo in a language with no built-in extractor is indexed to File and Token nodes without code changes.
- (b) At least 3 repos across at least 2 built-in languages plus 1 fallback language sit in one database, and a single query returns cross-repo, cross-language results filtered by language.
- (c) Adding a built-in language touches only extractor code, with 0 schema, storage or query changes.
- (d) `cargo tree` shows 0 `-sys` or C/C++ build dependencies, enforced in CI.
- (e) p95 search latency is under 100 ms on 1,000+ files. This target is a placeholder until story 18.
- (f) Re-indexing an unchanged repo re-parses 0 files.

**Out of Scope:** Network server or MCP interface, auth, semantic or embedding search, cross-file reference resolution (call graphs, type resolution), GUI, distributed storage, git history, a query language (fixed query functions only), and bundled extractors beyond Rust and Python in this epic.

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

Total: 19 stories, 89 pts (average about 4.7).

### MVP Slice
Stories 1–8 (33 pts). Any file in any language goes into a persisted graph as File and Token nodes under org/repo, and is searchable by token text with a language filter, through the library and the CLI. The C-dependency gate is active from the start.

Rationale: this proves the "any language" claim on day one, since the fallback tokenizer needs no language knowledge. It also retires the parser and storage unknowns first and locks the pure-Rust rule in CI before dependencies pile up.

Later slices:
- **Structure:** stories 9–12
- **Agent integration:** stories 13 and 14
- **Operational and reach:** stories 15–19

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
- Given a dependency whose crate name ends in `-sys` or that has a `build.rs` compiling C or C++, When CI runs the gate, Then it must fail and name the offending crate.
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
I want the graph stored in a database directory I choose
So that I index once and query in later sessions.
- Given a graph written with `--db ./g`, When the process exits and `./g` is reopened, Then all nodes and edges must be present and unchanged.
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
- Given `--grain token|symbol|file|repo|org` (default `token`), When searched, Then results are the distinct nodes of that grain containing a match, each with its containment path and a hit count; `--symbol-kind method` restricts the symbol grain to that kind.
- Given a match in a file with no enclosing symbol of the requested kind, When searched at symbol grain, Then it rolls up to its File and is flagged `no_symbols`.

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

**16. Python extractor via the Extractor trait (5 pts)**
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

### Rationale
- **Order:** the three P1 items with no dependencies (both spikes and the CI gate) come first because they fix the parser, storage and pure-Rust constraints. The fallback tokenizer is in the MVP because it proves the any-language claim without any language knowledge.
- **Language-agnostic by construction:** the schema uses a language tag plus a generic kind vocabulary with an optional language-specific kind string. Only extractors know about a language, and story 16 checks this by requiring zero schema, storage or query changes.
- **Two paths for a new language:** a compiled extractor (story 9 and 16 pattern) or externally supplied NDJSON (story 13). The WASM spike (story 19) is optional and comes last.
- **Sizing:** no story is over 8 points, and story 9 is the only 8. It is split-ready by symbol kind if it overruns.

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
5. Is an MCP or server interface wanted soon? Story 17 is the seam.
6. What are your scale targets (largest repo, number of repos)? Story 18 targets are placeholders.
7. Will the crate be published, and under which license? That constrains dependency licenses.

### Next Steps
1. Answer open questions 1, 2 and 4, which affect story 4 (schema) before it starts.
2. Run stories 1, 2 and 3 in parallel, then re-estimate stories 4–9 and 16.
3. Create the Cargo workspace (library plus CLI) with story 3 or 4, since no code exists yet.
4. Take a velocity baseline after the MVP slice, then plan the later slices.

**downstream_ready:** true for stories 1, 2 and 3 (fully defined, unblocked). Stories 4–19 have provisional estimates until the spikes close, and open questions 1, 2 and 4 should be answered before story 4.
