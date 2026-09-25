# memory-graph

An embedded graph database for source code. It indexes one or more repositories into a single file and answers questions such as "where is the token `Node` used?" or "which methods start with `parse`?" with exact byte, line and column spans.

- **Language-agnostic.** Every file of every language is tokenized with exact spans. Languages with an extractor (Rust, C#, JavaScript, HTML, ASP.NET markup) additionally get symbols: functions, types, methods and so on.
- **One file, no server.** The database is a single [redb](https://github.com/cberner/redb) file, about 8-10x the size of the indexed source.
- **Pure Rust.** No C dependencies (enforced in CI), so it builds anywhere Rust does and ships as a 7 MB static container image.
- **Incremental.** Unchanged files are skipped on re-index; deleted files can be pruned.

The graph is `Org → Repo → File → Symbol → Token`.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Indexing](#indexing)
- [Querying](#querying)
- [Docker](#docker)
- [Languages](#languages)
- [Storage](#storage)
- [Using it as a library](#using-it-as-a-library)
- [Development](#development)
- [Further reading](#further-reading)

## Install

**From source** (a stable Rust toolchain):

```sh
cargo build --release
# the binary is target/release/memory-graph; put it on your PATH or call it by path
```

**Docker** (no toolchain needed; see [Docker](#docker) for volumes and tags):

```sh
docker pull ghcr.io/p47phoenix/memory-graph:main
```

## Quick start

Index a directory as a repo, then query it. Every command takes `--db <file>` (default `./graph.redb`); the file is created on the first `index`.

```sh
memory-graph --db ./g index --org acme --repo api ./api        # whole directory; honors .gitignore, skips binaries
memory-graph --db ./g describe                                  # what got indexed: languages and symbol kinds per repo
memory-graph --db ./g search foo --language rust                # every token `foo` in Rust files
memory-graph --db ./g symbols 'pars*' --kind method --json      # method definitions whose name starts with `pars`
```

What that looks like on this repository's own `crates/graph-core/src`:

```text
$ memory-graph --db ./g index --org demo --repo graph-core crates/graph-core/src
indexed demo/graph-core: files=6 unchanged=0 symbols=277 tokens=11737 skipped=0 failed=0 pruned=0 elapsed=132ms
  rust: 6

$ memory-graph --db ./g search Node --language rust --limit 2
demo/graph-core/schema.rs:140:12    rust    Node    hits=1
demo/graph-core/schema.rs:202:17    rust    tests::node_round_trip::n    hits=1

$ memory-graph --db ./g describe
demo/graph-core: 6 files
  rust: 6 files, 277 symbols, 11737 tokens
    constant/const: 11
    function/fn: 64
    method/fn: 34
    ...
```

Run the same `index` again and every file is reported as `unchanged`: nothing is rewritten.

## Commands

| Command | What it does |
|---|---|
| `index --org O --repo R <DIR>` | Index a directory as one repo. Honors `.gitignore`, skips binary files and files over `--max-file-size` (8 MiB). |
| `index-file --org O --repo R <PATH>` | Index a single file (`--language` overrides detection). Re-indexing replaces it. |
| `search <TEXT>` | Find tokens by exact text. Filters: `--language`, `--org`, `--repo`, `--kind` (token class), `--grain`, `--symbol-kind`. |
| `symbols <PATTERN>` | Find symbol definitions by name: exact, `prefix*`, or `*` for everything. Filters: `--kind` (symbol kind), `--language`, `--org`, `--repo`, `--file`. |
| `describe` | Per repo: files, languages, symbols, tokens and the symbol kinds present (`--org`/`--repo` to narrow). |
| `sysinfo` | What `index` sizes itself from on this machine: CPUs, memory and its source, the starting budget, free disk on the database's volume. |
| `vacuum [--compact]` | Drop dictionary terms no file uses; `--compact` rebuilds the file to give the space back. |
| `export [--out FILE]` | Dump every node (org, repo, file, symbol, token, with spans) as newline-delimited JSON. An escape hatch; there is no importer yet. |

Every query command has `--json` (an object on stdout) and `--limit`/`--offset` for paging; results are ordered by org, repo, file, position.

Global options: `--db <file>` (default `./graph.redb`), `--chunk-bytes` (commit a transaction every this many source bytes, default 64 MiB) and `--cache-bytes` (redb's cache, default 1 GiB). Run `memory-graph <command> --help` for the full list.

## Indexing

### Incremental by default

Each file's fingerprint is the SHA-256 of its bytes plus the language, the extractor and tokenizer versions and the store format version. A file whose fingerprint is already stored is left untouched and counted as `unchanged` (a subset of `files`; it adds nothing to `symbols`/`tokens`). Changed content, a language override, or a new extractor version re-indexes the file fully.

| Flag | Effect |
|---|---|
| `--reindex` | Re-index every file even if unchanged (`index-file` has it too). |
| `--prune` | Remove this repo's files that this run did not index (deleted, renamed, newly ignored). Only files last written by a directory run are considered. Skipped when some paths were unreadable, and refused when the run indexed nothing, unless `--force`. |
| `--force` | With `--prune`: allow removals even when nothing was indexed. `--reindex --prune` still refuses that. |
| `--max-file-size N` | Skip files larger than N bytes (lockfiles, minified bundles, dumps). |

Known limitation: `index-file` stores the path as given, so it only shares a File node with a directory `index` when called with the same repo-relative path.

### Failures stay per file

If a file fails span validation (an extractor or tokenizer bug), only that file is left out: the rest of the batch is stored, `index` prints `failed: <path>: <reason>`, skips `--prune` with a warning and exits non-zero at the end (`failed=N` in the summary, `failed` and `failed_files` in `--json`). Storage errors still abort the batch. A panicking extractor stops the run naming the file; committed transactions stay stored.

### Sizing: threads, memory, disk

`index` streams the directory through three concurrent stages, *walk → parse → commit*, and sizes itself from the machine. Nothing needs tuning on a normal box; these are the knobs.

- **Threads.** One parse thread per CPU but one (the writer). `--jobs N` overrides. Files are committed in walk order by a single writer, so the stored content is the same for any thread count.
- **Memory.** The source bytes in flight (read but not yet committed) are capped by a budget derived from free RAM, re-sampled every quarter second: 20% of RAM is always left to the OS, the process may grow into 70% of what is free above that, and that headroom is divided by the measured growth per source byte (about 25x). Under pressure (free RAM below the reserve, the process past 60% of RAM, or Linux PSI stalls) the budget halves per sample and only grows again once 30% of RAM is free. `--memory 50%` changes the share; `--memory 2G` fixes the budget in source bytes; `MEMORY_GRAPH_MEMORY` sets the default. Memory is read from `/proc/meminfo` on Linux (else `sysinfo(2)`), capped by the cgroup limit when one is set, `host_statistics64` on macOS and `GlobalMemoryStatusEx` on Windows. If no probe works the budget is a fixed 512M and the memory line says why.
- **Disk.** The database is about 8x the source. `index` keeps a reserve free on the database's volume (5% of it, between 2G and 32G; `--min-free-disk 4G` or `5%`), refuses to start below it, and stops cleanly if free space or the projected final size would go below it: what was committed stays, the database is consistent, and a rerun resumes. A real "No space left on device" is reported the same way. `--no-disk-check` reports but never stops.
- **Reproducible files.** `--deterministic` commits fixed batches so the database file is byte-for-byte the same on any machine (slower when the writer is the bottleneck).

### Watching a run

- A live view on stderr (only when it is a terminal) shows each stage's work, what it waits for (`blocked: memory budget full`), memory in flight, disk, and the bottleneck. On by default without `--json`; `--progress` forces it with `--json`; `--no-progress` turns it off. Stdout carries only the final summary.
- `--stats` prints how busy each stage was and names the bottleneck (`writer-bound`), the memory source and a `disk:` line; with `--json` it is a `stats` object.
- `--trace run.json` writes a Chrome/Perfetto trace with one span per file per stage.
- `memory-graph sysinfo` (`--json` for an object) prints what the probes see; paste it into a report when sizing looks wrong.

## Querying

- `search` finds **tokens** by exact text; `symbols` finds **definitions** by name. `--kind` means a different thing on each: a token class on `search` (identifier, keyword, literal, operator, punctuation, comment, other) and a symbol kind on `symbols` (generic: module, type, function, method, variable, constant, other; or language-specific: struct, trait, impl, ...).
- `search --grain token|symbol|file|repo|org` rolls hits up to that level; `--grain symbol --symbol-kind method` keeps only methods.
- `--language`, `--kind` and `--symbol-kind` values are validated against what `describe` reports, so a typo is an error, not an empty result. Language names are case-insensitive.
- `symbols` patterns: `name` (exact), `prefix*`, `*` (all), `name\*` (a literal `*`). `**` is rejected as ambiguous.
- Page with `--limit N --offset M`. `--json` prints `{"query", "results": [...]}` (and `"grain"` for `search`).

## Docker

The image `ghcr.io/p47phoenix/memory-graph` is a static binary on an empty base (`FROM scratch`): no shell, 7 MB, `linux/amd64` and `linux/arm64`. It runs as user `65532`, its working directory is `/data`, and the default `--db ./graph.redb` therefore lands on the `/data` volume.

### Run

Mount the source read-only and keep the database on a named volume:

```sh
docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph index --org acme --repo api /src
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph describe
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph search foo --json
```

A fresh named volume is writable as is. To keep the database in a host directory instead, bind-mount it and run as its owner:

```sh
docker run --rm -v "$PWD/db:/data" --user "$(id -u):$(id -g)" ghcr.io/p47phoenix/memory-graph describe
```

A memory limit on the container sizes the budget for the container, not the host:

```sh
docker run --rm --memory=512m ghcr.io/p47phoenix/memory-graph sysinfo
```

(On Docker Desktop the memory source reads `sysinfo(2)` because `/proc/meminfo` is unreadable to non-root there; on a Linux host it reads `/proc/meminfo`.)

### Tags and releases

| Tag | Points at |
|---|---|
| `main` | The latest push to `main` (moves). |
| `sha-<short commit>` | That commit. |
| `0.1.0`, `0.1` | A release tag `v0.1.0`. |
| `latest` | The newest release. Never a prerelease (`v0.2.0-rc1`); absent until the first release. |

A release is cut by pushing a tag that matches the workspace version in `Cargo.toml` (bumped by hand; the workflow refuses a tag that does not match):

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The workflow (`.github/workflows/docker.yml`) builds and smoke-tests the image on every pull request and manual run but only pushes on `main` and `v*` tags.

### Build locally

```sh
docker build -t memory-graph .                                  # host architecture
docker buildx build --platform linux/arm64 -t memory-graph .    # cross-compiled; no QEMU needed
```

## Languages

Languages are detected per file from the extension, the filename (`Makefile`) or a `#!` line, so a polyglot repo needs no flags. Every file gets tokens from the generic tokenizer; these languages also get symbols:

| Language | Extensions | Extractor |
|---|---|---|
| Rust | `rs` | `syn` (full parse) |
| C# | `cs`, `csx` | token-stream scanner |
| JavaScript | `js`, `mjs`, `cjs`, `jsx` | token-stream scanner |
| HTML | `html`, `htm`, `xhtml` | element scanner |
| ASP.NET markup | `aspx`, `ascx`, `master` | HTML scanner plus directives |

Everything else (Python, Go, SQL, YAML, ...) is tokenized with exact spans and no symbols. Language names are lowercased, a UTF-8 BOM is ignored, and paths are normalized (`./a.rs` = `a.rs`).

Tokenizer dialects: the generic tokenizer treats `r"a\"b"` as an identifier `r` and a string with Python-style escapes. The Rust extractor uses the `rust_literals` dialect, where raw strings (`r"..."`, `r#"..."#`, `br#"..."#`) and byte literals (`b"..."`, `b'x'`) are single literal tokens and an unterminated raw string runs to end of input.

To add a language, implement the `Extractor` trait in its own crate: see [docs/adding-a-language.md](docs/adding-a-language.md) and [examples/toy-extractor](examples/toy-extractor).

## Storage

- **Format.** One redb file holding an interned dictionary, one compact stream per file with sparse checkpoints, and count postings ([ADR 0003](docs/adr/0003-data-model.md)). About 8-10x the source, 55 bytes per token on the test corpus; `scripts/measure-size.py` prints the full table and `crates/graph-cli/tests/size_gate.rs` enforces the ratio in CI.
- **Growth and reclaiming space.** Unchanged files add nothing on a rerun; `--reindex` can double the file until `vacuum --compact`, since redb reuses freed pages but never shrinks the file. `vacuum` frees dictionary terms after churn; `--compact` rebuilds the file.
- **Catalog.** `describe` and filter validation read a small counter catalog kept in step with every write, so they cost O(repos), not O(tokens). A database written before the catalog existed is backfilled on first open (the file must be writable), after which older builds refuse it with a schema-version error.
- **Versioned on disk.** Any change to the stored bytes bumps the schema version; a file from another version is refused without being written to.
- **The v1 format is retired (2026-09-25).** The original per-node layout cost about 525 bytes per token (a fresh index of a 10 GB tree reached 420 GB). Opening a v1 file fails with a message naming its schema version and leaves it untouched. Re-index from source into a new file, or convert it with the last v1-capable release, git tag `v1-last`, using `memory-graph migrate <new.redb>`. `--backend v2` is accepted as a no-op, `--backend v1` is an error; `--v2-chunk-bytes`/`--v2-cache-bytes` are now `--chunk-bytes`/`--cache-bytes` (old spellings still work).

## Using it as a library

The CLI depends on the object-safe `graph_store::Store` / `StoreRead` traits, not on redb. `open_store(path, extractors)` returns a `Box<dyn Store>` over `V2Store`, the one storage format; bring the traits into scope (`use graph_store::{Store, StoreRead}`) to call methods on it. Indexing is split into `Store::prepare` (pure, callable from many threads) and `Store::index_prepared` (the commit); `Store::index_batch` reports a file's `InvalidSpan` in that file's result slot and returns `Err` only for storage errors. `Extractor` requires `Send + Sync`. `graph_store::conformance::run_all` is a reusable test suite for any `Store` implementation. See the [architecture diagrams](docs/architecture-diagrams.md).

## Development

```sh
cargo fmt --all --check                                  # formatting (CI)
cargo clippy --workspace --all-targets -- -D warnings     # lints (CI, zero warnings)
cargo test --workspace                                    # unit + integration tests
cargo test -p graph-cli --test corpus                      # public-repo corpus: exact spans, cross-repo links
cargo test -p graph-cli --test e2e                          # CLI end-to-end
python3 scripts/test_gate.py                               # CI's extra gate
python3 scripts/check-no-c-deps.py                          # pure-Rust gate: fails on any C build script
docker build -t memory-graph .                              # the container image
```

CI runs all of the above on every push and pull request, plus a real disk-full run on tmpfs, the machine probes on ubuntu, macOS and Windows, and the Docker image's smoke test (`docs/testing.md`).

### Test corpus

`testdata/corpus/` vendors real public code (MIT/Apache-2.0 only, pinned commits, see each folder's `UPSTREAM.md`) as distinct repos grouped into applications by `corpus.json`:

- **messaging**: `rebus` + `rebus-rabbitmq` + `rebus-sqlserver` (transports implementing Rebus)
- **conduit**: `conduit-ui` (Angular) → `conduit-api` (Spring) → `conduit-data-access` (MyBatis) → `conduit-sql`
- **rust-library**: `anyhow`

`cargo test -p graph-cli --test corpus` checks the manifest (public, licensed), that every cross-repo link resolves, that every token of every file is parsed with exact spans, and that the graph stores exactly those tokens. Re-vendor with `scripts/vendor-corpus.py`.

## Further reading

- [docs/README.md](docs/README.md): the documentation index (glossary, epic, ADRs, spikes, learnings).
- [ADR 0001](docs/adr/0001-storage.md) storage engine, [ADR 0002](docs/adr/0002-parsing-and-crate-layout.md) parsing and crate layout, [ADR 0003](docs/adr/0003-data-model.md) data model.
- [docs/testing.md](docs/testing.md): how disk-full, the machine probes, the container image and the size gate are tested.
- [CLAUDE.md](CLAUDE.md): architecture summary and invariants for contributors.
