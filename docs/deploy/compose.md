# Running a cluster with Docker Compose

**TL;DR.** `docker compose -f deploy/compose/cluster.yml up -d --wait` starts three nodes on
one machine: node1 bootstraps, node2 and node3 join it and become voters once caught up. Clients
use `127.0.0.1:7001`, `:7002` or `:7003` (any node: writes are forwarded to the leader).
`down -v` deletes everything. The same file is exercised end to end in CI by
`deploy/compose/check.sh` (`.github/workflows/compose.yml`). Design: ADR 0004 D10, epic story 24.

## Start, use, stop

```sh
docker compose -f deploy/compose/cluster.yml up -d --wait     # builds the image from this repository
memory-graph --server 127.0.0.1:7001 cluster members          # 1 voter (leader), 2 voter, 3 voter
memory-graph --server 127.0.0.1:7002 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7003 --read linearizable search Subscribe
curl -s 127.0.0.1:9101/metrics | grep mg_raft_                # Prometheus text, one port per node
docker compose -f deploy/compose/cluster.yml logs -f node2     # JSON log lines
docker compose -f deploy/compose/cluster.yml down              # stop; the volumes keep the data
docker compose -f deploy/compose/cluster.yml down -v           # stop and delete the data
```

To run the published image instead of building one:
`MG_IMAGE=ghcr.io/p47phoenix/memory-graph:main docker compose -f deploy/compose/cluster.yml up -d --wait --no-build`.

## What the file sets up

| | node1 | node2 | node3 |
|---|---|---|---|
| Start-up | `--bootstrap` (a new cluster) | `--join node1:7000 --auto-promote` | `--join node1:7000 --auto-promote` |
| Node id / advertised address | 1 / `node1:7000` | 2 / `node2:7000` | 3 / `node3:7000` |
| gRPC port on the host | 7001 | 7002 | 7003 |
| `/metrics` on the host | 9101 | 9102 | 9103 |
| Override the host ports | `MG_GRPC_PORT_1`, `MG_METRICS_PORT_1` | `..._2` | `..._3` |
| Volume (`/data`) | `node1-data` | `node2-data` | `node3-data` |

- **Health.** Every node's healthcheck is `memory-graph health --ready --server 127.0.0.1:7000`,
  the `memory-graph.ready` gRPC health service: `SERVING` only while a leader is known and the node
  heard from it within three election timeouts, and has applied to within `--ready-max-lag`
  (default 1000) entries of the leader's commit index.
  node2 and node3 start once node1 is healthy (`depends_on: condition: service_healthy`), and
  `up --wait` returns once all three are. The image's own `HEALTHCHECK` (`health` without
  `--ready`: the process serves) stays for single-container use.
- **Restarts are plain restarts.** `--bootstrap` and `--join` on a data directory that already
  belongs to the cluster resume from it, so `docker compose stop/start/restart`, a crash
  (`restart: unless-stopped`) or a host reboot need no other command line.
- **Stopping one node** leaves two voters, a quorum: writes and linearizable reads go on through
  the other two. With two nodes stopped the last one keeps answering `local` reads and refuses
  writes (`NoLeader`, exit code 4 after `--write-deadline`) until a second one is back.
- **Disk reserve.** The file passes `--min-free-disk 512M` so a laptop or CI runner does not
  trip the default reserve (5% of the volume, at least 2 GiB).
- **Logs** are JSON (`--log-format json`), level `info` (`MEMORY_GRAPH_LOG=debug docker compose
  ... up` for RPC and apply spans).

## The CI check

`deploy/compose/check.sh` (run from the repository root, with a built CLI:
`cargo build --release -p graph-cli`) does what the `compose` CI job does: `up -d --wait`;
waits for three voters (`cluster members` through node1); indexes `testdata/corpus` through
node2; compares node3's answers (`--read linearizable`) with an embedded run of the same
indexing; scrapes every node's `/metrics`; stops node1, writes through node2 and reads the write
through node3; starts node1 and waits until all three report the same applied index
(`cluster status --json`); `down -v`. It runs as its own Compose project
(`CHECK_PROJECT`, default `memory-graph-check-<pid>`) on its own host ports (`CHECK_PORT_BASE`,
default 17000: gRPC 17001-17003, metrics 19101-19103), so its `down -v` never touches a cluster
started from this file the documented way (project `memory-graph`). On failure it prints the
container logs. `KEEP=1` leaves
the cluster running; `MG=<path>` picks the CLI; `EMBEDDED_IN_IMAGE=1` runs the embedded side in
the image (a host whose platform differs from the containers').

See also [kubernetes.md](kubernetes.md) and [data-dir.md](data-dir.md).
