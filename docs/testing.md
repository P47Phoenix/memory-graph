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
`serve --data-dir /data --bootstrap --node-id 1 --listen 0.0.0.0:7000` on a
docker network and a volume. One-shot containers on that network wait for
`health`, then run `index --server`, `search --server`, `health --ready`,
`cluster leader` and `cluster status --json` (the node leads and reports its
cluster id and data directory). The image's own `HEALTHCHECK` must turn
`healthy`. `docker stop` (SIGTERM) must exit 0 and leave `graph.redb`,
`raft.redb` and `node.json`, but no `LOCK`, on the volume. `docker start`
(the same command, `--bootstrap` included) must come back ready with the
same cluster id and the indexed data. After a second stop, an embedded
`describe` of `/data/graph.redb` lists the repo.

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
   `serve`); on unix, SIGTERM stopping the server gracefully; on Linux, an
   idle `serve --db` waking under 8 times/s and a one-node `serve --data-dir`
   under 12 (context switches of all its threads from `/proc` over 10 s,
   issue #205; the Docker workflow checks the image the same way with
   `scripts/idle-cpu.sh`, see [the idle CPU spike](spikes/idle-cpu.md)). Exit code 5
   (protocol mismatch) needs a server speaking another protocol version, so
   its mapping is unit-tested in `crates/graph-cli/src/target.rs` instead.
4. **RPC overhead** (`crates/graph-client/examples/rpc_bench.rs`, run by hand
   in release): embedded versus remote on the same file, p50/p95; the numbers
   and the verdict against the 5 ms trigger are in
   [spikes/rpc-overhead.md](spikes/rpc-overhead.md).

## Cluster (ADR 0004 stages B and C)

Two more layers on top of the four above. Every wait polls a condition with
a hard timeout and a message naming what it waited for. Faults are injected
through deterministic hooks (failpoints in `TestingHooks`, the network
`FaultPlan`, a fake free-space probe, the apply gate
`ServeConfig::testing_apply_gate`), never by sleeping and hoping. The
`testing_*` hooks are for tests only: `#[doc(hidden)]`, not a supported API,
and they may change without notice.

**Test-only environment variables.** `memory-graph serve` reads two
variables that exist only for `serve_e2e` and are not features; a server
started with any `TestingHooks` set logs a warning at start:

- `MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER=<n>`: let n write proposals
  through, then park every later one, so a test can kill the server
  mid-write.
- `MEMORY_GRAPH_TESTING_WITHHOLD_LEADER` (any value): act as if no leader
  were known (writes and linearizable reads answer `NoLeader`).

