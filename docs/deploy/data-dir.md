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

### Automatic backups to a directory (`--backup-url`)

```sh
memory-graph serve --data-dir ./n1 ... --backup-url file:///srv/mg-backups --backup-keep 7
```

With `--backup-url file://<dir>` (ADR 0006), each snapshot the leader builds is copied to
`<dir>/<cluster_id>/snap-<term>-<index>.redb`, and then its `.meta` (size, SHA-256, store format,
extractors hash) is written. A backup counts only once its `.meta` exists, so an upload that was
cut short (a crash, a full disk) is never restorable. The directory may be local or mounted.

- `--backup-on leader|all|none` (default `leader`): which nodes upload the snapshots they build.
  With `all`, give each node its own `--backup-url`: the keys carry no node id.
- `--backup-keep N` (default 7, 0 = all): older backups of this cluster are deleted, `.meta`
  first, and only after a newer one committed. Data without a `.meta` older than 24 h (an
  interrupted upload) is swept. Other clusters' prefixes are never touched.
- Uploads run on their own thread, one at a time; a newer snapshot replaces a queued one. They
  never block or delay a snapshot or a log purge. A failed upload is retried 3 times (a 403 or
  another request error that retrying cannot fix is not retried), then logged and counted:
  `mg_backup_failures_total`, and `cluster status` shows `last_backup_error`. The last success
  is `mg_backup_last_success_timestamp` / `mg_backup_last_index`.
- An upload that ran out of retries is not tried again: the next snapshot's upload catches up
  (it carries everything). An upload that fails while a newer snapshot is already queued is
  dropped for the newer one and is not counted as a failure, so `failures_total` counts only
  backups that are actually missing.
- The upload reads the snapshot through an open handle, not a copy: on Windows the handle is
  opened with delete sharing, so (on NTFS with POSIX delete semantics, Windows 10 1809 and later) a newer snapshot can still replace the old one while it is read,
  and the bytes read are checked against the snapshot's SHA-256 before the `.meta` is written.
- All of these are also `serve --config` TOML keys (`backup-url`, `backup-keep`, `backup-on`).

### Automatic backups to S3-compatible storage (`s3://`)

```sh
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
memory-graph serve --data-dir ./n1 ... --backup-url s3://mg-backups/prod \
  --backup-endpoint http://minio:9000
```

The same layout, `.meta` rule, retention and failure handling as `file://`, written through the
S3 REST API with SigV4 (a small built-in client, no AWS SDK). Uploads over 64 MiB go in 16 MiB
parts; a failed multipart upload is aborted. Each request body is signed with its SHA-256, and
the stored size is checked with a HEAD after each upload.

