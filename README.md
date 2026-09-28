# memory-graph

An embedded graph database for source code. It indexes one or more repositories into a single file and answers questions such as "where is the token `Node` used?" or "which methods start with `parse`?" with exact byte, line and column spans.

- **Language-agnostic.** Every file of every language is tokenized with exact spans. Languages with an extractor (Rust, C#, JavaScript, HTML, ASP.NET markup) additionally get symbols: functions, types, methods and so on.
- **One file, optionally served.** The database is a single [redb](https://github.com/cberner/redb) file, about 8-10x the size of the indexed source. Opened in-process by default; `memory-graph serve` shares it with other processes, machines and containers over gRPC ([Server mode](#server-mode)).
- **Pure Rust.** No C dependencies (enforced in CI), so it builds anywhere Rust does and ships as a 7 MB static container image.
- **Incremental.** Unchanged files are skipped on re-index; deleted files can be pruned.

The graph is `Org → Repo → File → Symbol → Token`.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Indexing](#indexing)
- [Querying](#querying)
- [Server mode](#server-mode)
- [Docker](#docker)
- [Languages](#languages)
- [Storage](#storage)
- [Using it as a library](#using-it-as-a-library)
- [Development](#development)
- [Further reading](#further-reading)

## Install

**From source** (current stable Rust; there is no pinned minimum version):

```sh
cargo build --release
# the binary is target/release/memory-graph; put it on your PATH or call it by path
```

**Docker** (no toolchain needed; see [Docker](#docker) for the full instructions):

```sh
docker pull ghcr.io/p47phoenix/memory-graph:main
docker run --rm ghcr.io/p47phoenix/memory-graph:main --help
```

## Quick start

Index a directory as a repo, then query it. Every command takes `--db <file>` (default `./graph.redb`); the file is created on the first `index`. `--org` and `--repo` are names you choose; they become the top two levels of the graph, and one database can hold many repos.

```sh
memory-graph --db ./g index --org acme --repo api ./api        # whole directory; honors .gitignore, skips binaries
memory-graph --db ./g describe                                  # what got indexed: languages and symbol kinds per repo
memory-graph --db ./g search foo --language rust                # every token `foo` in Rust files
memory-graph --db ./g symbols 'Node*' --kind struct --json      # struct definitions whose name starts with `Node`
```

What that looks like on this repository's own `crates/graph-core/src`:

```text
$ memory-graph --db ./g index --org demo --repo graph-core crates/graph-core/src
indexed demo/graph-core: files=6 unchanged=0 symbols=277 tokens=11737 skipped=0 failed=0 pruned=0 elapsed=132ms
  rust: 6

$ memory-graph --db ./g search Node --language rust --limit 2
demo/graph-core/schema.rs:140:12	rust	Node	hits=1
demo/graph-core/schema.rs:202:17	rust	tests::node_round_trip::n	hits=1

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
| `index --org O --repo R <DIR>` | Index a directory as one repo. Honors `.gitignore`, skips binary files and, with `--max-file-size`, large ones. |
| `index-file --org O --repo R <PATH>` | Index a single file (`--language` overrides detection). Re-indexing replaces it. |
| `search <TEXT>` | Find tokens by exact text. Filters: `--language`, `--org`, `--repo`, `--kind` (token class), `--grain` (token, symbol, method, class, file, repo, org), `--symbol-kind`. |
| `symbols <PATTERN>` | Find symbol definitions by name: exact, `prefix*`, or `*` for everything. Filters: `--kind` (symbol kind), `--language`, `--org`, `--repo`, `--file`. |
| `describe` | Per repo: files, languages, symbols, tokens and the symbol kinds present (`--org`/`--repo` to narrow). |
| `sysinfo` | What `index` sizes itself from on this machine: CPUs, memory and its source, the starting budget, free disk on the database's volume. |
| `vacuum [--compact]` | Drop dictionary terms no file uses; `--compact` rebuilds the file to give the space back. |
| `export [--out FILE]` | Dump every node (org, repo, file, symbol, token, with spans) as newline-delimited JSON. An escape hatch; there is no importer yet. |
| `serve --db FILE [--listen HOST:PORT]` | Serve a database over gRPC for `--server` clients ([Server mode](#server-mode)). |
| `health [--ready]` | With `--server`: exit 0 when the server is serving (`--ready`: and has a leader), 1 when not. |
| `cluster status` / `cluster leader` | With `--server`: the node's role, term, leader and log indexes; `leader` exits 3 when there is none. |

`search`, `symbols` and `describe` take `--json` (an object on stdout); `search` and `symbols` also take `--limit`/`--offset` for paging, with results ordered by org, repo, file, position.

Global options: `--db <file>` (default `./graph.redb`), `--server <host:port>` and `--read local|linearizable` ([Server mode](#server-mode)), `--chunk-bytes` (commit a transaction every this many source bytes, default 64 MiB) and `--cache-bytes` (redb's cache, default 1 GiB). Run `memory-graph <command> --help` for the full list.

## Indexing

### Incremental by default

Each file's fingerprint is the SHA-256 of its bytes plus the language, the extractor and tokenizer versions and the store format version. A file whose fingerprint is already stored is left untouched (only its `origin` is refreshed if it differs) and counted as `unchanged` (a subset of `files`; it adds nothing to `symbols`/`tokens`; `index-file` prints `[unchanged]`). Unchanged files still count as seen for `--prune`. Changed content, a language override, or a new extractor version re-indexes the file fully.

| Flag | Effect |
|---|---|
| `--reindex` | Re-index every file even if unchanged (`index-file` has it too). |
| `--prune` | Remove this repo's files that this run did not index (deleted, renamed, newly ignored). Only files last written by a directory run are considered (`index-file` clears that mark). Skipped when some paths were unreadable, and refused when the run indexed nothing, unless `--force`. |
| `--force` | With `--prune`: allow removals even when nothing was indexed. `--reindex` does not bypass that check. |
| `--max-file-size N` | Skip files larger than N bytes (lockfiles, minified bundles, dumps). Off by default: the only built-in limit is the store's 4 GiB span limit, and such a file is skipped with the reason `larger than 4 GiB (span limit)` without being read. A file is parsed in memory whole and its parse takes about 25x its size (the measured growth per source byte), so a 1 GB file needs about 25 GB of RAM; the memory budget admits it alone. Set this flag when a tree may hold such files. |

Known limitation: `index-file` stores the path as given, so it only shares a File node with a directory `index` when called with the same repo-relative path.

### Failures stay per file

If a file fails span validation (an extractor or tokenizer bug), only that file is left out: the rest of the batch is stored, `index` prints `failed: <path>: <reason>`, skips `--prune` with a warning and exits non-zero at the end (`failed=N` in the summary, `failed` and `failed_files` in `--json`). Storage errors still abort the batch. `index-file` (one file) still hard-fails on an invalid span. A panicking extractor stops the run naming the file; committed transactions stay stored.

### Sizing: threads, memory, disk

`index` streams the directory through three concurrent stages, *walk → parse → commit*, and sizes itself from the machine. Nothing needs tuning on a normal box; these are the knobs.

- **Threads.** One parse thread per CPU but one (the writer). `--jobs N` overrides. Files are committed in walk order by a single writer, so the stored content is the same for any thread count.
- **Memory.** The source bytes in flight (read but not yet committed) are capped by a budget derived from free RAM, re-sampled every quarter second: 20% of RAM is always left to the OS, the process may grow into 70% of what is free above that, and that headroom is divided by the measured growth per source byte (about 25x). Under pressure (free RAM below the reserve, the process past 60% of RAM, or Linux PSI stalls) the budget halves per sample and only grows again once 30% of RAM is free. The budget is at least 256M and at most half of RAM (64M under pressure). `--memory 50%` changes the share; `--memory 2G` fixes the budget in source bytes (never re-sampled); `MEMORY_GRAPH_MEMORY` sets the default. Memory is read from `/proc/meminfo` on Linux (else `sysinfo(2)`), capped by the cgroup limit when one is set, `host_statistics64` on macOS and `GlobalMemoryStatusEx` on Windows. If no probe works the budget is a fixed 512M and the memory line says why. A single file is always admitted once nothing else is in flight, whatever its size, so one huge file can exceed the budget by itself.
- **Disk.** The database is about 10x the source. `index` keeps a reserve free on the database's volume (5% of it, between 2G and 32G; `--min-free-disk 4G` or `5%`), refuses to start below it, and stops cleanly if free space or the projected final size (10x the remaining source until measured, then the measured ratio) would go below it: what was committed stays, the database is consistent, it exits non-zero with `stopped before the disk filled: ...; free space and rerun to resume`, and a rerun resumes. A real "No space left on device" is reported the same way. `--no-disk-check` reports but never stops.
- **Reproducible files.** `--deterministic` commits fixed batches (256 files / 32 MiB; a file that does not fit the open batch closes it and starts the next one, alone if it is larger than a batch) so the database file is byte-for-byte the same on any machine (slower when the writer is the bottleneck). The budget is raised to fit one batch, the file closing it is admitted on top, and a disk stop writes out the partial batch. `--chunk-bytes` must be at least one batch.

### Watching a run

- A live view on stderr (only when it is a terminal) shows each stage's work, what it waits for (`blocked: memory budget full`), memory in flight, disk, and the bottleneck. On by default without `--json`; `--progress` forces it with `--json`; `--no-progress` turns it off. Stdout carries only the final summary.
- `--stats` prints how busy each stage was and names the bottleneck (`writer-bound`), the memory source and a `disk:` line; with `--json` it is a `stats` object (with `stats.disk`).
- `--trace run.json` writes a Chrome/Perfetto trace with one span per file per stage.
- `memory-graph sysinfo` (`--json` for an object) prints what the probes see; paste it into a report when sizing looks wrong.

## Querying

- `search` finds **tokens** by exact text; `symbols` finds **definitions** by name. `--kind` means a different thing on each: a token class on `search` (identifier, keyword, literal, operator, punctuation, comment, other) and a symbol kind on `symbols` (generic: module, type, function, method, variable, constant, other; or language-specific: struct, trait, impl, ...).
- `search --grain token|symbol|method|class|file|repo|org` rolls hits up to that level. `symbol` is the nearest enclosing symbol of any kind, `method` the nearest enclosing method or free function, `class` the nearest enclosing type (struct, class, interface, trait, enum, ...) or Rust `impl` block. These rows carry the whole definition's span: text output prints `file:start_line:start_col-end_line:end_col`, and `--json` has `span` with byte offsets and line/col for both ends. `--symbol-kind` narrows within the grain (`--grain class --symbol-kind struct` is the nearest enclosing struct; `--grain method --symbol-kind function` free functions only); a generic kind the grain can never hold (`--grain class --symbol-kind method`) is refused. A hit with no enclosing symbol of that grain is rolled up to its file with `no_matching_symbol`; a file with no symbols at all (a language without an extractor) with `no_symbols`.
- `--language`, `--kind` and `--symbol-kind` values are validated against what `describe` reports, so a typo is an error, not an empty result. Language names are case-insensitive.
- `symbols` patterns: `name` (exact), `prefix*`, `*` (all), `name\*` (a literal `*`). `**` is rejected as ambiguous.
- Page with `--limit N --offset M`. `--json` prints `{"query", "results": [...]}` (and `"grain"` for `search`).

## Server mode

A database file is opened by one process at a time. To share one between processes, machines or containers, serve it and point the other commands at the server:

```sh
memory-graph serve --db ./g --listen 127.0.0.1:7000     # prints: memory-graph serve: listening on 127.0.0.1:7000 (db ./g, node 1)
memory-graph --server 127.0.0.1:7000 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7000 search foo --language rust
export MEMORY_GRAPH_SERVER=127.0.0.1:7000                 # every command in this shell now uses the server
memory-graph describe
memory-graph health && echo up                            # exit 0 when serving, 1 when not
```

- **Same commands, same answers.** Every command in this README takes `--server` in place of `--db` and prints byte for byte what it prints on the file (tested on the vendored corpus). `index --server` reads and sends the files; the server parses and commits them (the progress view shows `send` and `replicate: acked by leader N (idx K)` stages, and `--stats` an `rpc` row). Reads run while an index writes.
- **Choosing the target.** `--db` and `--server` are exclusive; `MEMORY_GRAPH_SERVER` stands in for `--server` (the flag wins), and `--db` together with either is an error that names both. Neither means `./graph.redb`. `--read linearizable` (or `MEMORY_GRAPH_READ`) makes reads wait until they see every acknowledged write; the default `local` reads the node's store as it is (the same thing on a single node).
- **Settings that belong to the server.** `--cache-bytes` goes to `serve`; `--chunk-bytes` is refused with `--server` (the server cuts its log entries at 8 MiB itself); `--jobs` only sizes the client's reading threads (a warning says so); the disk guard runs on the server, which reports a full disk as an error.
- **`serve`.** `--listen` defaults to `127.0.0.1:7000` (`0.0.0.0:7000` to accept other machines; port `0` picks a free port and the printed line names it). `--node-id` (default 1), `--cache-bytes`, `--snapshot-max-age` (how long a paging client's frozen view may live, default `15m`). It writes `<db>.LOCK` (`{"pid", "listen", "started"}`) next to the file and removes it on a graceful stop (Ctrl-C, SIGTERM, `docker stop`). The Raft log lives in `<db>.raft.redb`.
- **A served file opened directly** waits up to 5 s for the lock (`MEMORY_GRAPH_LOCK_WAIT_MS` changes that), then says who holds it: `database ./g is locked by pid 4242 (memory-graph serve on 127.0.0.1:7000); use --server 127.0.0.1:7000 or stop it`. Once the server stops, the file opens directly again and answers exactly as the server did.
- **Also over `--server`:** `sysinfo` prints the server machine's report under `server <addr> node N (leader: M)`; `vacuum --compact` compacts the server's file; `health [--ready]` and `cluster status [--json]` / `cluster leader` report on the node.
- **Exit codes:** 0 success; 1 failure (and `health`: not serving; a read whose server is unreachable or whose connection was lost); 3 `cluster leader` found no leader; 4 a write was not acknowledged within its deadline (10 s of retries): no leader, the server unreachable, or the connection lost mid-write; 5 the server speaks another protocol or store format version.
- **An error does not prove a write failed.** A write that fails with a lost connection or exit code 4 may still have been applied (the server can commit it and die before answering). Rerunning it is safe: `index` skips unchanged files by fingerprint, `prune` and `vacuum` are idempotent, and `ingest` of the same extraction stores the same thing.
- **Not yet:** this release serves a **single node**. Replication to followers, membership commands (`cluster add-learner`, `promote`, ...), TLS and authentication come with later stages of [ADR 0004](docs/adr/0004-client-server-and-replication.md); bind to loopback or a private network meanwhile.

## Docker

The image `ghcr.io/p47phoenix/memory-graph` is the CLI as a static binary on an empty base (`FROM scratch`): no shell, no package manager, 7 MB, built for `linux/amd64` and `linux/arm64`. Every CLI command and flag in this README works unchanged inside it. Three things to know:

- It runs as user `65532`, not root.
- Its working directory is `/data`, so the default `--db ./graph.redb` lands on whatever you mount at `/data`. Mount the same volume on every run and the commands share one database.
- Always give a tag. `latest` only exists once a release has been tagged (see [Tags and releases](#tags-and-releases)); until then use `:main`.

### First run

Pull the image, index a project into a named volume, then query it. The source is mounted read-only at `/src`; the database lives in the `mg-data` volume.

```sh
docker pull ghcr.io/p47phoenix/memory-graph:main
cd ~/code/api
docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main describe
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main search foo --language rust
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main search foo --grain method --json
```

The `index` line prints the same summary as the native binary (`indexed acme/api: files=... symbols=... tokens=...`). Re-run it after editing the code: unchanged files are skipped. To index a second project into the same database, run `index` again with another `--repo` (or `--org`) and a different source mount; `describe` then lists both.

To see the live progress view (one line per pipeline stage) give the container a terminal with `-t`; without it only the final summary is printed.

The image has no shell, so there is nothing to `docker exec` into and `--entrypoint /bin/sh` fails. Everything is done through the `memory-graph` entrypoint, and each command exits when done.

### Windows

PowerShell: same commands, with `${PWD}` for the current directory.

```powershell
docker run --rm -v "${PWD}:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main describe
```

Git Bash rewrites container paths such as `/src` and `/data` into Windows paths (the error reads `` `C:/Program Files/Git/src` is not a directory``, and a bind-mounted `/data` leaves a stray `db;C` directory behind). Turn that off for every `docker run` that mounts something, including the alias below: prefix the command, or export the variable once for the shell.

```sh
MSYS_NO_PATHCONV=1 docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
export MSYS_NO_PATHCONV=1     # or once per shell
```

### Keeping the database in a host directory

A fresh named volume is writable by the image's user as is. To keep the database in a directory you can see, bind-mount it and run as its owner; on Linux and macOS that is `--user "$(id -u):$(id -g)"`. On Docker Desktop (Windows, macOS) a bind mount is writable without `--user`; in Git Bash add the `MSYS_NO_PATHCONV=1` prefix from the Windows section. Keep the directory outside the source tree, or the database file shows up in the index summary as `skipped (database file)`.

```sh
mkdir -p ~/mg-db
docker run --rm -v "$PWD:/src:ro" -v "$HOME/mg-db:/data" --user "$(id -u):$(id -g)" ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
ls ~/mg-db     # graph.redb
```

A `Permission denied ... must be writable` error on `/data/graph.redb` means the directory (or an existing database) is owned by another user: match it with `--user`, or use a named volume. Once a database was created under one `--user`, keep using it; a later run as the image's default user cannot open it.

### Memory, threads and other settings

The container sizes itself like the native binary: parse threads from the CPUs it can see, the memory budget from free RAM, and a container memory limit is honoured (the budget is sized for the limit, not the host). All `index` flags work; `--memory` can also be given as an environment variable.

```sh
docker run --rm --memory=512m ghcr.io/p47phoenix/memory-graph:main sysinfo                                 # what the container will size from
docker run --rm --cpus=4 -e MEMORY_GRAPH_MEMORY=1G -v "$PWD:/src:ro" -v mg-data:/data \
  ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src --jobs 4 --stats
```

On Docker Desktop the memory source reads `sysinfo(2)+cgroup v2` because `/proc/meminfo` is unreadable to non-root there; on a Linux host it reads `/proc/meminfo+cgroup v2`. The total is the same either way; `sysinfo(2)` has no page-cache figure, so its free-memory reading, and the budget derived from it, is somewhat lower.

### Docker Compose

For repeated use, a `compose.yaml` next to the project fixes the mounts and settings once. The `name:` on the volume keeps it the same `mg-data` volume the `docker run` commands above use (without it Compose prefixes the project name and the database is a different one). The file itself is indexed along with the project (one `yaml` file in the summary).

```yaml
services:
  memory-graph:
    image: ghcr.io/p47phoenix/memory-graph:main
    volumes:
      - ./:/src:ro
      - mg-data:/data
    environment:
      MEMORY_GRAPH_MEMORY: "50%"

volumes:
  mg-data:
    name: mg-data
```

```sh
docker compose run --rm memory-graph index --org acme --repo api /src
docker compose run --rm memory-graph search foo --grain class
docker compose run --rm -T memory-graph search foo --json > hits.json   # -T: no TTY when piping or redirecting
docker compose down -v      # also deletes the database volume
```

### Shell alias

A one-line wrapper makes the container feel like the native binary (in Git Bash, `export MSYS_NO_PATHCONV=1` first):

```sh
alias mg='docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main'
mg index --org acme --repo api /src
mg search foo --grain method
mg search foo --json > hits.json
```

Add `-t` to see the live view while indexing, but not when piping or redirecting `--json` output: a TTY merges stderr into stdout and ends lines with CRLF. The same applies to `docker compose run`, which allocates a TTY by default; pass `-T` there when piping.

### Serving from a container

The image exposes port 7000 and has a `HEALTHCHECK` that asks the server itself (`health --server 127.0.0.1:7000`), so a served container reports `healthy`:

```sh
docker network create mg
docker run -d --name mg-server --network mg -p 127.0.0.1:7000:7000 -v mg-data:/data \
  ghcr.io/p47phoenix/memory-graph:main serve --db /data/graph.redb --listen 0.0.0.0:7000
docker run --rm --network mg -v "$PWD:/src:ro" ghcr.io/p47phoenix/memory-graph:main \
  --server mg-server:7000 index --org acme --repo api /src
memory-graph --server 127.0.0.1:7000 search foo          # from the host, through the published port
docker stop mg-server                                     # SIGTERM: a graceful stop, the LOCK sidecar is removed
```
### Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `manifest unknown` on pull or run | No tag given, so Docker asked for `latest`, which does not exist until the first release. Use `:main` or a `sha-…`/version tag. |
| `` `/src` is not a directory`` or a `C:/Program Files/Git/...` path in the error | Git Bash path conversion. Prefix the command with `MSYS_NO_PATHCONV=1`, or use PowerShell. |
| ``database `./graph.redb` does not exist`` on `search`/`describe` | The `/data` mount differs from the one `index` used. Mount the same named volume or directory. |
| `Permission denied ... must be writable` | A bind-mounted `/data` owned by another user. Add `--user "$(id -u):$(id -g)"` or use a named volume. |
| No progress lines, only the summary | Progress needs a terminal: add `-t`. |
| `exec: "/bin/sh": stat /bin/sh: no such file or directory` | The image has no shell by design. Use the `memory-graph` commands. |

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
docker buildx build --platform linux/arm64 --load -t memory-graph .   # cross-compiled; no QEMU needed to build
```

## Languages

Languages are detected per file from the extension, the filename (`Makefile`) or a `#!` line, so a polyglot repo needs no flags. Every file gets tokens from the generic tokenizer; these languages also get symbols:

| Language | Extensions | Extractor |
|---|---|---|
| Rust | `rs` | `syn` (full parse) |
| C# | `cs`, `csx` | token-stream scanner |
| JavaScript | `js`, `mjs`, `cjs`, `jsx` | token-stream scanner |
| HTML | `html`, `htm`, `xhtml` | element scanner |
| ASP.NET markup | `aspx`, `ascx`, `master` | HTML scanner plus directives, server controls, code blocks and bindings |

Everything else (Python, Go, SQL, YAML, ...) is tokenized with exact spans and no symbols; the same happens to a Rust file if the Rust extractor is not registered (a library build without it). Language names are lowercased, a UTF-8 BOM is ignored, and paths are normalized (`./a.rs` = `a.rs`).

Tokenizer dialects: the generic tokenizer treats `r"a\"b"` as an identifier `r` and a string with Python-style escapes. The Rust extractor uses the `rust_literals` dialect, where raw strings (`r"..."`, `r#"..."#`, `br#"..."#`) and byte literals (`b"..."`, `b'x'`) are single literal tokens and an unterminated raw string runs to end of input.

To add a language, implement the `Extractor` trait in its own crate: see [docs/adding-a-language.md](docs/adding-a-language.md) and [examples/toy-extractor](examples/toy-extractor).

## Storage

- **Format.** One redb file holding an interned dictionary, one compact stream per file with sparse checkpoints, and count postings ([ADR 0003](docs/adr/0003-data-model.md)). About 10x the source and 70 bytes per token on the small test corpus (40 bytes per token at 10 M tokens, where page and dictionary overhead amortise); `scripts/measure-size.py` prints the full table and `crates/graph-cli/tests/size_gate.rs` enforces the ratio in CI (15x, 90 bytes per token).
- **Growth and reclaiming space.** Unchanged files add nothing on a rerun; `--reindex` can double the file until `vacuum --compact`, since redb reuses freed pages but never shrinks the file. `vacuum` frees dictionary terms after churn; `--compact` rebuilds the file.
- **Catalog.** `describe` and filter validation read a small counter catalog kept in step with every write, so they cost O(repos), not O(tokens). It is part of the format, written from the first index.
- **Versioned on disk.** Any change to the stored bytes bumps the schema version; a file from another version is refused without being written to.
- **The v1 format is retired (2026-09-25).** The original per-node layout cost about 525 bytes per token (a fresh index of a 10 GB tree reached 420 GB). Opening a v1 file fails with a message naming its schema version and leaves it untouched. Re-index from source into a new file, or convert it with the last v1-capable release, git tag `v1-last`, using `memory-graph migrate <new.redb>`. `--backend v2` is accepted as a no-op, `--backend v1` is an error; `--v2-chunk-bytes`/`--v2-cache-bytes` are now `--chunk-bytes`/`--cache-bytes` (old spellings still work).

## Using it as a library

The CLI depends on the object-safe `graph_store::Store` / `StoreRead` traits, not on redb. `open_store(path, extractors)` returns a `Box<dyn Store>` over `V2Store`, the one storage format (`V2Store::open(path)` gives the concrete type); bring the traits into scope (`use graph_store::{Store, StoreRead}`) to call methods on it. Indexing is split into `Store::prepare` (pure, callable from many threads) and `Store::index_prepared` (the commit); `Store::index_batch` reports a file's `InvalidSpan` in that file's result slot and returns `Err` only for storage errors. `Extractor` requires `Send + Sync`. `graph_store::conformance::run_all` is a reusable test suite for any `Store` implementation. See the [architecture diagrams](docs/architecture-diagrams.md).

## Development

```sh
cargo fmt --all --check                                  # formatting (CI)
cargo clippy --workspace --all-targets -- -D warnings     # lints (CI, zero warnings)
cargo test --workspace                                    # unit + integration tests
cargo test -p graph-cli --test corpus                      # public-repo corpus: exact spans, cross-repo links
cargo test -p graph-cli --test e2e                          # CLI end-to-end
python3 scripts/test_gate.py                               # CI's extra gate
python3 scripts/check-no-c-deps.py                          # pure-Rust gate: fails on any C build script, native link or deny-listed crate, on any shipped target
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
- [ADR 0001](docs/adr/0001-storage.md) storage engine, [ADR 0002](docs/adr/0002-parsing-and-crate-layout.md) parsing and crate layout, [ADR 0003](docs/adr/0003-data-model.md) data model, [ADR 0004](docs/adr/0004-client-server-and-replication.md) client/server access and Raft replication (Accepted 2026-09-28; built in stages).
- [docs/testing.md](docs/testing.md): how disk-full, the machine probes, the container image and the size gate are tested.
- [CLAUDE.md](CLAUDE.md): architecture summary and invariants for contributors.
