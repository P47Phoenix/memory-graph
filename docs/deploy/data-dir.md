# The data directory: layout, backup and restore

**TL;DR.** A cluster node (`serve --data-dir <dir>`) keeps everything in one directory: its
identity (`node.json`), the store (`graph.redb`), the Raft log (`raft.redb`), the latest
snapshot (`snapshots/`) and a `LOCK` file. Back up a cluster with
`cluster snapshot --out backup.redb` against any node; restore by bootstrapping a new cluster
from it with `serve --data-dir <empty dir> --bootstrap --restore backup.redb`. Design: ADR 0004
D6, D7.

## Layout

| Path | What | Notes |
|---|---|---|
| `node.json` | Node id, cluster id, advertised address, binary / protocol / store format versions, extractor version set hash, creation time | Written on the first start. A different `--node-id` or `--advertise` on a later start is refused; `--update-advertise` rewrites the address once the cluster recorded it. |
| `graph.redb` | The store: every applied log entry, and the index of the last one applied | What reads answer from. Same format as an embedded `--db` file. |
| `raft.redb` | The Raft log, the vote and the committed index | Purged below each snapshot (keeping `--log-keep-entries`), then compacted. |
| `snapshots/snap-<term>-<index>.redb` + `.meta` | The latest snapshot: a copy of the store at a log index, with its SHA-256, size and membership | One pair kept; a follower too far behind is sent it. |
| `LOCK` | `{"pid", "listen", "started"}` of the running server | Removed on a graceful stop; a stale one (dead pid) is ignored. |
| `replaced-<time>/` | What `--join --accept-snapshot-overwrite` moved aside | Only after that flag; delete when no longer needed. |

The whole directory belongs to one node. Do not copy it to start another node (two nodes with one
id break Raft's safety); start the other node empty and let it `--join`.

In a container the directory is `/data` (a named volume in Compose, a PersistentVolumeClaim in
Kubernetes), owned by the image's user 65532.

## Disk

Plan for the store, plus one snapshot copy (the same size), plus the log (up to
`--snapshot-log-bytes`, default 1G, before a snapshot purges it), plus a transient second copy while
a snapshot is received. The disk guard (`--min-free-disk`, default 5% of the volume between 2G and
32G) refuses writes with `RESOURCE_EXHAUSTED` before the volume fills; `mg_store_bytes` and
`mg_log_bytes` on `/metrics` show the two files.

## Backup

```sh
memory-graph --server <any node> cluster snapshot --out backup.redb     # builds a snapshot now and downloads it
```

The download is checked against the snapshot's SHA-256 and size. The file is a complete store at
one log index: it opens embedded (`memory-graph --db backup.redb describe`) and answers every
query as the cluster did at that index. Taking it from a follower is fine; it is at most a little
behind the leader.

A file-level copy of `graph.redb` from a stopped node also works, but a running node's files are
not a consistent backup.

## Restore

A restore creates a **new** cluster (a new cluster id and a fresh log) whose store is the backup:

```sh
memory-graph serve --data-dir ./n1 --bootstrap --node-id 1 --restore backup.redb --listen 0.0.0.0:7000
memory-graph serve --data-dir ./n2 --node-id 2 --join n1:7000 --auto-promote --listen 0.0.0.0:7000
memory-graph serve --data-dir ./n3 --node-id 3 --join n1:7000 --auto-promote --listen 0.0.0.0:7000
```

`--restore` works only with `--bootstrap` and only into an empty directory; the file is checked to
be a store of the current format. Nodes of the old cluster refuse the new one (`WrongCluster`),
so wipe their directories before they join it.

In Compose: `down -v`, then start node1 once by hand with `--restore` on its volume (for example
`docker compose run --rm -v "$PWD/backup.redb:/backup.redb:ro" node1 serve --data-dir /data
--bootstrap --node-id 1 --restore /backup.redb ...`, stop it once it prints `listening on`), then
`up -d --wait`. In Kubernetes: scale to 0, delete the claims, restore into pod 0's new claim with
a one-off pod running the same command, then scale back to 3.

## Moving a node

A node's advertised address is part of the membership. To move a node to another address (a new
IP, port or DNS name), restart it with `--update-advertise <host:port>`:

```sh
memory-graph serve --data-dir ./n3 --listen 0.0.0.0:7013 --update-advertise host3:7013
```

Once it serves at the new address, the node asks the leader (through itself or any member its
membership lists) to record the address: the leader asks the server there who it is (it must be
this node, of this cluster, with the same extractors) and commits one membership entry that
replaces the address (voters and learners unchanged). An address another member has recorded,
even one that is down, is refused. Only then is `node.json` rewritten. If no
leader accepts it within 2 minutes, the start fails and `node.json` keeps the old address; run the
same command again. A plain `--advertise` with another address is still refused. Until the leader
has the new address it cannot reach the node, so move one node at a time and let it rejoin before
the next. Removing the node (`cluster remove <id>`), wiping its directory and joining again under
the new address also works, at the cost of a full copy.

**A crash mid-move.** If the node stops after the cluster committed the new address but before
`node.json` was rewritten, `node.json` still names the old address while the membership (in the
node's own log) names the new one. A plain restart then refuses to start, because it would serve at
the old address while the leader replicates to the new one:

```text
this node's address in node.json is host3:7003, but the cluster's membership records node 3 at
host3:7013 (an --update-advertise that stopped after the cluster committed it); restart with
--update-advertise host3:7013, listening where host3:7013 reaches, to finish the move
```

Run the command it names (the one you ran before, with the same `--update-advertise`). The leader
already has that address, so it only confirms it, and then `node.json` is rewritten. To go back to
the old address instead, run `--update-advertise <old address>`.

`node.json` is rewritten only once the node's own log holds the new address, so a restart right
after a successful move is never refused. A node whose log is merely behind (its membership still
lists an older address) is not refused either: before refusing, a restart serves and waits a few
seconds for the leader to catch it up, then checks again.

**Addresses are compared as exact strings.** The check above, and the leader's own checks, compare
the address in `node.json` (`--advertise` / `--update-advertise`) with the one in the membership
character for character. `localhost:7003` and `127.0.0.1:7003` are different addresses, as are
`[::1]:7003` and `::1:7003`. If you add a node by hand with `cluster add-learner <id> <addr>`,
spell `<addr>` exactly as that node's `--advertise`, or its next restart is refused.

## Stopping a node

A graceful stop drains in-flight requests, shuts Raft down and closes the store, then removes
`LOCK`. It is triggered by Ctrl-C or SIGTERM (`docker stop`, Kubernetes) on Unix, and by Ctrl-C or
Ctrl-Break on Windows. A supervisor on Windows starts the server in its own process group
(`CREATE_NEW_PROCESS_GROUP`) and sends it Ctrl-Break (Python:
`proc.send_signal(signal.CTRL_BREAK_EVENT)`), as `scripts/cluster_soak.py` does. `taskkill /F` and
`TerminateProcess` are kills: the node recovers from its log on the next start, but it does not
drain.
