# memory-graph

An embedded graph database for source code. It indexes one or more repositories into a single file and answers questions such as "where is the token `Node` used?" or "which methods start with `parse`?" with exact byte, line and column spans.

- **Language-agnostic.** Every file of every language is tokenized with exact spans. Languages with an extractor ([22 of them](#languages)) additionally get symbols: functions, types, methods and so on.
- **One file, optionally served.** The database is a single [redb](https://github.com/cberner/redb) file, about 10x the size of the indexed source. Opened in-process by default; `memory-graph serve` shares it with other processes, machines and containers over gRPC, and can run as a replicated Raft cluster.
- **Pure Rust.** No C dependencies (enforced in CI), so it builds anywhere Rust does and ships as a 7 MB static container image.
- **Incremental.** Unchanged files are skipped on re-index; deleted files can be pruned.

The graph is `Org → Repo → File → Symbol → Token`.

## Install

**From source** (current stable Rust; there is no pinned minimum version):

```sh
cargo build --release
# the binary is target/release/memory-graph; put it on your PATH or call it by path
```

**Docker** (no toolchain needed; see the [Docker guide](docs/guide/docker.md)):

```sh
docker pull ghcr.io/p47phoenix/memory-graph:main
docker run --rm ghcr.io/p47phoenix/memory-graph:main --help
```

## Quick start

Index a directory as a repo, then query it. Every command takes `--db <file>` (default `./graph.redb`); the file is created on the first `index`. `--org` and `--repo` are names you choose; they become the top two levels of the graph, and one database can hold many repos.

```sh
memory-graph --db ./g index --org acme --repo api ./api        # whole directory; honors .gitignore, skips binaries
memory-graph --db ./g describe                                  # what got indexed: languages and symbol kinds per repo
memory-graph --db ./g search Node --language rust               # every token `Node` in Rust files (prints nothing if absent)
memory-graph --db ./g symbols 'Node*' --kind struct --json      # struct definitions whose name starts with `Node`
```

What that looks like on this repository's own `crates/graph-core/src`:

```text
$ memory-graph --db ./g index --org demo --repo graph-core crates/graph-core/src
indexed demo/graph-core: files=6 unchanged=0 symbols=589 tokens=27510 skipped=0 failed=0 pruned=0 elapsed=134ms
  rust: 6

$ memory-graph --db ./g search Node --language rust --limit 2
demo/graph-core/schema.rs:140:12	rust	Node	hits=1
demo/graph-core/schema.rs:202:17	rust	tests::node_round_trip::n	hits=1

$ memory-graph --db ./g describe
demo/graph-core: 6 files
  rust: 6 files, 589 symbols, 27510 tokens
    constant/const: 18
    function/fn: 129
    method/fn: 50
    ...
```

Run the same `index` again and every file is reported as `unchanged`: nothing is rewritten.

## Commands

| Command | What it does |
|---|---|
| `index --org O --repo R <DIR>` | Index a directory as one repo. Honors `.gitignore`, skips binary files and, with `--max-file-size`, large ones. |
| `index-file --org O --repo R <PATH>` | Index a single file (`--language` overrides detection). Re-indexing replaces it. |
| `search <TEXT>` | Find tokens by exact text. Filters: `--language`, `--org`, `--repo`, `--kind` (token class), `--grain` (token, symbol, method, class, file, repo, org), `--symbol-kind`. |
| `symbols <PATTERN>` | Find symbol definitions by name: exact, `prefix*`, or `*` for everything. Filters: `--kind` (symbol kind), `--language`, `--org`, `--repo`, `--file`. Each hit points at the line and column of its name, not at an attribute above it ([declaration position](docs/guide/querying.md#declaration-position)); `--json` also keeps the full span. |
| `describe` | Per repo: files, languages, symbols, tokens and the symbol kinds present (`--org`/`--repo` to narrow). |
| `sysinfo` | What `index` sizes itself from on this machine: CPUs, memory and its source, the starting budget, free disk on the database's volume. |
| `vacuum [--compact]` | Drop dictionary terms no file uses; `--compact` rebuilds the file to give the space back. |
| `export [--out FILE]` | Dump every node (org, repo, file, symbol, token, with spans) as newline-delimited JSON. An escape hatch; there is no importer yet. |
| `mcp` | Serve the queries as read-only MCP tools over stdio for AI assistants ([docs/mcp.md](docs/mcp.md)). |
| `serve --db FILE` / `serve --data-dir DIR [--bootstrap \| --join PEER [--auto-promote \| --standby]]` `[--listen HOST:PORT]` | Serve a database file, or a cluster node's data directory, over gRPC for `--server` clients ([server](docs/guide/server.md), [cluster](docs/guide/cluster.md)). |
| `health [--ready]` | With `--server`: exit 0 when the server is serving (`--ready`: and has a leader), 1 when not. |
| `cluster status` / `cluster leader` / `cluster snapshot [--out FILE]` | With `--server`: the node's role, term, leader, log indexes, members and replication lag; `leader` exits 3 when there is none; `snapshot` builds a Raft snapshot and can download it. |
| `cluster members` / `add-learner ID ADDR` / `promote ID` / `remove ID [--force]` / `transfer-leader ID` | With `--server` (any node; forwarded to the leader): list, grow and shrink the membership, with guards ([membership](docs/guide/cluster.md#more-nodes-join-promote-remove)). |

`search`, `symbols` and `describe` take `--json` (an object on stdout); `search` and `symbols` also take `--limit`/`--offset` for paging, with results ordered by org, repo, file, position.

Global options: `--db <file>` (default `./graph.redb`), `--server <host:port>[,<host:port>...]` and `--read local|linearizable` ([server guide](docs/guide/server.md)), `--chunk-bytes` (commit a transaction every this many source bytes, default 64 MiB) and `--cache-bytes` (redb's page cache; default a quarter of available memory, clamped to 64 MiB..4 GiB). Run `memory-graph <command> --help` for the full list.

## Source encodings

Files in UTF-16, Windows code pages, Shift_JIS, GBK, Big5 and other encodings are detected and decoded to UTF-8, so one search matches across them; override detection with `--encoding` or a `.memory-graph.toml`: [docs/guide/indexing.md](docs/guide/indexing.md#source-encodings).

## Languages

Languages are detected per file from the extension, the filename or a `#!` line, so a polyglot repo needs no flags. Every file gets exact-span tokens; these also get symbols:

Rust (`syn`), C#, JavaScript, TypeScript, Python, Java, HTML, ASP.NET markup, SQL, shell, R, F#, Haskell, Elixir, GDScript, C, C++, Go, Scala, COBOL, RPG IV and assembly.

Extensions and what each extractor finds: [docs/guide/languages.md](docs/guide/languages.md). To add one: [docs/adding-a-language.md](docs/adding-a-language.md).

## Server mode

One served database for many processes, machines and containers, exit codes, retries: [docs/guide/server.md](docs/guide/server.md).

## Cluster

3-node Raft clusters, membership, backups: [docs/guide/cluster.md](docs/guide/cluster.md).

## Observability

Logs, Prometheus metrics, health and readiness probes: [docs/guide/observability.md](docs/guide/observability.md).

## Where to go next

| Guide | What it covers |
|---|---|
| [Indexing](docs/guide/indexing.md) | Incremental rules, `--prune`/`--force`, paths, per-file failures, source encodings, sizing (threads, memory, disk), progress and traces |
| [Querying](docs/guide/querying.md) | Grains, `--kind`, symbol patterns, paging, `--json`, MCP |
| [Server mode](docs/guide/server.md) | `serve --db`, choosing the target, LOCK, exit codes, retries and write deadlines |
| [Cluster](docs/guide/cluster.md) | Bootstrap, tuning, TOML config, backups (S3 walkthrough), join/promote/remove, read modes |
| [Observability](docs/guide/observability.md) | Logs, metrics, health and readiness |
| [Docker](docs/guide/docker.md) | The container image: first run, Windows, mounts, Compose, serving, troubleshooting, tags |
| [Languages](docs/guide/languages.md) | Extensions, extractors, tokenizer dialects |
| [Storage](docs/guide/storage.md) | Format, size, reclaiming space, schema upgrades, the retired v1 format |
| [Development](docs/guide/development.md) | Building, tests, CI gates, test corpus, using it as a library |
| [Deployment](docs/deploy/data-dir.md) | Data directory, backup and restore; also [Compose](docs/deploy/compose.md) and [Kubernetes](docs/deploy/kubernetes.md) |
| [Documentation index](docs/README.md) | Glossary, epic, ADRs, spikes, learnings |

Contributors: [CLAUDE.md](CLAUDE.md) has the architecture summary and invariants.

## License

[Apache-2.0](LICENSE).
