# Cluster

`serve --data-dir` runs a node that replicates through Raft (ADR 0004 D5-D9). Every write is a log entry that a majority fsyncs before it is acknowledged, and every node answers reads from its own copy. Nodes join with `--join`, and any node takes writes and membership commands (forwarding them to the leader).

Everything in the [server guide](server.md) (targets, exit codes, write deadlines) applies here too. The data directory itself (layout, disk planning, backup, restore, moving a node) is documented once, in [docs/deploy/data-dir.md](../deploy/data-dir.md).

```sh
memory-graph serve --data-dir ./n1 --bootstrap --node-id 1 --listen 127.0.0.1:7001   # a NEW one-node cluster (prints a warning saying so)
memory-graph --server 127.0.0.1:7001 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7001 cluster status          # role, term, leader, log, snapshot, members, lag
memory-graph --server 127.0.0.1:7001 cluster snapshot --out backup.redb
memory-graph serve --data-dir ./n1 --listen 127.0.0.1:7001   # later: a restart needs no flags
```

## Starting a node

- **Data directory.** It holds `node.json`, `graph.redb`, `raft.redb`, `snapshots/` and `LOCK` ([layout](../deploy/data-dir.md#layout)). `--data-dir` and `--db` are exclusive.
- **Start-up.** `--bootstrap` on an empty directory creates a new cluster with a random id and this node as its only voter. On a directory that is already initialized it is a plain restart, so a container can keep the flag in its command. With neither flag, an initialized directory restarts from its state and an empty one is refused ("pass --bootstrap"). `--node-id` is required on the first start and then read from `node.json`; a different id is refused.
- **`--advertise HOST:PORT`** is the address peers and clients reach this node at. The default is the listen address, with `0.0.0.0` or `[::]` replaced by the host name. It is stored in `node.json` and in the cluster membership, so a restart with a different `--advertise` is refused. To change it, see [moving a node](../deploy/data-dir.md#moving-a-node) (`--update-advertise`).
- **`cluster status [--json]`** adds, for a data-dir node: role, cluster id, advertised address, snapshot and purged indexes, every member with its role and address, the leader's per-peer replication lag, and the log and store file sizes. **`cluster snapshot [--out FILE] [--json]`** builds a snapshot now. With `--out` it downloads the file to this machine and verifies its SHA-256 and size.

## Tuning: log, snapshots, timing, disk

- **Log and snapshots.** A snapshot is built after `--snapshot-log-entries` (default 10000) applied entries or `--snapshot-log-bytes` (default 1G) of log. The log below it is then purged, keeping `--log-keep-entries` (default 1000) so a briefly lagging follower catches up from the log, and `raft.redb` is compacted.
- **Timing.** `--heartbeat-interval` (ms, default 250), `--election-timeout-min` / `--election-timeout-max` (ms, default 1000 / 2000).
- **Disk guard.** `--min-free-disk` (a size, or a percentage of the volume; the default for a data directory is 5% of it, between 2G and 32G, and off for `--db`). Writes and snapshot builds are refused with `RESOURCE_EXHAUSTED` while less than that plus one snapshot copy is free.

### Quorum loss

A leader that has heard from no quorum for `--quorum-loss-timeout` (ms, default max(election timeout max, 2 × heartbeat)) answers its pending writes `NoLeader`, so on a minority partition a client's write fails with exit code 4 instead of hanging. That takes at most about its write deadline plus 1.5x the window (the window, then a health probe of the silent voters of up to half of it); with a 1 s write deadline and the 2 s default window, expect up to 4 s. Slow but healthy writes are not affected. `NoLeader` does not mean the write was not applied; retries are idempotent.

A real network partition is bounded this way; a peer whose process answers health checks but cannot append (disk full, a stuck apply, a middlebox dropping only Raft RPCs) still keeps a pending write waiting, as before.

## Configuration file

`serve --config serve.toml` (or `MEMORY_GRAPH_CONFIG`) reads the same settings from TOML: every `serve` flag is a key of the same name, kebab-case or snake_case, plus `db` and `cache-bytes`; a switch is `true`/`false`, a list an array. A flag on the command line, or its environment variable (`MEMORY_GRAPH_LOG`), overrides the file, and the file overrides the defaults. An unknown key is an error, and the file's values go through the flags' own checks. For example:

```toml
data-dir = "/var/lib/memory-graph"
listen = "0.0.0.0:7000"
advertise = "node1.internal:7000"
node-id = 1
bootstrap = true
metrics-listen = "0.0.0.0:9100"
log-format = "json"
```

## Backups

`cluster snapshot --out FILE` downloads a consistent copy of the store from any node; `serve --data-dir <empty dir> --bootstrap --node-id 1 --restore FILE` seeds a new cluster from it. `serve --backup-url file://<dir>` or `s3://bucket/prefix` copies every snapshot the leader builds (ADR 0006). The full reference (retention, failure handling, restore checks, S3 flags and credentials, and the production TLS, lifecycle and IAM notes) is in [docs/deploy/data-dir.md](../deploy/data-dir.md#backup).

### Walkthrough: backups to S3-compatible storage

`--backup-url s3://bucket/prefix` writes backups to an S3-compatible server over plain HTTP: MinIO, Ceph RGW, R2, B2, Garage, or AWS S3 through a TLS sidecar (native HTTPS waits on #104). A walkthrough on one machine with SeaweedFS's S3 gateway in Docker (which turns SigV4 auth on from the two variables; any S3-compatible server works the same way):

```sh
export AWS_ACCESS_KEY_ID=mg-demo AWS_SECRET_ACCESS_KEY=mg-demo-secret-123
docker run -d --name s3 -p 8333:8333 -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY chrislusf/seaweedfs:4.48 server -s3 -dir=/data
until docker exec s3 sh -c "echo 's3.bucket.create -name mg-backups' | weed shell -master=localhost:9333" 2>&1 \
  | grep -q "created bucket"; do sleep 1; done   # retries until SeaweedFS is up

memory-graph serve --data-dir ./n1 --bootstrap --node-id 1 --listen 127.0.0.1:7001 \
  --backup-url s3://mg-backups/prod --backup-endpoint http://127.0.0.1:8333 &
memory-graph --server 127.0.0.1:7001 index --org acme --repo api ./api
memory-graph --server 127.0.0.1:7001 cluster snapshot --upload   # upload now; prints the URL and sha256
memory-graph --server 127.0.0.1:7001 cluster backups             # newest first, with the URLs --restore takes

# Rebuild from the newest backup into an empty directory (a new cluster):
CLUSTER=$(memory-graph --server 127.0.0.1:7001 cluster status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["cluster_id"])')
memory-graph serve --data-dir ./restored --bootstrap --node-id 1 --listen 127.0.0.1:7002 \
  --restore s3://mg-backups/prod/$CLUSTER/latest --backup-endpoint http://127.0.0.1:8333 &
memory-graph --server 127.0.0.1:7002 describe                    # the same answers as 127.0.0.1:7001
```

The leader also uploads every snapshot it builds (`--backup-on leader`, the default) and keeps the newest 7 (`--backup-keep`). Credentials come from the environment or `--backup-credentials-file`, never from a flag. For production, see [S3 in production](../deploy/data-dir.md#s3-in-production-tls-lifecycle-and-iam) (the TLS sidecar recipe for AWS, the bucket lifecycle rule and a minimal IAM policy). CI runs this path against SeaweedFS on every push (`s3-e2e` job, `crates/graph-cli/tests/s3_e2e.rs`).

Measurements (ingest on one and three nodes, snapshots, read latency, the corpus and 10 M tokens, a 60-minute soak): [docs/spikes/raft-replication.md](../spikes/raft-replication.md).

## More nodes: join, promote, remove

```sh
memory-graph serve --data-dir ./n2 --node-id 2 --listen 127.0.0.1:7002 --join 127.0.0.1:7001 --auto-promote
memory-graph serve --data-dir ./n3 --node-id 3 --listen 127.0.0.1:7003 --join 127.0.0.1:7001 --auto-promote
memory-graph --server 127.0.0.1:7003 cluster members          # 1 voter ... (leader), 2 voter ..., 3 voter ...
memory-graph --server 127.0.0.1:7002 index --org acme --repo api ./api   # a follower: forwarded to the leader
memory-graph --server 127.0.0.1:7002 cluster transfer-leader 2
memory-graph --server 127.0.0.1:7001 cluster remove 3 --force
```

### Joining

- **`--join <host:port>`** names any member (it forwards to the leader). On an empty directory the node asks to be added as a learner, takes the cluster id, and catches up from the leader's log or snapshot; it prints its `listening on` line once it is in. It retries while no leader answers, up to `--join-timeout` (default `2m`). The leader refuses a node with another extractor version set, store format or protocol version, a node id that is already a member at another address, and a node id that is already a voter (an empty directory under a voter's id would have lost its vote and log: `cluster remove` it first).
- **`--auto-promote`** makes the node a voter once its replication lag is zero (the leader does it; a node restarted while still a learner asks again). **`--standby`**, the default without `--auto-promote`, keeps it a learner: a read replica that replicates everything, answers reads and forwards writes, until `cluster promote <id>`.
- **Idempotent restarts.** `--bootstrap` and `--join` on a directory that already belongs to the cluster are plain restarts, so a container keeps the same command line. A directory of another cluster is refused with `WrongCluster` (exit code 6) before anything is opened. A directory that holds a store or a log but no `node.json` (a `--db` file copied in, say) is refused unless `--accept-snapshot-overwrite`, which moves what it holds into `replaced-<time>/` and joins empty.

### Writes and reads through any node

- **Writes through any node.** A write sent to a follower or learner is forwarded to the leader by the server (an `index` streams through, a few files at a time), and the answer is the leader's (`forwarded_to_leader`; `cluster status --json` counts `writes_forwarded_total`). With no leader known the server answers `NoLeader` and the client retries until `--write-deadline` (then exit code 4); a node cut off from the majority refuses writes this way while it keeps answering `local` reads. Membership commands are forwarded the same way, so `--server` may name any node.
- **`--read local`** (the default) answers from the node's own store at once, even with no leader; it may miss the latest acknowledged writes. With `--json` the output of `search`, `symbols` and `describe` then says `"stale_possible": true`: the node knows no leader, has not heard from it within the freshness lease (`election_timeout_min` minus two heartbeats), or has not applied what the leader last reported committed; `false` otherwise. `"stale_possible": false` is a best-effort hint, not a guarantee: detection lags by up to the lease (after a leader dies, or while a leader is cut off from the majority). Output of an embedded `--db` run has no `stale_possible`.
- **`--read linearizable`** sees every write acknowledged before the read began: on a follower the node asks the leader for its read index and answers once it has applied it. With no reachable leader (a node cut off in a minority, an election) it fails with `NoLeader` (exit code 4) instead of answering stale data, once `--read-deadline` (or `MEMORY_GRAPH_READ_DEADLINE`, default 5s; it also bounds the first connect) has passed. Only `--read linearizable` guarantees freshness.
- **Several nodes in `--server`.** `--server a:7000,b:7000,c:7000` (or the same in `MEMORY_GRAPH_SERVER`) uses the first node that answers and moves on to the next when one is down or has no leader, so a script keeps working while a node restarts.

### Membership commands

- **`cluster members [--json]`** lists every member with its role and address and marks the leader.
- **`cluster add-learner <id> <host:port> [--no-wait]`** adds a node that is already serving (the node is asked who it is first: another node id, another cluster or other extractors are refused).
- **`cluster promote <id>`** makes a learner a voter; it is refused for an unknown node or a node whose extractor version set differs (asked again at promotion time).
- Membership changes are idempotent under a retry: promoting a voter, adding a learner already listed at the same address, and removing a node that is not a member succeed without a change. A change (`add-learner`, `promote`, `remove`) that times out may still commit later: check `cluster members` before retrying.
- **`cluster remove <id> [--force]`** is refused for the leader ("transfer leadership first"), for any removal after which the voters reachable right now would be fewer than a quorum of the remaining voters or of the current ones (`--force` does not override this), and for 3 voters down to 2 unless `--force` (two voters tolerate no failure). A learner is removed without these checks.
- **`cluster transfer-leader <id>`** hands leadership to a voter that has caught up. openraft 0.9 has no transfer of its own, so the leader pauses new writes and membership changes (clients retry, as in an election), lets what is in flight finish, pauses its heartbeats and asks the target to call an election every 50 ms; the target wins as soon as the leader lease (`--election-timeout-max`) runs out, before any other node campaigns. Only one transfer runs at a time; if the target cannot be reached the transfer ends at once and the old leader keeps leading undisturbed, and if it does not take over within 20 s the old leader resumes.
- **Exit code 6:** the data directory belongs to another cluster than `--join`'s peer (or a node named in `add-learner` does).

## Deployment

- [docs/deploy/compose.md](../deploy/compose.md): a three-node cluster with `deploy/compose/cluster.yml`, checked end to end in CI.
- [docs/deploy/kubernetes.md](../deploy/kubernetes.md): a StatefulSet from `deploy/kubernetes/`, with `serve --node-id-from-hostname --bootstrap-or-join <pod 0>`; a pod 0 that lost its volume asks the other pods and rejoins their cluster instead of creating a second one.
- [docs/deploy/data-dir.md](../deploy/data-dir.md): the data directory, backup and restore, moving a node.
- [Observability](observability.md): logs, metrics and health probes.
