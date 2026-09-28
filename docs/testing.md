# Testing notes

## Disk space

`memory-graph index` keeps a reserve of free space on the database's volume
(`--min-free-disk`, default 5% of the volume, between 2 GB and 32 GB) and
projects the final database size from what it has stored so far. It refuses
up front when the volume is already below the reserve, and stops cleanly mid-run
when free space drops below it or when the projection (once the directory
walk is done) would not fit. Stopping commits what is pending, leaves the
database consistent, and exits non-zero; rerunning resumes, since stored
files are skipped as unchanged.

Three layers test this:

1. **Unit tests** (`crates/graph-cli/src/diskinfo.rs`): the policy over
   scripted samples (reserve rule, projection, calibration of the
   database-bytes-per-source-byte ratio, headroom vs projection stop, unknown
   platform and `--no-disk-check` never stop) and the recognition of
   "No space left on device" / Windows disk-full error text.
2. **Injected probe** (`crates/graph-cli/tests/diskguard.rs`): `index_dir`
   with a probe that reports plenty of space until the database passes a
   size, then almost none; asserts the stop message, a consistent database
   (`describe` shows no open batch), a partial store, and a full resume.
   Runs in `cargo test --workspace` on every platform.
3. **Real ENOSPC** (`scripts/disk-full-tmpfs.sh`, Linux with sudo): indexes a
   20x copy of `testdata/corpus` into a 48 MB tmpfs three ways: with the
   check off (a real `No space left on device`, reported as "disk full",
   database still consistent), with a 4 MB reserve (clean stop), then after
   enlarging the volume (resume). CI runs it on ubuntu. On Windows a
   size-capped volume needs a VHD and administrator rights, so that layer is
   manual there; the injected probe covers the logic.

## Machine probes

`memory-graph sysinfo` prints what `index` sizes itself from: CPUs, memory
(with the probe that read it, or the cause when none could), the starting
budget and the free space on the database's volume; `--json` gives an object.
The memory probe is `/proc/meminfo` on Linux (else `sysinfo(2)`), capped by
the cgroup v2/v1 memory limit when one is below physical RAM,
`host_statistics64` on macOS and `GlobalMemoryStatusEx` on Windows.

1. **Unit tests** (`crates/graph-cli/src/sysinfo.rs`, `report.rs`): the
   `/proc/meminfo` parser with and without `MemAvailable` and on malformed
   lines, the cgroup cap (`max`, v1's unlimited sentinel, usage above the
   limit), the fallback reason text, the report's text and JSON.
2. **e2e** (`crates/graph-cli/tests/e2e.rs`, `sysinfo_reports_the_probes`):
   the command's output, and `index --stats` naming the same source.
3. **CI `probes` job** (`.github/workflows/ci.yml`): on ubuntu 24.04,
   macOS and Windows, the unit and e2e tests plus `sysinfo --json` checked
   by `scripts/check-probes.py` (memory known, source named); on ubuntu also
   inside `docker run --memory=512m` (total at most 512 MiB, source mentions
   `cgroup`) and with `/proc/meminfo` masked (source `sysinfo(2)`). The
   macOS and Windows legs are the only places those probes run in CI (the
   main job is Linux). The cgroup walk (`cgroup_walk_up`) is unit-tested on
   every platform over a fake cgroup tree, including the v1 spellings that
   no CI runner has any more.

## Container image

`.github/workflows/docker.yml` builds the `Dockerfile` (a static musl
`memory-graph` on `scratch`, cross-compiled for `linux/amd64` and
`linux/arm64` without QEMU) on every pull request, push to `main`, `v*`
tag and manual run, and a second job, reached only by the push events and the
only one allowed to write packages, pushes both architectures to
`ghcr.io/p47phoenix/memory-graph` (a `v*` tag must equal the `Cargo.toml`
version or it refuses). The first job loads the amd64 image and smoke-tests it: `sysinfo`
(and `sysinfo --json` through `scripts/check-probes.py`, once plainly and once
under `docker run --memory=512m`, where the source must mention `cgroup` and
the total must fit the limit), `index` of `crates/graph-core/src` from a
read-only mount into a fresh named volume on `/data` (files and tokens
stored, none failed), `search Node --json` (some results), `describe --json`
(the repo is listed), and the image's user, working directory and entrypoint.
The arm64 image is linked from the same pure-Rust source but not executed in
CI (there is no arm64 runner); the amd64 smoke test is the assurance.

