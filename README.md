# memory-graph

An embedded graph database for source code. It indexes one or more repositories into a single file and answers questions such as "where is the token `Node` used?" or "which methods start with `parse`?" with exact byte, line and column spans.

- **Language-agnostic.** Every file of every language is tokenized with exact spans. Languages with an extractor (Rust, C#, JavaScript, TypeScript, Python, Java, HTML, ASP.NET markup, SQL, shell, R, F#, Haskell, Elixir, GDScript, C, C++, Go, Scala) additionally get symbols: functions, types, methods and so on.
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
| `serve --db FILE` / `serve --data-dir DIR [--bootstrap \| --join PEER [--auto-promote \| --standby]]` `[--listen HOST:PORT]` | Serve a database file, or a cluster node's data directory, over gRPC for `--server` clients ([Server mode](#server-mode), [Cluster](#cluster)). |
| `health [--ready]` | With `--server`: exit 0 when the server is serving (`--ready`: and has a leader), 1 when not. |
| `cluster status` / `cluster leader` / `cluster snapshot [--out FILE]` | With `--server`: the node's role, term, leader, log indexes, members and replication lag; `leader` exits 3 when there is none; `snapshot` builds a Raft snapshot and can download it. |
| `cluster members` / `add-learner ID ADDR` / `promote ID` / `remove ID [--force]` / `transfer-leader ID` | With `--server` (any node; forwarded to the leader): list, grow and shrink the membership, with guards ([More nodes](#more-nodes-join-promote-remove)). |

`search`, `symbols` and `describe` take `--json` (an object on stdout); `search` and `symbols` also take `--limit`/`--offset` for paging, with results ordered by org, repo, file, position.

Global options: `--db <file>` (default `./graph.redb`), `--server <host:port>[,<host:port>...]` and `--read local|linearizable` ([Server mode](#server-mode)), `--chunk-bytes` (commit a transaction every this many source bytes, default 64 MiB) and `--cache-bytes` (redb's cache, default 1 GiB). Run `memory-graph <command> --help` for the full list.

## Indexing

### Incremental by default

Each file's fingerprint is the SHA-256 of its bytes plus the language, the extractor and tokenizer versions and the store format version. A file whose fingerprint is already stored is left untouched (only its `origin` is refreshed if it differs) and counted as `unchanged` (a subset of `files`; it adds nothing to `symbols`/`tokens`; `index-file` prints `[unchanged]`). Unchanged files still count as seen for `--prune`. Changed content, a language override, or a new extractor version re-indexes the file fully.

| Flag | Effect |
|---|---|
| `--reindex` | Re-index every file even if unchanged (`index-file` has it too). |
| `--prune` | Remove this repo's files that this run did not index (deleted, renamed, newly ignored). Only files last written by a directory run are considered (`index-file` clears that mark). Skipped when some paths were unreadable, and refused when the run indexed nothing, unless `--force`. |
| `--force` | With `--prune`: allow removals even when nothing was indexed. `--reindex` does not bypass that check. |
| `--max-file-size N` | Skip files larger than N bytes (lockfiles, minified bundles, dumps). Off by default: the only built-in limit is the store's 4 GiB span limit, and such a file is skipped with the reason `larger than 4 GiB (span limit)` without being read. A file is parsed in memory whole and its parse takes about 25x its size (the measured growth per source byte), so a 1 GB file needs about 25 GB of RAM; the memory budget admits it alone. Set this flag when a tree may hold such files. |

Stored paths are `/`-separated on every OS: `\` is read as a separator wherever a path comes in (the walk, `index-file`, `ingest`, a `--server` client, the server itself), so a tree indexed on Windows, on Linux, or from Windows through a Linux server gives the same database. Consequences:

- A Unix file literally named `a\b` would be stored as `a/b`, so the directory walk skips it with the reason ``\`` in file name`` rather than let it collide. Non-relative shapes stay distinct but are not portable: `C:\x\a.rs` is stored as `C:/x/a.rs`, a UNC path `\\srv\share\a.rs` as `/srv/share/a.rs`, a leading `..` is kept on a relative path.
- **Upgrading a database built on Windows before this change**, which holds `\` paths: re-indexing adds the `/` copy and the old `\` entries stay (they still show up in search) until `index --prune` removes them. `--prune` only removes files last written by a directory run, so `\` entries written by `index-file` or `ingest` are never pruned; for those, index into a fresh database. `--file` and other path lookups normalize their argument, so they cannot reach an old `\` key.

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
- **`serve`.** `--listen` defaults to `127.0.0.1:7000` (`0.0.0.0:7000` to accept other machines; port `0` picks a free port and the printed line names it). `--node-id` (default 1 with `--db`), `--cache-bytes`, `--snapshot-max-age` (how long a paging client's frozen view may live, default `15m`). With `--db` it writes `<db>.LOCK` (`{"pid", "listen", "started"}`) next to the file and removes it on a graceful stop (Ctrl-C, SIGTERM, `docker stop`; on Windows Ctrl-C or Ctrl-Break, which is what a supervisor sends a console process started in its own process group, e.g. Python's `send_signal(signal.CTRL_BREAK_EVENT)`; `taskkill /F` is a kill, not a graceful stop); the Raft log lives in `<db>.raft.redb`. `--data-dir` (below) keeps everything in one directory instead.
- **A served file opened directly** waits up to 5 s for the lock (`MEMORY_GRAPH_LOCK_WAIT_MS` changes that), then says who holds it: `database ./g is locked by pid 4242 (memory-graph serve on 127.0.0.1:7000); use --server 127.0.0.1:7000 or stop it`. Once the server stops, the file opens directly again and answers exactly as the server did.
- **Also over `--server`:** `sysinfo` prints the server machine's report under `server <addr> node N (leader: M)`; `vacuum --compact` compacts the server's file; `health [--ready]` and `cluster status [--json]` / `cluster leader` report on the node.
- **Exit codes:** 0 success; 1 failure (and `health`: not serving; a read whose server is unreachable or whose connection was lost); 3 `cluster leader` found no leader; 4 a write was not acknowledged within its deadline (`--write-deadline`, default 10 s of retries): no leader, the server unreachable, or the connection lost mid-write; 5 the server speaks another protocol or store format version; 6 a data directory (or a node named in a membership change) belongs to another cluster (`WrongCluster`).
- **An error does not prove a write failed.** A write that fails with a lost connection or exit code 4 may still have been applied (the server can commit it and die before answering). Rerunning it is safe: `index` skips unchanged files by fingerprint, `prune` and `vacuum` are idempotent, and `ingest` of the same extraction stores the same thing. A retried write reports what the retry did: a `prune` that landed before the connection was lost reports 0 removed on the retry, though the stored state is correct.
- **Write deadline.** `--write-deadline <duration>` (or `MEMORY_GRAPH_WRITE_DEADLINE`; e.g. `500ms`, `10s`, `2m`; default `10s`) is how long a write keeps retrying through no leader or a lost connection before it fails with exit code 4.
- **Not yet:** TLS (issue #104, once a pure-Rust provider passes the no-C gate) and authentication (#105), see [ADR 0004](docs/adr/0004-client-server-and-replication.md); bind to loopback or a private network meanwhile.

### Cluster

`serve --data-dir` runs a node that replicates through Raft (ADR 0004 D5-D9). Every write is a log entry that a majority fsyncs before it is acknowledged, and every node answers reads from its own copy. Nodes join with `--join`, and any node takes writes and membership commands (forwarding them to the leader).

```sh
memory-graph serve --data-dir ./n1 --bootstrap --node-id 1 --listen 127.0.0.1:7001   # a NEW one-node cluster (prints a warning saying so)
memory-graph --server 127.0.0.1:7001 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7001 cluster status          # role, term, leader, log, snapshot, members, lag
memory-graph --server 127.0.0.1:7001 cluster snapshot --out backup.redb
memory-graph serve --data-dir ./n1 --listen 127.0.0.1:7001   # later: a restart needs no flags
```

- **Data directory.** It holds `node.json` (node id, cluster id, advertised address, versions), `graph.redb` (the store), `raft.redb` (the Raft log and vote), `snapshots/` (the latest snapshot and its `.meta`) and `LOCK`. `--data-dir` and `--db` are exclusive.
- **Start-up.** `--bootstrap` on an empty directory creates a new cluster with a random id and this node as its only voter. On a directory that is already initialized it is a plain restart, so a container can keep the flag in its command. With neither flag, an initialized directory restarts from its state and an empty one is refused ("pass --bootstrap"). `--node-id` is required on the first start and then read from `node.json`; a different id is refused.
- **`--advertise HOST:PORT`** is the address peers and clients reach this node at. The default is the listen address, with `0.0.0.0` or `[::]` replaced by the host name. It is stored in `node.json` and in the cluster membership, so a restart with a different `--advertise` is refused.
- **Moving a node.** `serve --data-dir ./n3 --listen 0.0.0.0:7013 --update-advertise host3:7013` restarts a member at a new address (new IP, port or DNS name). Once it serves, it asks the leader, through any member it knows, to record the new address in the membership (the leader first asks the new address who is there: it must be this node of this cluster with the same extractors), and only then rewrites `node.json`. No re-join, no data copied. If no leader accepts it within 2 minutes the start fails and `node.json` is unchanged; the same address again is a plain restart.
- **Restore.** `serve --data-dir <empty dir> --bootstrap --node-id 1 --restore backup.redb` seeds a new cluster from a `cluster snapshot --out` file. The store is checked, and the new cluster gets its own id and a fresh log.
- **`cluster status [--json]`** adds, for a data-dir node: role, cluster id, advertised address, snapshot and purged indexes, every member with its role and address, the leader's per-peer replication lag, and the log and store file sizes. **`cluster snapshot [--out FILE] [--json]`** builds a snapshot now. With `--out` it downloads the file to this machine and verifies its SHA-256 and size.
- **Log and snapshots.** A snapshot is built after `--snapshot-log-entries` (default 10000) applied entries or `--snapshot-log-bytes` (default 1G) of log. The log below it is then purged, keeping `--log-keep-entries` (default 1000) so a briefly lagging follower catches up from the log, and `raft.redb` is compacted. Timing: `--heartbeat-interval` (ms, default 250), `--election-timeout-min` / `--election-timeout-max` (ms, default 1000 / 2000).
- **Disk guard.** `--min-free-disk` (a size, or a percentage of the volume; the default for a data directory is 5% of it, between 2G and 32G, and off for `--db`). Writes and snapshot builds are refused with `RESOURCE_EXHAUSTED` while less than that plus one snapshot copy is free.
- **Configuration file.** `serve --config serve.toml` (or `MEMORY_GRAPH_CONFIG`) reads the same settings from TOML: every `serve` flag is a key of the same name, kebab-case or snake_case, plus `db` and `cache-bytes`; a switch is `true`/`false`, a list an array. A flag on the command line, or its environment variable (`MEMORY_GRAPH_LOG`), overrides the file, and the file overrides the defaults. An unknown key is an error, and the file's values go through the flags' own checks. For example:

  ```toml
  data-dir = "/var/lib/memory-graph"
  listen = "0.0.0.0:7000"
  advertise = "node1.internal:7000"
  node-id = 1
  bootstrap = true
  metrics-listen = "0.0.0.0:9100"
  log-format = "json"
  ```
- **Backup.** `cluster snapshot --out FILE` downloads a consistent copy of the store from any node (SHA-256 and size verified); `serve --data-dir <empty dir> --bootstrap --restore FILE` seeds a new cluster from it. See [docs/deploy/data-dir.md](docs/deploy/data-dir.md).

Measurements (ingest on one and three nodes, snapshots, read latency, the corpus and 10 M tokens, a 60-minute soak): [docs/spikes/raft-replication.md](docs/spikes/raft-replication.md).

#### More nodes: join, promote, remove

```sh
memory-graph serve --data-dir ./n2 --node-id 2 --listen 127.0.0.1:7002 --join 127.0.0.1:7001 --auto-promote
memory-graph serve --data-dir ./n3 --node-id 3 --listen 127.0.0.1:7003 --join 127.0.0.1:7001 --auto-promote
memory-graph --server 127.0.0.1:7003 cluster members          # 1 voter ... (leader), 2 voter ..., 3 voter ...
memory-graph --server 127.0.0.1:7002 index --org acme --repo api ./api   # a follower: forwarded to the leader
memory-graph --server 127.0.0.1:7002 cluster transfer-leader 2
memory-graph --server 127.0.0.1:7001 cluster remove 3 --force
```

- **`--join <host:port>`** names any member (it forwards to the leader). On an empty directory the node asks to be added as a learner, takes the cluster id, and catches up from the leader's log or snapshot; it prints its `listening on` line once it is in. It retries while no leader answers, up to `--join-timeout` (default `2m`). The leader refuses a node with another extractor version set, store format or protocol version, a node id that is already a member at another address, and a node id that is already a voter (an empty directory under a voter's id would have lost its vote and log: `cluster remove` it first).
- **`--auto-promote`** makes the node a voter once its replication lag is zero (the leader does it; a node restarted while still a learner asks again). **`--standby`**, the default without `--auto-promote`, keeps it a learner: a read replica that replicates everything, answers reads and forwards writes, until `cluster promote <id>`.
- **Idempotent restarts.** `--bootstrap` and `--join` on a directory that already belongs to the cluster are plain restarts, so a container keeps the same command line. A directory of another cluster is refused with `WrongCluster` (exit code 6) before anything is opened. A directory that holds a store or a log but no `node.json` (a `--db` file copied in, say) is refused unless `--accept-snapshot-overwrite`, which moves what it holds into `replaced-<time>/` and joins empty.
- **Writes through any node.** A write sent to a follower or learner is forwarded to the leader by the server (an `index` streams through, a few files at a time), and the answer is the leader's (`forwarded_to_leader`; `cluster status --json` counts `writes_forwarded_total`). With no leader known the server answers `NoLeader` and the client retries until `--write-deadline` (then exit code 4); a node cut off from the majority refuses writes this way while it keeps answering `local` reads. `--read linearizable` on a follower asks the leader for its read index and answers once it has applied it. Membership commands are forwarded the same way, so `--server` may name any node.
- **Read modes.** `--read local` (the default) answers from the node's own store at once, even with no leader; it may miss the latest acknowledged writes, and with `--json` the output of `search`, `symbols` and `describe` then says `"stale_possible": true` (the node knows no leader, has not heard from it within the freshness lease, `election_timeout_min` minus two heartbeats, or has not applied what the leader last reported committed; `false` otherwise). `"stale_possible": false` is a best-effort hint, not a guarantee: detection lags by up to the lease (after a leader dies, or while a leader is cut off from the majority), and only `--read linearizable` guarantees freshness. `--read linearizable` sees every write acknowledged before the read began: on a follower the node asks the leader for its read index and answers once it has applied it; with no reachable leader (a node cut off in a minority, an election) it fails with `NoLeader` (exit code 4) instead of answering stale data, once `--read-deadline` (or `MEMORY_GRAPH_READ_DEADLINE`, default 5s; it also bounds the first connect) has passed. Output of an embedded `--db` run has no `stale_possible`.
- **Several nodes in `--server`.** `--server a:7000,b:7000,c:7000` (or the same in `MEMORY_GRAPH_SERVER`) uses the first node that answers and moves on to the next when one is down or has no leader, so a script keeps working while a node restarts.
- **`cluster members [--json]`** lists every member with its role and address and marks the leader. **`cluster add-learner <id> <host:port> [--no-wait]`** adds a node that is already serving (the node is asked who it is first: another node id, another cluster or other extractors are refused). **`cluster promote <id>`** makes a learner a voter; it is refused for an unknown node or a node whose extractor version set differs (asked again at promotion time). Membership changes are idempotent under a retry: promoting a voter, adding a learner already listed at the same address, and removing a node that is not a member succeed without a change.
- **`cluster remove <id> [--force]`** is refused for the leader ("transfer leadership first"), for any removal after which the voters reachable right now would be fewer than a quorum of the remaining voters or of the current ones (`--force` does not override this), and for 3 voters down to 2 unless `--force` (two voters tolerate no failure). A learner is removed without these checks.
- A membership change (`add-learner`, `promote`, `remove`) that times out may still commit later: check `cluster members` before retrying.
- **`cluster transfer-leader <id>`** hands leadership to a voter that has caught up. openraft 0.9 has no transfer of its own, so the leader pauses new writes and membership changes (clients retry, as in an election), lets what is in flight finish, pauses its heartbeats and asks the target to call an election every 50 ms; the target wins as soon as the leader lease (`--election-timeout-max`) runs out, before any other node campaigns. Only one transfer runs at a time; if the target cannot be reached the transfer ends at once and the old leader keeps leading undisturbed, and if it does not take over within 20 s the old leader resumes.
- **Exit codes** add 6: the data directory belongs to another cluster than `--join`'s peer (or a node named in `add-learner` does).

#### Observability

```sh
memory-graph serve --data-dir ./n1 --listen 127.0.0.1:7001 --metrics-listen 127.0.0.1:9101 --log-format json
curl -s 127.0.0.1:9101/metrics                                   # Prometheus text format 0.0.4
memory-graph --server 127.0.0.1:7001 health --ready && echo ready  # exit 0 ready, 1 not (or unreachable)
MEMORY_GRAPH_LOG=debug memory-graph serve ...                     # RPC and apply spans
```

- **Logs** go to stderr (stdout carries only the `listening on` start line, and `metrics on` before it; under `--log-format json` these are JSON objects too, with `event` (`metrics`/`listening`) and `addr`, and their `message` keeps the text form, so a container runtime that merges stdout into the log stream sees only JSON). `--log-format text` (default) or `json`: one object per line with `timestamp`, `level`, `target`, `message`, the event's fields beside it, and `span` (the innermost span and its fields). `--log-level <filter>` (or `MEMORY_GRAPH_LOG`; default `info`) takes `tracing` EnvFilter syntax, e.g. `info,graph_server=debug`. At `debug`, every gRPC call logs `rpc finished` inside an `rpc` span (`method`, `peer`, `outcome`, `duration_ms`) and every applied log entry `applied` inside an `apply` span (`index`, `kind`, `files`, `duration_ms`).
- **Metrics.** `--metrics-listen HOST:PORT` serves `GET /metrics` (plain HTTP/1.1, one request per connection, at most 64 connections at once and 5 s to send the request; anything else is 404/405). `Admin.Metrics` returns the same text over gRPC. Names are stable:

  | Metric | Type | Meaning |
  |---|---|---|
  | `mg_raft_term` | gauge | Current Raft term |
  | `mg_raft_leader_id` | gauge | Known leader's node id (0: none) |
  | `mg_raft_role{role}` | gauge | 1 for the current role (`leader`, `follower`, `candidate`, `learner`, `shutdown`) |
  | `mg_raft_last_log_index`, `mg_raft_committed_index`, `mg_raft_applied_index` | gauge | Last log entry, last known committed, last applied to the store |
  | `mg_raft_snapshot_index`, `mg_raft_purged_index` | gauge | Current snapshot's index, last purged index (0: none) |
  | `mg_raft_replication_lag{peer}` | gauge | Leader only: entries each peer is behind the leader's last log index |
  | `mg_store_bytes`, `mg_log_bytes` | gauge | Store file and Raft log file sizes |
  | `mg_snapshot_handles_open` | gauge | Open snapshot handles held for paging clients |
  | `mg_rpc_duration_seconds{rpc,outcome}` | histogram | gRPC call duration until the response headers (buckets 0.5 ms to 10 s); `rpc` like `Store/Search` (`unknown` for any path that is not one of the server's methods, so a client cannot create label values), `outcome` `ok` or the gRPC code (`unavailable`, `failed_precondition`, ...) |
  | `mg_rpc_total{rpc,outcome}` | counter | gRPC calls served |
  | `mg_writes_forwarded_total` | counter | Writes and membership changes forwarded to the leader |
  | `mg_apply_duration_seconds` | histogram | Time to apply one committed log entry |
  | `mg_build_info{version,protocol,store_format}` | gauge | Always 1 |

  `cluster status --json` reports the same Raft and store numbers, `writes_forwarded_total`, `rpcs_total` and `entries_applied_total`. Contract note on `outcome`: it is read from the response headers, so an error a streaming call (`Descendants`, `FileTokens`, snapshot download) reports in its trailers after its first message counts as `ok`; alert on stream failures from the client side.
- **Health.** gRPC `grpc.health.v1`: the default service (`""`) is `SERVING` once the store is open; `memory-graph.ready` is `SERVING` only while a leader is known, a leader's `AppendEntries` (heartbeats included; learners get them too) reached this node within three maximum election timeouts, and it has applied to within `--ready-max-lag` entries (default 1000) of the leader's commit index: a learner still catching up, or one cut off from the leader, is not ready; an idle, caught-up one is. Both go `NOT_SERVING` as soon as a shutdown starts. `memory-graph health [--ready] --server <addr>` exits 0 when serving, 1 otherwise; it is the probe for the shell-less image (the Docker `HEALTHCHECK` runs `health`, Compose and Kubernetes use `--ready`).
- **Deployment.** [docs/deploy/compose.md](docs/deploy/compose.md) (a three-node cluster with `deploy/compose/cluster.yml`, checked end to end in CI), [docs/deploy/kubernetes.md](docs/deploy/kubernetes.md) (a StatefulSet from `deploy/kubernetes/`, with `serve --node-id-from-hostname --bootstrap-or-join <pod 0>`; a pod 0 that lost its volume asks the other pods and rejoins their cluster instead of creating a second one), [docs/deploy/data-dir.md](docs/deploy/data-dir.md) (the data directory, backup and restore).

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
  ghcr.io/p47phoenix/memory-graph:main serve --data-dir /data --bootstrap --node-id 1 --listen 0.0.0.0:7000
docker run --rm --network mg -v "$PWD:/src:ro" ghcr.io/p47phoenix/memory-graph:main \
  --server mg-server:7000 index --org acme --repo api /src
memory-graph --server 127.0.0.1:7000 search foo          # from the host, through the published port
docker stop mg-server                                     # SIGTERM: a graceful stop, /data/LOCK is removed
docker start mg-server                                    # same command again: --bootstrap on an initialized /data is a plain restart
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
| TypeScript | `ts`, `tsx`, `mts`, `cts` | the JavaScript scanner plus interfaces, type aliases, enums, namespaces, abstract classes and typed class fields |
| Python | `py`, `pyw`, `pyi` | indentation scanner (classes, functions, methods, lambdas, module constants); a file with unbalanced brackets or broken indentation is flagged `has_errors` and gets tokens only |
| Java | `java` | token-stream scanner (package, classes, interfaces, enums, records, annotation types, methods, fields, constants) |
| HTML | `html`, `htm`, `xhtml` | element scanner |
| ASP.NET markup | `aspx`, `ascx`, `master` | HTML scanner plus directives, server controls, code blocks and bindings |
| SQL | `sql` | `CREATE` statement scanner (ANSI, T-SQL, PL/pgSQL, PL/SQL, MySQL) |
| Shell | `sh`, `bash`, `zsh`, `ksh` (and `#!` lines) | token-stream scanner: functions, `export` / `readonly` |
| R | `r`, `rmd`, `qmd` | token-stream scanner (R chunks only in R Markdown / Quarto) |
| F# | `fs`, `fsi`, `fsx` | layout scanner: namespaces, modules, types, `let`, members |
| Haskell | `hs`, `lhs` | layout scanner: module, data/newtype/type/class/instance, functions (signature and equations grouped); literate bird-track and `\begin{code}` |
| Elixir | `ex`, `exs` | `do`/`end` scanner: `defmodule` (a type), `def`/`defp`/`defmacro`..., `defstruct`, `defprotocol`/`defimpl` |
| GDScript | `gd` | layout scanner: `class_name` (the file class), inner classes, `func`, `signal`, `enum`, `const`, `var` |
| C | `c`, `h` | token-stream scanner (`lang-c` feature) |
| C++ | `cpp`, `cc`, `cxx`, `hpp`, `hh`, `hxx`, `ipp` | token-stream scanner (`lang-c` feature, shared with C; a `.h` stays language `c` but is scanned with the C++ rules when it contains `class`/`namespace`/`template`/`public:`) |
| Go | `go` | token-stream scanner (receiver methods are siblings of their type, so they do not roll up under `--grain class`) |
| Scala | `scala`, `sc` | token-stream scanner (brace and indentation syntax; a `def` in an `object` is a function). `.sc` is also SuperCollider's extension; such files are scanned as Scala and get few or odd symbols |
| COBOL | `cbl`, `cob`, `cpy` | sentence scanner: programs, divisions, sections, paragraphs, level-01/77 items (fixed and free format) |
| RPG IV / RPGLE | `rpgle`, `sqlrpgle`, `rpgleinc`, `rpg` | token-stream scanner: procedures, subroutines, prototypes, interfaces, data structures, standalone fields, constants, tags (`**FREE`, mixed and fixed form) |
| Assembly | `asm`, `s` (not `inc`: PHP, Pascal and POV-Ray use it too) | line scanner (NASM, MASM, GNU as; x86 and ARM): labels, procs, macros, sections, segments, structs, constants |

Everything else (YAML, ...) is tokenized with exact spans and no symbols; the same happens to a Rust file if the Rust extractor is not registered (a library build without it). Language names are lowercased, a UTF-8 BOM is ignored, and paths are normalized (`./a.rs` = `a.rs`).

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

## License

[Apache-2.0](LICENSE).