5. **Cluster testbed** (`cargo test -p graph-server --test cluster`):
   `graph_server::testing::ClusterTestbed` runs n in-process nodes on
   `127.0.0.1:0` with temporary data directories and fast Raft timing. It
   runs them on its own runtime thread, so `RemoteStore` can be used from
   the test thread. Node 1 bootstraps and the others start uninitialized;
   `form()` adds and promotes them. Nodes can `stop()`, `kill()` (no
   graceful shutdown, the store dropped without a clean close) and
   `restart()` on the same port and directory. The tests cover:
   - replication of a corpus subset: three languages and a file over 1 MiB,
     with `run_differential` between two nodes and against an embedded
     oracle;
   - leader loss, with local reads on the survivors checked continuously
     through the election, writes resuming on the new leader, and the old
     leader catching up;
   - a laggard catching up by `InstallSnapshot` after the log was purged
     past it;
   - restarting every node from persisted state;
   - exactly-once apply across a crash before apply and inside the apply
     transaction (failpoints), and an acknowledged write surviving `kill()`
     of every node;
   - the log's durability order (the `LogFlushed` callback only after the
     redb commit returns, recorded by an observer);
   - `cluster snapshot --out` restored into a new cluster (same answers, a
     new cluster id, a fresh log);
   - bootstrap idempotence, the refusals (an empty directory without flags,
     a different node id, another extractors hash on install), and the disk
     guard's `RESOURCE_EXHAUSTED`.

   The log store's own unit tests (`cargo test -p graph-server --lib
   log_store`) include the post-purge compaction.
6. **Three processes** (`cargo test -p graph-cli --test cluster_e2e`): three
   real `memory-graph serve --data-dir ... --node-id N --listen 127.0.0.1:0`
   processes run in these steps:
   1. Node 1 starts with `--bootstrap`, and nodes 2 and 3 with `--join
      <node 1> --standby` (learners). `cluster add-learner` (idempotent for
      a learner already there) and `cluster promote` make them voters, and
      `cluster status` shows every member and the leader's lag.
   2. The vendored corpus is indexed through node 1 with `--server`. Node 3,
      a follower serving local reads, answers `describe`, `search`,
      `symbols` and `export` byte for byte as an embedded run does.
   3. The leader is stopped with `Admin.Shutdown`, and `cluster leader` on a
      survivor names the new one. A write goes through it and is read back
      from the other survivor.
   4. The old leader restarts from its data directory with no flags (same
      port, since the advertised address is part of its identity). It
      rejoins as a follower, catches up and answers identically.
   5. On unix, SIGTERM stops the new leader cleanly and the remaining two
      nodes elect again.

   A second test covers the `serve --data-dir` refusals (an empty directory
   without `--bootstrap`, `--restore` without `--bootstrap`, `--db` with
   `--data-dir`, a different `--node-id` on restart) and `--bootstrap` on
   an initialized directory keeping its cluster id. A third,
   `join_forward_remove_transfer_and_wrong_cluster` (stage C, about 2 s),
   starts nodes 2 and 3 with `--join <node 1> --auto-promote` and waits for
   `cluster members --json` to list three voters; indexes a directory
   through node 2 (a follower: forwarded, `writes_forwarded_total` counts
   it, the output equals an embedded run) and reads it from node 3; checks
   that `cluster remove` of the leader and 3 -> 2 without `--force` exit 1
   with the reason on stderr; moves leadership with `cluster
   transfer-leader 2` (sent to node 3); removes node 3 with `--force` right
   after it, once (the CLI retries the new leader's transient quorum
   refusal, issue #225); restarts a data directory of another cluster with
   `--join`, which must exit with code 6 (`WrongCluster`; run with a hard
   timeout, and only the process the test spawned is ever killed); and,
   with node 1 killed, checks that `cluster remove 1 --write-deadline 3s`
   is retried for the deadline and then exits 1 with the quorum refusal. The ignored
   `measure_replication` in the same file produces
   [spikes/raft-replication.md](spikes/raft-replication.md) (run it with
   `--release --ignored --nocapture`); the ignored
   `measure_replication_at_scale` produces its "Stage F at scale" section
   (the corpus, or a generated tree of `MG_SCALE_TOKENS` tokens).

The Raft log's size gate is in `crates/graph-cli/tests/size_gate.rs`. After
three corpus passes, `cluster snapshot` and the purge, `raft.redb` must be at
most 1.5x the source indexed. The Docker smoke (above) serves
`--data-dir /data --bootstrap` and checks that `docker start` after a stop is
an idempotent restart.

## Membership and forwarding (ADR 0004 stage C)

`cargo test -p graph-server --test membership` runs on the same
`ClusterTestbed` (helpers shared with `tests/cluster.rs` live in
`tests/support/mod.rs`). `ClusterTestbed::add_node` starts one more node
from `node_config(id, InitMode::Join(..))` (or any `ServeConfig`) and returns
a start-up refusal instead of panicking; `TestNode::set_extractors` rebuilds
a node with another extractor set for its next restart. Each test runs in
well under 30 s (the slowest, the partition, about 5 s):

- `write_via_follower_is_forwarded`: `IndexFile` and a streamed `Index` sent
  to a follower answer `forwarded_to_leader` with the leader's applied
  index; the leader's own write does not; every node holds the data; the
  follower's `writes_forwarded_total` is 2; a linearizable read on the
  follower (the leader's read index) sees a write made on the leader.
- `join_auto_promote_from_empty`, `standby_stays_learner`: a joiner adopts
  the cluster id and becomes a voter once caught up; a standby stays a
  learner with lag zero, serves reads and forwards writes.
- `join_refuses_other_extractor_hash`, `promote_refuses_other_extractor_hash`:
  the leader refuses a joiner with other extractors (nothing is added), and
  a learner restarted with other extractors is refused promotion; unknown
  ids and voters too.
- `restart_with_same_bootstrap_or_join_flags_is_idempotent`,
  `wrong_cluster_is_refused`,
  `join_into_non_empty_store_requires_accept_snapshot_overwrite`: the same
  command lines restart with the same cluster id, members and data; a
  directory of cluster B joining cluster A fails with a typed
  `WrongCluster { expected: A, found: B }` and `node.json` untouched; a
  stray store is refused without `--accept-snapshot-overwrite` and moved
  into `replaced-<secs>/` with it.
- `remove_guards`: the leader, 3 -> 2 without `--force`, an unknown id, and
  below quorum (a voter killed, the leader's `Status` showing its
  `last_error`) even with `--force` are refused (that refusal is
  transient: the client retries it until its 2 s write deadline, then
  reports it as `Rejected`, not `NoLeader`); with every voter back and
  caught up, `--force` works.
- `remove_right_after_a_transfer_is_retried_until_the_new_leader_reaches_its_peers`
  (issue #225): the fault plan holds the new leader's appends to one peer
  for 3 s (`hold_new_term_appends_to`, that peer's own elections off), so
  right after `TransferLeader` a raw `Remove` is refused with a transient
  quorum refusal; the same remove through `RemoteStore` (forwarded by the
  old leader) is retried and succeeds once the peer is matched.
- `remove_lost_self_retries_only_the_transient_refusal`: the same race
  for `serve --bootstrap-or-join`'s removal of a lost incarnation
  (`join::remove_lost_self`): retried until it succeeds, while a final
  refusal (removing the leader) ends it at once. `remove_guards` also
  checks that final refusals come back at once through a client with a
  15 s write deadline, and that only the quorum refusal carries the
  transient flag on the wire.
- `transfer_leader_moves_leadership` (3 nodes) and `..._five_nodes`: sent
  to a follower, forwarded; exactly the target leads (never a third node),
  the old leader follows it, writes work through both; the 3-node case runs
  a writer through the old leader during the transfer (refused with
  `NoLeader` and retried) and moves leadership twice more.
- `a_failed_transfer_disturbs_nobody`: a transfer to a killed target ends
  at once with the leader and the term unchanged on every node; while it
  holds its slot (the `transfer_hold_ms` hook, 3 s) a second transfer is
  refused and a membership change answers `NoLeader`.
- `remove_needs_a_quorum_of_the_old_voter_set_too`: 4 voters, 2 down;
  removing a dead one is refused (joint consensus needs the old set too).
- `a_removed_auto_promote_learner_stays_removed`: its `rejoin` is refused
  and its own re-join loop does not bring it back.
- `follower_linearizable_read_never_misses_an_acknowledged_write`: a
  follower whose appends are dropped answers a `LINEARIZABLE` read with
  `NoLeader`, never stale data, and the new write once healed.
- `a_forward_to_a_hung_leader_times_out`, `a_forward_to_a_dead_leader_is_no_leader`,
  `a_forwarded_request_is_never_forwarded_again`,
  `prune_and_vacuum_sent_twice_are_idempotent`: forwarding's deadline, its
  real transport-failure path (no fault plan), the loop guard, and
  idempotent re-sends.
- `join_request_with_other_extractors_is_refused_without_a_probe`,
  `join_probe_catches_a_node_running_other_extractors`,
  `a_join_that_times_out_names_the_cleanup`: each join guard on its own.
- Every test here arms `support::watchdog` (5 min): a hung test exits the
  binary with its name instead of holding CI.
- `partition_minority_serves_local_reads_refuses_writes_and_converges`:
  `FaultPlan::partition([m], majority)` (the plan also gates forwarding);
  on `m` a write fails with `NoLeader` at the client's 2 s deadline, a
  local read succeeds, a linearizable one fails; the majority keeps
  writing; after `heal()` `m` converges and `run_differential` passes.
- `membership_change_under_load_loses_no_acked_write`: a writer thread
  indexes two-file batches through the leader while a learner is added,
  promoted and a voter removed; every acknowledged batch is on every voter.
- `duplicate_index_chunk_after_leader_change_is_idempotent`: the same batch
  re-sent after a leadership transfer (through the old leader, so it is
  forwarded) is a new log entry that applies as `unchanged`; counts and
  `describe` are unchanged on every node, and `run_differential` against an
  embedded oracle passes.

## Linearizable reads and crash tests (ADR 0004 stage D)

`crates/graph-server/tests/linearizable.rs` runs on the in-process `ClusterTestbed` on every platform (`cargo test -p graph-server --test linearizable`):

- `reads_carry_read_meta`: every read answer has a `ReadMeta` (`mg-read-meta` header); fresh on the leader and on a caught-up follower.
- `linearizable_read_on_lagging_follower_sees_the_write`: the fault plan drops `AppendEntries` to a follower; a `local` read there misses the acked write and reports `stale_possible`; a `linearizable` read parks on the leader's read index (the test waits on `RaftNode::read_index_waits`, never a sleep), the test heals, and the read includes the write.
- `stale_leader_linearizable_read_never_returns_old_data`: the old leader is partitioned away with a connected client, the majority elects and acks a write; linearizable reads on the old leader answer `NoLeader`/`NotLeader` or the new data, never the old; after the heal they see the write.
- `minority_linearizable_read_fails_no_leader`: bounded `NoLeader`, while `local` still answers (stale_possible).
- `old_leader_is_stale_once_a_new_leader_commits`: the leader is partitioned; once the majority elected a new leader and acked a write, one `local` read on the old leader (no retry loop) must say `stale_possible` (the freshness lease ends before any election can).
- `follower_is_stale_soon_after_the_leader_dies`: after the leader is killed a follower's `local` reads turn `stale_possible` within `election_timeout_max`.
- `follower_behind_the_leader_commit_is_stale`: the `testing_apply_gate` hook parks a follower's apply while heartbeats still arrive; its `local` read reports `leader_committed_index > applied_index` and `stale_possible`.
- `linearizable_snapshot_on_lagging_follower_has_the_write`: a linearizable snapshot handle opened on a lagging follower waits for the read index and holds the acked write; its reads stay `stale_possible: false` after the node turns stale (the meta is the handle's, frozen at the open), while a `local` handle opened then says `true`.
- `snapshot_reads_stay_on_the_node_that_opened_the_handle`: the client rotated to another endpoint (a `NoLeader`); reads through the handle still go to its node.
- `client_moves_on_when_its_node_dies` and `a_dead_first_endpoint_is_skipped_fast` (a dead first endpoint costs one 250 ms quick connect, not the 30 s retry budget).
- `history_checker`: three writer threads (one repo each, clients over all three endpoints) and three linearizable reader threads (clients over all three endpoints, each starting on another node) for 10 s, while a seeded fault schedule runs: `AppendEntries` to a random node dropped for 0.3-1.2 s, a leadership transfer to a random voter, or a random node (the leader too) partitioned away for 0.3-1.2 s, each healed before the next. Checked for every read: every write acknowledged before it started is visible to it (linearizability as observed); a read fails only with `NoLeader`/`NotLeader`, never another error, and is never `stale_possible`; `applied_index` never goes back per client per node (a client may move between nodes, which may be at different indexes). Seed and length: `MEMORY_GRAPH_HISTORY_SEED`, `MEMORY_GRAPH_HISTORY_SECS`; a failure prints the seed and the fault list.

Power loss and process kill are covered by `tests/durability.rs` (`PowerCutDisk`: an acked write survives a power cut of every node, a cut mid-write leaves no torn state) and `tests/process_kill.rs` (a killed child process keeps every acked write).

### The `cluster` CI job

`.github/workflows/cluster.yml` (Linux; on push to `main`, nightly, on demand, and on PRs touching any crate the `memory-graph` binary is built from, `Cargo.lock`, the script or the workflow) runs `scripts/cluster_kill_test.py`: three real `memory-graph serve --data-dir` processes (node 1 `--bootstrap`, 2 and 3 `--join <node 1> --auto-promote`; each start retries on a fresh port if its port was taken in the meantime), a writer indexing numbered repos through `--server a,b,c`, alternating one-file `index-file` batches and 20-file `index <dir>` batches, and recording each batch whose command exited 0, and three rounds of: SIGKILL the current leader (`cluster leader --json`), keep writing on the survivors, restart the killed node with no cluster flags, pause the writer and wait until all three report one applied index. At the end every acknowledged batch must be in `describe --json --read local` on every node, and every file of an acknowledged multi-file batch must be there (`describe` file counts per repo). Only processes the script started are killed; every wait has a timeout.

Locally (Windows uses `TerminateProcess`):

```sh
cargo build --release -p graph-cli
python3 scripts/cluster_kill_test.py --bin target/release/memory-graph   # --rounds N, --keep
```

### The soak (`scripts/cluster_soak.py`)

The same three nodes, with the kill test's process machinery (it imports `cluster_kill_test.py`), under a continuous indexing loop for `--minutes` (default 60), one node restarted every `--restart-every` seconds (default 300) round robin, alternating a graceful stop (SIGTERM; Ctrl-Break on Windows, each node in its own process group) and a kill. Each restart waits until the node caught up with the leader's committed index. Every `--sample-every` seconds each node's `cluster status` is recorded (`log_bytes`, `purged_index`, `snapshot_index`, `applied_index`). At the end: every acknowledged batch on every node, every graceful stop exited 0, and on every node the purged index advanced and `log_bytes` stayed under `--max-log-bytes` (default 64 MiB; the nodes run with `--snapshot-log-entries 1000 --log-keep-entries 100` so purges happen in a short run). It prints the samples (`--csv FILE` writes them). The `cluster` workflow runs a 5-minute variant (a restart every minute) on its weekly schedule only; the 60-minute run is local, recorded in [spikes/raft-replication.md](spikes/raft-replication.md).

```sh
python3 scripts/cluster_soak.py --bin target/release/memory-graph              # 60 minutes
python3 scripts/cluster_soak.py --bin target/release/memory-graph --minutes 5 --restart-every 60
```

## Database size

`crates/graph-cli/tests/size_gate.rs` indexes `testdata/corpus` and fails if
the database grows past a fixed multiple of the source bytes (and bytes per
token), before and after `vacuum --compact`, so on-disk cost regressions are
caught in CI. `scripts/measure-size.py` prints the full matrix (commit modes,
re-index, vacuum, compact) as a Markdown table; its numbers are the
provenance for the gate thresholds and for `diskinfo::DISK_RATIO`.