A second step runs a server round on the same image: container 1 runs
`serve --db /data/graph.redb --listen 0.0.0.0:7000` on a docker network and a volume; one-shot containers on that network wait for `health`, run
`index --server` and `search --server`, `health --ready` and `cluster leader`; image's own `HEALTHCHECK` must turn `healthy`; `docker stop` (SIGTERM) must 0 and leave no `graph.redb.LOCK` on the volume, after which an embedded
`describe` of the file lists the repo.

## Server and client (ADR 0004 stage A)

Four layers, from the wire up:

1. **Wire types** (`crates/graph-proto`): proptest round trips for every
   converted type (`Node`, `Query`, `SymbolQuery`, `Hit`, `SymbolHit`,
   `RepoInfo`, `IngestStats`, `Extraction`, `Span`) and for
   `StoreError` <-> `tonic::Status` (variant and message kept), plus "decoding
   arbitrary bytes never panics" for each top-level message. The generated
   code is checked by CI's `proto-regen` job (run the xtask, fail on a diff).
2. **Remote conformance** (`cargo test -p graph-client --test conformance`):
   the store conformance suite (`run_all`) against `RemoteStore` over an
   in-process `graph_server::testing::TestServer` (a fresh server per case,
   with that case's extractors), `run_differential(embedded, remote)`,
   `run_crash_rerun_differential`, a server restart mid-batch, snapshot handle
   expiry and the 65th-handle refusal, default-limit paging (more than 1000
   hits from one frozen view), a `StoreError` round trip through a real RPC,
   an unknown protocol version refused, and two servers on one file.
3. **CLI end to end** (`cargo test -p graph-cli --test serve_e2e`): the real
   `memory-graph serve` binary on `--listen 127.0.0.1:0` (the test reads the
   bound port from the `listening on` line, so there are no port races).
   `index`, `describe`, `search`, `symbols` and `export` over the vendored
   corpus with `--server` (and with `--read linearizable`) print byte for byte
   what an embedded run prints (elapsed times normalised); the served file,
   reopened embedded after `Admin.Shutdown`, answers the same again and the
   LOCK sidecar is gone; a second process reads while an index run writes;
   target-selection errors (`--db` with `--server` or `MEMORY_GRAPH_SERVER`,
   `--read` without a server), the `--chunk-bytes` / `--cache-bytes`
   refusals; `health` exit codes before, during and after the server;
   `cluster leader`/`status`, `sysinfo --server`, `vacuum --compact
   --server`; the `Locked` message naming the server's pid and address (for
   an embedded open, with `MEMORY_GRAPH_LOCK_WAIT_MS=300`, and for a second
   `serve`); on unix, SIGTERM stopping the server gracefully. Exit code 5
   (protocol mismatch) needs a server speaking another protocol version, so
   its mapping is unit-tested in `crates/graph-cli/src/target.rs` instead.
4. **RPC overhead** (`crates/graph-client/examples/rpc_bench.rs`, run by hand
   in release): embedded versus remote on the same file, p50/p95; the numbers
   and the verdict against the 5 ms trigger are in
   [spikes/rpc-overhead.md](spikes/rpc-overhead.md).

## Database size

`crates/graph-cli/tests/size_gate.rs` indexes `testdata/corpus` and fails if
the database grows past a fixed multiple of the source bytes (and bytes per
token), before and after `vacuum --compact`, so on-disk cost regressions are
caught in CI. `scripts/measure-size.py` prints the full matrix (commit modes,
re-index, vacuum, compact) as a Markdown table; its numbers are the
provenance for the gate thresholds and for `diskinfo::DISK_RATIO`.