- `--backup-endpoint http://host:port` is required: stage 1 speaks plain HTTP only, and an
  `https://` endpoint is refused. Works with MinIO, Ceph RGW, R2, B2 and Garage. For AWS S3
  itself (HTTPS only), run a TLS sidecar (stunnel) reached under the real S3 host name, or back
  up to `file://` and `aws s3 sync` the directory: see
  [S3 in production](#s3-in-production-tls-lifecycle-and-iam). Native HTTPS waits on a pure-Rust TLS
  provider (#104).
- `--backup-region` (default `us-east-1`) is the region requests are signed for.
- Path-style addressing (`http://host/bucket/key`) is the default; `--backup-virtual-host`
  uses `http://bucket.host/key`.
- Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`),
  which win, else from `--backup-credentials-file <file>` (the AWS INI format) with
  `--backup-profile` (default `default`). They are never taken from a flag or a TOML key (a
  secret-looking key in the config file is refused), never logged, and instance roles (IMDS)
  are not supported.
- Restore straight from S3 into an empty data directory with the same S3 settings:
  `serve --data-dir ./new --node-id 1 --bootstrap --restore s3://mg-backups/prod/<cluster_id>/latest
  --backup-endpoint http://minio:9000` (or `.../snap-T-I.redb`). It is verified like a `file://`
  restore (size, sha256, store format, extractors with `--restore-allow-extractor-mismatch`,
  free disk), downloaded to `<store>.restore.tmp` and removed on any refusal; `latest` falls
  back past orphans and damaged pairs to the highest committed backup that verifies.
- `--restore s3://` and `--backup-url s3://` share one set of S3 flags (`--backup-endpoint`,
  `--backup-region`, `--backup-virtual-host`, `--backup-credentials-file`, `--backup-profile`);
  with a `file://` backup URL they serve the restore alone.
- `memory-graph --server <any node> cluster snapshot --upload` has the leader build a snapshot
  now and upload it (whatever `--backup-on` says), and prints its URL and sha256 (if a newer
  snapshot replaced it first, that one is uploaded and reported). The call waits up to an hour
  and is never resent; if the server answers `DEADLINE_EXCEEDED`, or the client gives up, the
  upload continues in the background (check `cluster status` and `cluster backups`).
- `cluster backups [--json]` lists the committed backups of the cluster in the backup location
  of the node you connect to (its own `--backup-url`), newest first, with the URLs `--restore`
  takes. A `.meta` that cannot be read, or whose data object is missing, is listed with an
  error.
- A multipart upload cut short by a crash or an outage is invisible (no `.meta`) but still
  stored and billed until aborted. Retention aborts those under the cluster's prefix once they
  are older than 24 h. As belt and braces, also give the bucket a lifecycle rule that aborts
  incomplete uploads, e.g. for AWS / MinIO:
  `{"Rules":[{"ID":"abort-mpu","Status":"Enabled","Filter":{"Prefix":"prod/"},"AbortIncompleteMultipartUpload":{"DaysAfterInitiation":2}}]}`
  (`aws s3api put-bucket-lifecycle-configuration --bucket mg-backups --lifecycle-configuration file://rule.json`).
- The request timeout (60 s) bounds connecting, each chunk of a response, and the wait for a
  response, which also allows 1 s per MiB of request body.
- TOML keys: `backup-endpoint`, `backup-region`, `backup-virtual-host`,
  `backup-credentials-file`, `backup-profile`.

### S3 in production: TLS, lifecycle and IAM

**TLS in front of AWS S3.** Stage 1 speaks plain HTTP only (native HTTPS is story 39, deferred
with #104). AWS S3 accepts HTTPS only, so either run a TLS sidecar next to the node, or back up
to `file://` and sync that directory (the simpler of the two).

- **The `Host` header matters.** SigV4 signs the `Host` header memory-graph sends, which is the
  `--backup-endpoint` host (plus `:port` unless it is 80). AWS routes and verifies by that same
  header, so it must be the real S3 name, e.g. `s3.eu-west-1.amazonaws.com`, and no proxy may
  rewrite it (a rewrite fails with `SignatureDoesNotMatch`). So an endpoint of
  `http://127.0.0.1:9080` does **not** work against AWS. Instead, keep the real name on port 80
  and make that name resolve to the sidecar for memory-graph only (#173 tracks a
  `--backup-connect-to` flag that would remove the name trick):
- **stunnel sidecar in Docker Compose** (the sidecar resolves the name normally; memory-graph's
  container maps it to the sidecar with `extra_hosts`):

  `stunnel.conf`:

  ```ini
  foreground = yes
  [s3]
  client = yes
  accept = 0.0.0.0:80
  connect = s3.eu-west-1.amazonaws.com:443
  verifyChain = yes
  CAfile = /etc/ssl/certs/ca-certificates.crt
  checkHost = s3.eu-west-1.amazonaws.com
  ```

  ```yaml
  services:
    s3tls:
      image: debian:stable-slim       # or any image with stunnel installed
      command: sh -c "apt-get update && apt-get install -y stunnel4 ca-certificates && exec stunnel /etc/stunnel/stunnel.conf"
      volumes: ["./stunnel.conf:/etc/stunnel/stunnel.conf:ro"]
      networks: { backup: { ipv4_address: 172.30.0.10 } }
    memory-graph:
      image: ghcr.io/p47phoenix/memory-graph:main
      command: >
        serve --data-dir /data --bootstrap --node-id 1 --listen 0.0.0.0:7000
        --backup-url s3://mg-backups/prod --backup-region eu-west-1
        --backup-endpoint http://s3.eu-west-1.amazonaws.com
      extra_hosts: ["s3.eu-west-1.amazonaws.com:172.30.0.10"]
      env_file: aws-backup.env        # AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
      networks: [backup]
  networks:
    backup: { ipam: { config: [{ subnet: 172.30.0.0/24 }] } }
  ```

  In Kubernetes, the same shape is a stunnel sidecar container plus a `hostAliases` entry, but
  `hostAliases` applies to every container in the pod, so the sidecar must then `connect` to
  a different name for the same region (`s3.dualstack.eu-west-1.amazonaws.com:443`). Use the
  bucket's regional endpoint and path-style addressing (the default). The hop between
  memory-graph and the sidecar is plain HTTP, so keep it on a private network. (This recipe is
  not exercised in CI, which runs SeaweedFS over plain HTTP.)
- Or keep `--backup-url file:///srv/mg-backups` and copy it off the box on a timer:
  `aws s3 sync /srv/mg-backups s3://mg-backups/prod --exact-timestamps` (add `--delete` to let
  retention's deletions follow). A backup is committed by its `.meta`, so a sync that catches an
  upload half-way copies only data without a `.meta`, which is never restored and is swept
  later.

**Bucket lifecycle rule.** Retention deletes old backups and aborts this cluster's stale
multipart uploads itself, but only while a node with `--backup-url` runs. As a backstop, give the
bucket a lifecycle rule that aborts incomplete multipart uploads and expires objects well past
what `--backup-keep` would keep (here 2 days and 90 days; pick the expiration longer than
`--backup-keep` snapshots take to accumulate, or it deletes backups retention meant to keep):

```json
{
  "Rules": [
    {
      "ID": "memory-graph-backups",
      "Status": "Enabled",
      "Filter": { "Prefix": "prod/" },
      "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 2 },
      "Expiration": { "Days": 90 }
    }
  ]
}
```

`aws s3api put-bucket-lifecycle-configuration --bucket mg-backups --lifecycle-configuration
file://rule.json` (AWS), or `mc ilm import local/mg-backups < rule.json` (MinIO).

**Minimal IAM policy.** The node needs to put, get, list and delete objects under its prefix,
and to list and abort multipart uploads. Nothing else (no bucket creation, no ACLs):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "ListThePrefix",
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::mg-backups",
      "Condition": { "StringLike": { "s3:prefix": ["prod/", "prod/*"] } }
    },
    {
      "Sid": "ListMultipartUploads",
      "Effect": "Allow",
      "Action": "s3:ListBucketMultipartUploads",
      "Resource": "arn:aws:s3:::mg-backups"
    },
    {
      "Sid": "ObjectsUnderThePrefix",
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
        "s3:DeleteObject",
        "s3:AbortMultipartUpload",
        "s3:ListMultipartUploadParts"
      ],
      "Resource": "arn:aws:s3:::mg-backups/prod/*"
    }
  ]
}
```

`s3:ListBucketMultipartUploads` does not support the `s3:prefix` condition, so it has its own
statement on the bucket (a condition there would deny it, and retention's multipart sweep with
it). HEAD requests are covered by `s3:GetObject`, and the multipart create/upload-part/complete calls
by `s3:PutObject`. A restore-only machine needs just `s3:ListBucket` and `s3:GetObject`. Add
`s3:x-amz-server-side-encryption` conditions if the bucket requires SSE. The credentials go in
the environment or a credentials file readable only by the service user.

**Testing.** CI's `s3-e2e` job runs `crates/graph-cli/tests/s3_e2e.rs` against SeaweedFS on Linux (MinIO, the ADR's choice, is no longer pullable from Docker Hub):
bootstrap, index the vendored corpus, `cluster snapshot --upload` three times with
`--backup-keep 2`, `cluster backups`, restore `latest` into a new directory and compare the
answers, a wrong secret refused, a real multipart upload, and no multipart upload left in
progress. Run it against any S3-compatible server with `MG_S3_ENDPOINT=http://host:port`
(`MG_S3_BUCKET`, default `mg-e2e`, must exist; `MG_S3_REGION`; `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY`); without `MG_S3_ENDPOINT` it skips.

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

From a `--backup-url` directory, name the backup or `latest` (the highest committed index):

```sh
memory-graph serve --data-dir ./n1 --bootstrap --node-id 1 \
  --restore file:///srv/mg-backups/<cluster_id>/latest --listen 0.0.0.0:7000
```

The `.meta` is read first: the store format must match this binary, and so must the extractors
hash (`--restore-allow-extractor-mismatch` accepts other extractors: the store stays valid and
affected files re-extract on their next index; the format check has no override). The free disk
must cover `--min-free-disk` plus the backup. The data is downloaded to `graph.redb.restore.tmp`
and its size and SHA-256 are checked against the `.meta` before it becomes the store; any
mismatch is refused and the temporary file removed. A refused restore leaves the data directory without a store (the directory itself may remain), and the same command can be retried. With `latest`, a pair that fails verification is skipped for the next-highest committed one. A missing `--backup-url` directory is created on the first upload. A plain `--restore <file>` with a `.meta` next
to it (a copy of a backup pair) is verified the same way; without one it restores with a warning.

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
