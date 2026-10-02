# Measurement: idle CPU and wakeups of `serve` in a container

Status: evidence for issue #205 (stories S1-S3 fixed it, S5 guards it). Date 2026-10-02. Not a decision. The consequence for the design is recorded in [ADR 0004](../adr/0004-client-server-and-replication.md) (Consequences: "A one-member Raft still runs Raft's timers").

**TL;DR (plain language).**
- **The question:** users running `memory-graph serve --db` in Docker Desktop on macOS saw steady host CPU while the server did nothing. Why, and how low can it go?
- **On a Mac the cost of an idle process is its wakeups, not its CPU time.** Each wakeup inside Docker Desktop's VM (a timer, a futex) is a hypervisor exit, billed to `com.docker.virtualization` on the host, not to the container in `docker stats`. So the number to fix is wakeups per second. [E]
- **Before: an idle `serve --db` woke 39.6 times a second** (0.17 % of one core, 34 threads on a 32-vCPU VM). With longer Raft timings by hand it was 8.3/s. [M]
- **The cause was Raft's timers, not the database.** A 50 ms heartbeat made openraft tick every 75 ms; every tick published new Raft metrics, which woke the readiness loop (it re-sent the gRPC health status every pass) and the snapshot-policy loop. A one-node "cluster" ticked although it has nobody to heartbeat. [M]
- **After the fix (PR #208): `serve --db` wakes 3.0 times a second (13x fewer), a one-node `--data-dir` 5.7.** All of it is one thread: openraft 0.9's tick timer, which still wakes every 1.5 heartbeats to see that ticks are off. That is the floor without changing openraft. [M]
- **A regression guard keeps it there:** a Linux test in `serve_e2e` (`--db` under 5/s, `--data-dir` under 8/s) and a step in the Docker workflow (`scripts/idle-cpu.sh`, under 5/s). [M]

**Labels.** [M] measured, [E] estimated or inferred (reason stated). Machine: AMD Ryzen 9 7950X (16 cores, 32 threads), 63 GB RAM, Windows 11 Pro (10.0.26200), Docker Desktop 29.6 with the WSL2 backend (a Linux VM with 32 vCPUs). Not measured on a Mac: the macOS cost model is Docker's documented VM design and the user report, not a measurement here. Other programs were running.

## Method

- Image: `ghcr.io/p47phoenix/memory-graph:main` at f04084d (before), and the same Dockerfile built from the fix branch (after). Each variant runs as its own container with a fresh named volume on `/data`, left idle (no client, nothing indexed) for at least 15 s before measuring.
- Tool: [`scripts/idle-cpu.sh`](../../scripts/idle-cpu.sh) `<container> [seconds] [max]`. The image has no shell, so the script runs an alpine container in the server's pid namespace (`docker run --pid=container:<c>`), where the server is pid 1. It reads every thread's CPU ticks (`/proc/1/task/*/stat`, utime + stime) and context switches (`/proc/1/task/*/status`, voluntary + nonvoluntary) at the start and end of a 60 s window. Wakeups/s is the total of context switches divided by the window. Reading `/proc` from another process does not wake the server.
- A context switch is the right proxy: a thread that sleeps on a timer or futex and wakes up does at least one voluntary switch, and a thread that never wakes does none.

```sh
docker run -d --name mg -v mg-idle:/data memory-graph serve --db /data/g.redb --listen 0.0.0.0:7000
scripts/idle-cpu.sh mg 60
```

## Before (f04084d)

60 s windows. [M]

| `serve` variant | wakeups/s | CPU | threads |
|---|---:|---:|---:|
| `--db` defaults | **39.6** | 0.17 % of a core | 34 |
| `--db --heartbeat-interval 250 --election-timeout-min 1000 --election-timeout-max 2000` | 8.3 | 0.05 % | 34 |
| same + `docker run --cpus=2` | 8.2 | 0.02 % | 4 |
| `--data-dir --bootstrap` | 8.3 | 0.05 % | 34 |

Longer timings alone cut the rate by almost 5x, which pointed at Raft's timers. `--cpus=2` shrank the thread count but not the wakeups: the wakeups came from timers, not from idle workers polling.

## Root cause

All of these are in the server's own tasks; the store itself is quiet.

1. **Test-oriented standalone timings.** `RaftSettings::standalone()` (the `--db` mode) used a 50 ms heartbeat and a 100-200 ms election timeout, chosen so a test server elects itself fast. openraft ticks every 1.5 heartbeats, so `--db` ticked every 75 ms, about 13 times a second. [M]
2. **Every tick publishes `RaftMetrics`,** and two loops watched the metrics channel. The readiness loop re-evaluated readiness and re-sent the tonic-health status on every pass, and tonic-health notifies every watcher even when the status did not change. The snapshot-policy loop also woke and re-checked its thresholds. One tick fanned out into several task wakeups and cross-thread handoffs, about 3 context switches per tick. [M]
3. **A sole voter still ticks.** A one-node cluster has nobody to send heartbeats to and nobody to lose an election to, but openraft keeps ticking. [M]
4. **The tokio pool has one worker per vCPU** (34 threads on 32 vCPUs). Idle workers park and do not poll, so they add little by themselves, but a wakeup handed to another worker costs a switch on each side. [M]
5. **The image's `HEALTHCHECK` starts a client process every 30 s** (`memory-graph health --server 127.0.0.1:7000`): a process start, a TCP connection and a gRPC call, plus Docker's own exec machinery. It is small next to items 1-2 but is outside the server's control. [E]

## Ruled out

- **redb:** it has no background threads and does no I/O while nothing writes; the store is touched only by requests and by Raft applies. [M]
- **Backups, MCP, metrics:** off by default; when on, `--backup-url` uploads only after a snapshot, and the MCP and `/metrics` listeners only wake on an accepted connection. [M]
- **Progress reporters:** they run only during `index`, never in `serve`. [M]
- **rayon:** not used by the server. [M]
- **Database size:** nothing on the idle path depends on how much is stored; the measurements above are on an empty store, and the same timers run on a large one. [E]
- **Other periodic timers:** the snapshot-handle reaper runs every 5 minutes (`SNAPSHOT_REAP_INTERVAL`), negligible. [M]

## The fix (PR #208, stories S1-S3)

- **S1:** a node that leads alone (no other voter, no learner) suspends openraft's tick, and resumes it when the membership grows; every membership change holds ticks on for its duration. A restarted sole voter campaigns at once instead of waiting out an election timeout.
- **S2:** the readiness loop sends a health status only when readiness changes, and a leader has no recheck timer; the snapshot-policy loop has a timer only while a snapshot is pending or refused for disk space. Both are otherwise driven by events.
- **S3:** `--db` timings are now a 500 ms heartbeat and a 2-4 s election timeout (the immediate campaign of S1 keeps start-up fast).

## After

60 s windows, same machine, image built from the fix branch. [M]

| `serve` variant | wakeups/s | CPU | threads |
|---|---:|---:|---:|
| `--db` defaults | **3.0** | 0.033 % | 34 |
| `--db`, `docker run --cpus=2` | 3.0 | 0.017 % | 4 |
| `--data-dir --bootstrap --node-id 1` | **5.7** | 0.033 % | 34 |

The whole remaining rate is on one tokio worker. The `serve_e2e` guard (debug build, 10 s window, run in a `rust:1.98-bookworm` container on the same machine) measured the same: 3.0/s for `--db` and 5.7/s for `--data-dir`. The same test on the code before the fix (f04084d) measured 50.5/s for `--db` and 10.6/s for `--data-dir`, failing both limits (5 and 8). [M]

## The remaining floor

openraft 0.9's tick timer is a task of its own that wakes every 1.5 heartbeats, checks whether ticks are enabled, and goes back to sleep: every 750 ms for `--db`'s 500 ms heartbeat, every 375 ms for the cluster default of 250 ms. Each wake costs about two context switches, which matches the rates above (1.3 and 2.7 timer wakes a second, so about 2.7 and 5.3 switches a second). Removing it would need openraft to stop its timer while ticks are off (or a much longer heartbeat while alone, which openraft cannot change at runtime). It is not worth a fork for a few wakeups a second. [E]

## Why Mac users feel it

On Linux, 40 wakeups a second is 0.17 % of one core and nobody notices. On Docker Desktop for macOS the containers run in a VM (Apple's Virtualization framework). A guest wakeup that needs the host (a timer interrupt, an idle vCPU going back to work) is a VM exit, and the host CPU it costs is charged to `com.docker.virtualization`, not to the container. `docker stats` reads the container's cgroup inside the VM and shows almost nothing, while Activity Monitor shows the VM process busy. So: count wakeups (this script), and judge the result in Activity Monitor, not `docker stats`. [E]

The [Docker guide](../guide/docker.md#idle-cpu-on-macos-docker-desktop) has the user-side settings: the native image architecture, a named volume, `--cpus`, a longer health-check interval.

## Regression guard (S5)

- `crates/graph-cli/tests/serve_e2e.rs`, module `idle` (Linux only): starts the real binary as `serve --db` and as a one-node `serve --data-dir --bootstrap`, lets it settle for 2 s, then sums the context switches of all its threads over 10 s (sampled every 200 ms so a thread that exits mid-window still counts). Limits: `--db` under 5/s, `--data-dir` under 8/s. The rate is driven by timers, so it barely moves: across repeated runs, a CPU-starved `--cpus=2` with CPU hogs, and the whole test binary in parallel, `--db` stayed at 2.9-3.0/s and `--data-dir` at 5.2-5.8/s. Partial reverts measured in review all fail at least the `--db` limit:

  | revert | `--db` | `--data-dir` |
  |---|---|---|
  | ticks never suspended (S1) | 5.6/s | 11.0/s |
  | readiness and policy loops on timers again (S2) | 6.9/s | 8.8/s |
  | S1 and S2 | 6.7/s | 11.4/s |
  | 50 ms `--db` heartbeat (S3) | 26.3/s | (unchanged) |
  | before the fix (f04084d) | 50.5/s | 10.6/s |
- `.github/workflows/docker.yml`, build job: runs the freshly built image as `serve --db /data/g.redb` on a named volume, waits for it to listen and for the health check's start period to pass, then `scripts/idle-cpu.sh <container> 20 5` (3.1/s measured; 5.6/s with S1 and S2 reverted, 39.8/s for the image before the fix).
