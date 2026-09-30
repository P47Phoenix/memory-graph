# Indexing

How `index` and `index-file` decide what to write, what happens when a file fails, how a run sizes itself to the machine, and how to watch it. For the commands themselves, see the [README](../../README.md#commands).

## Incremental by default

Each file's fingerprint is the SHA-256 of its bytes plus the language, the extractor and tokenizer versions and the store format version. A file whose fingerprint is already stored is left untouched (only its `origin` is refreshed if it differs) and counted as `unchanged` (a subset of `files`; it adds nothing to `symbols`/`tokens`; `index-file` prints `[unchanged]`). Unchanged files still count as seen for `--prune`. Changed content, a language override, or a new extractor version re-indexes the file fully.

| Flag | Effect |
|---|---|
| `--reindex` | Re-index every file even if unchanged (`index-file` has it too). |
| `--prune` | Remove this repo's files that this run did not index (deleted, renamed, newly ignored). Only files last written by a directory run are considered (`index-file` clears that mark). Skipped when some paths were unreadable, and refused when the run indexed nothing, unless `--force`. |
| `--force` | With `--prune`: allow removals even when nothing was indexed. `--reindex` does not bypass that check. |
| `--max-file-size N` | Skip files larger than N bytes (lockfiles, minified bundles, dumps). Off by default: the only built-in limit is the store's 4 GiB span limit, and such a file is skipped with the reason `larger than 4 GiB (span limit)` without being read. A file is parsed in memory whole and its parse takes about 25x its size (the measured growth per source byte), so a 1 GB file needs about 25 GB of RAM; the memory budget admits it alone. Set this flag when a tree may hold such files. |

## Paths

Stored paths are `/`-separated on every OS: `\` is read as a separator wherever a path comes in (the walk, `index-file`, `ingest`, a `--server` client, the server itself), so a tree indexed on Windows, on Linux, or from Windows through a Linux server gives the same database. Consequences:

- A Unix file literally named `a\b` would be stored as `a/b`, so the directory walk skips it with the reason ``\`` in file name`` rather than let it collide. Non-relative shapes stay distinct but are not portable: `C:\x\a.rs` is stored as `C:/x/a.rs`, a UNC path `\\srv\share\a.rs` as `/srv/share/a.rs`, a leading `..` is kept on a relative path.
- **Upgrading a database built on Windows before this change**, which holds `\` paths: re-indexing adds the `/` copy and the old `\` entries stay (they still show up in search) until `index --prune` removes them. `--prune` only removes files last written by a directory run, so `\` entries written by `index-file` or `ingest` are never pruned; for those, index into a fresh database. `--file` and other path lookups normalize their argument, so they cannot reach an old `\` key.

Known limitation: `index-file` stores the path as given, so it only shares a File node with a directory `index` when called with the same repo-relative path.

## Failures stay per file

If a file fails span validation (an extractor or tokenizer bug), only that file is left out: the rest of the batch is stored, `index` prints `failed: <path>: <reason>`, skips `--prune` with a warning and exits non-zero at the end (`failed=N` in the summary, `failed` and `failed_files` in `--json`). Storage errors still abort the batch. `index-file` (one file) still hard-fails on an invalid span.

A panicking extractor fails only that file the same way (`failed: <path>: extractor panicked: ...`; the panic's own `thread ... panicked` line also appears on stderr). Failed files are listed in path order. With `index --server`, `--stats` `transactions` counts write requests; the commits happen on the server.

## Source encodings

Every file is decoded to UTF-8 before it is tokenized (ADR 0007). Detection is automatic; to override it:

- `--encoding <auto|ansi|LABEL>` on `index` and `index-file` (also `MEMORY_GRAPH_ENCODING`; the flag wins). `ansi` is the Windows system code page (windows-1252 elsewhere), resolved on the client. `replacement` and unknown labels are refused.
- A `.memory-graph.toml` at the root of the directory given to `index`:

  ```toml
  [encoding]
  "legacy/**/*.pas" = "windows-1252"
  "docs/jp/**" = "shift_jis"
  ```

  Globs match the path relative to the root, with `/` separators; `*` does not cross `/`, `**` does. They are case-sensitive on every OS. A glob starting with `./` or `/` is refused (write it relative to the root). The first matching glob decides, and a glob set to `auto` also stops the search (that file is auto-detected). `index-file` does not read this file; use `--encoding`. An invalid file is an error naming it and the key, before anything is written.
- Precedence per file: BOM > `--encoding` (other than `auto`) > the first matching glob > auto. Hints are resolved on the client and sent with each file, so `--server` decodes the same way. A changed hint re-indexes the affected files without `--reindex`.

`--strict-encoding` refuses a file whose decode is lossy (bytes invalid in its encoding) instead of storing it with U+FFFD replacements. Such a file is skipped with the reason `invalid in its encoding (--strict-encoding)`. Earlier releases called this bucket `not valid UTF-8`. As with other skips, a file indexed earlier keeps its stored content, and `--prune` removes it.

## Sizing: threads, memory, disk

`index` streams the directory through three concurrent stages, *walk → parse → commit*, and sizes itself from the machine. Nothing needs tuning on a normal box; these are the knobs.

- **Threads.** One parse thread per CPU but one (the writer). `--jobs N` overrides. Files are committed in walk order by a single writer, so the stored content is the same for any thread count.
- **Memory.** The source bytes in flight (read but not yet committed) are capped by a budget derived from free RAM, re-sampled every quarter second: 20% of RAM is always left to the OS, the process may grow into 70% of what is free above that, and that headroom is divided by the measured growth per source byte (about 25x).
  - Under pressure (free RAM below the reserve, the process past 60% of RAM, or Linux PSI stalls) the budget halves per sample and only grows again once 30% of RAM is free.
  - The budget is at most half of RAM and at least a floor: 256M (64M under pressure) on hosts with about 25 GB or more; on smaller hosts and containers the floors scale down to what takes 25% (6.25% under pressure) of total memory at the growth estimate, so a 512 MB container is not budgeted gigabytes.
  - `--memory 50%` changes the share; `--memory 2G` fixes the budget in source bytes (never re-sampled); `MEMORY_GRAPH_MEMORY` sets the default.
  - Memory is read from `/proc/meminfo` on Linux (else `sysinfo(2)`), capped by the cgroup limit when one is set, `host_statistics64` on macOS and `GlobalMemoryStatusEx` on Windows. If no probe works the budget is a fixed 512M and the memory line says why.
  - A single file is always admitted once nothing else is in flight, whatever its size, so one huge file can exceed the budget by itself.
- **Disk.** The database is about 10x the source. `index` keeps a reserve free on the database's volume (5% of it, between 2G and 32G; `--min-free-disk 4G` or `5%`), refuses to start below it, and stops cleanly if free space or the projected final size (10x the remaining source until measured, then the measured ratio) would go below it: what was committed stays, the database is consistent, it exits non-zero with `stopped before the disk filled: ...; free space and rerun to resume`, and a rerun resumes. A real "No space left on device" is reported the same way. `--no-disk-check` reports but never stops.
- **Reproducible files.** `--deterministic` commits fixed batches (256 files / 32 MiB; a file that does not fit the open batch closes it and starts the next one, alone if it is larger than a batch) so the database file is byte-for-byte the same on any machine (slower when the writer is the bottleneck). A share-of-free-memory budget is raised to fit one batch, the file closing it is admitted on top, and a disk stop writes out the partial batch. `--chunk-bytes` and a fixed `--memory` must each be at least one batch (32M); smaller values are refused.

## Watching a run

- A live view on stderr (only when it is a terminal) shows each stage's work, what it waits for (`blocked: memory budget full`), memory in flight, disk, and the bottleneck. On by default without `--json`; `--progress` forces it with `--json`; `--no-progress` turns it off. Stdout carries only the final summary.
- `--stats` prints how busy each stage was and names the bottleneck (`writer-bound`), the memory source and a `disk:` line; with `--json` it is a `stats` object (with `stats.disk`).
- `--trace run.json` writes a Chrome/Perfetto trace with one span per file per stage.
- `memory-graph sysinfo` (`--json` for an object) prints what the probes see; paste it into a report when sizing looks wrong.
