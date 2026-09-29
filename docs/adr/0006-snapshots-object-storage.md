# ADR 0006: Snapshots to object storage

**Status:** Accepted (2026-09-29, by the owner). The owner's decisions of 2026-09-29 are recorded below. Builds on [ADR 0004](0004-client-server-and-replication.md) D7 (snapshots). Tracks issue #110. Epic amendment: stories 35-39 in the [epic](../epic-code-memory-graph.md).

## In plain words

1. Today a cluster's only backups are `cluster snapshot --out <file>` and `serve --bootstrap --restore <file>`. Each node keeps just one snapshot on its own disk.
2. This ADR lets the leader copy each snapshot it builds to a backup location, keep the last few there, and restore a new cluster straight from that location.
3. Stage 1 supports a local or mounted directory (`file://`) and S3-compatible object storage over plain HTTP (`s3://`, for AWS through a sidecar, MinIO, Ceph, R2, B2, Garage).
4. Talking to AWS directly over HTTPS needs a TLS library that passes our pure-Rust gate; that is deferred (story 39, shared with #104). Until then, use a TLS sidecar or sync a `file://` directory with `aws s3 sync`.
5. A backup only counts once its small `.meta` file (checksum, size, versions) is written after the data. Restores check that `.meta` before touching anything.
6. The snapshot format does not change, so there is no schema bump.

## Owner decisions (2026-09-29)

- Stage 1 is a `file://` sink plus plain-HTTP S3. Stories S1-S4 (epic 35-38) are in scope; S5 (native HTTPS, epic 39) is deferred and shared with #104.
- Leader-only upload by default, keep 7.
- Server-side encryption (SSE) headers only; no client-side encryption.
- No IMDS or instance-role credentials in stage 1.

## Context

- ADR 0004 D7: a snapshot is a whole-file export, `snap-<term>-<index>.redb`, plus a JSON `.meta` holding sha256, size, term, index, membership, `extractors_hash` and `store_format_version`. One pair is kept per node.
- Today the only backups are `cluster snapshot --out <file>` and `serve --bootstrap --restore <file>`.
- `paths::restore_into` checks only that the file is a store; it does not verify a `.meta`.
- The gate denies `ring`, `aws-lc-sys` and `openssl-sys`. rustls 0.23 needs a `CryptoProvider`, and its defaults (aws-lc-rs, ring) are denied. `aws-sdk-s3`, `object_store` (aws) and `rust-s3` all pull in one of them.

## Decisions

| # | Question | What was decided |
|---|---|---|
| E1 | Scope | Upload each snapshot built (or on `cluster snapshot --upload`), restore by URL, count-based retention; scheduling stays with Raft. |
| E2 | Which node uploads | The leader only by default (`--backup-on leader\|all\|none`). |
| E3 | Providers | A `BackupSink` trait with `file://` and `s3://` sinks. |
| E4 | Client | A small hand-written S3 client with SigV4; no AWS SDK. |
| E5 | TLS | Plain `http://` only in stage 1; `https://` refused with guidance. |
| E6 | Credentials | Env, then a credentials file; never flags or TOML secrets; no IMDS. |
| E7 | Layout | `<prefix>/<cluster_id>/snap-T-I.redb` + `.meta`; data first, `.meta` commits. |
| E8 | Integrity | sha256 on upload; size, sha256 and version checks before any restore. |
| E9 | Retention | `--backup-keep N` (default 7), own prefix only, orphan sweep. |
| E10 | Restore by URL | `--restore s3://.../snap-T-I.redb` or `.../latest`, and `file://`. |

### E1. Scope

- Upload each snapshot the node builds, or only on `cluster snapshot --upload`.
- `--restore <url>` for `s3://` and `file://`.
- Count-based retention on the remote.
- Scheduling stays with Raft's snapshot policy.

### E2. Which node uploads

The leader only by default (`--backup-on leader|all|none`). A term change mid-upload is harmless, because objects are keyed by term and index.

### E3. Providers

An internal trait `BackupSink { put, get, list, delete }` with two sinks:
- `file://<dir>`;
- `s3://bucket/prefix` through the S3 REST API with SigV4 (AWS, MinIO, Ceph RGW, R2, B2, Garage via `--backup-endpoint`).

GCS and Azure are reached through their S3-compatible modes.

### E4. Client

Hand-written in `graph-server/src/backup/`:
- SigV4 with `hmac` + `sha2` (RustCrypto, pure Rust);
- PUT, GET, HEAD, ListObjectsV2, DELETE and multipart;
- over the `hyper` and `http` already in the tree;
- new crates: `hmac` and `quick-xml` (both MIT/Apache); `percent-encoding` is already in the tree through axum/tonic;
- no AWS SDK. About 600-900 lines.

### E5. TLS

Stage 1 accepts `http://` endpoints only. `https://` is refused with guidance pointing to a sidecar (stunnel, envoy, or `aws s3 sync` from a `file://` directory) and to #104. Stage 2 goes behind a `backup-tls` feature once a pure-Rust rustls provider passes the gate (candidates: rustls-rustcrypto, graviola), through spike S5 (story 39).

### E6. Credentials

- Taken from the environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), then from `--backup-credentials-file` (INI with a profile, `--backup-profile`).
- Never flags or TOML keys: the TOML may name the file path only, and a secret in TOML is refused.
- Redacted in logs and in `Debug` output.
- No IMDS.
- `--backup-region` defaults to `us-east-1`. Path-style addressing is the default; `--backup-virtual-host` is optional.
- Server-side encryption headers may be set; there is no client-side encryption.

### E7. Layout

`<prefix>/<cluster_id>/snap-T-I.redb` plus `.meta`, the unchanged sidecar JSON. The data object is uploaded first, then the `.meta`. A backup exists only if its `.meta` exists.

### E8. Integrity

- Upload: stream with sha256 in `x-amz-content-sha256` (or per part), then HEAD to check the size.
- Restore: download to the existing `<store>.restore.tmp` sibling (the name `paths::restore_into` already uses), check size and sha256 against `.meta`, and check `store_format_version` and `extractors_hash` against the binary. Refuse on any mismatch before `restore_into`.
- The `extractors_hash` check is strict by default. An explicit `--restore-allow-extractor-mismatch` overrides it, for restoring onto a binary with upgraded extractors: the store stays valid and the affected files re-extract on the next index, because their fingerprints include the extractor version. Confirmed by the owner on 2026-09-29. The format version check has no override.
- A local `--restore <file>` also verifies a sibling `.meta` when present, and warns when absent.

### E9. Retention

- `--backup-keep N` (default 7, 0 = keep all).
- Deletes older `.meta` objects first, then their data, and only after a newer `.meta` has committed.
- Touches only its own `<cluster_id>` prefix.
- Sweeps orphan data older than 24 h.

### E10. Restore by URL

`serve --bootstrap --restore s3://…/<cluster_id>/snap-T-I.redb`, or `…/latest` (the highest committed index). `file://` works the same way. A restore gets a new cluster id, as today.

## Failure modes

| Failure | Behaviour |
|---|---|
| Partial upload | No `.meta`, so the backup is invisible. A multipart upload is aborted on error; the orphan sweep covers a crash. |
| Upload failure | 3 retries with backoff, then a log line, `mg_backup_failures_total` and `last_backup_error` in `cluster status`. It never blocks or delays a snapshot or purge: uploads are async, one at a time, and a newer snapshot supersedes a queued one. |
| Local snapshot replaced mid-upload | Keep an open handle (Windows share flags) or copy to a `<snapshot>.backup.tmp` sibling first; decided in S1 (story 35). |
| Corrupt download, wrong format or extractors | Refused; the only file written, `<store>.restore.tmp`, is removed. |
| Clock skew | SigV4 `RequestTimeTooSkewed` is surfaced verbatim. |
| Disk | The restore download checks `--min-free-disk` plus the object size first. |

## CLI

- `serve` flags: `--backup-url`, `--backup-endpoint`, `--backup-region`, `--backup-virtual-host`, `--backup-keep`, `--backup-on`, `--backup-credentials-file`, `--backup-profile`. All are TOML keys except secrets.
- `cluster snapshot --upload`.
- `cluster backups [--json]`.
- `--restore` accepts a path, `file://` or `s3://`, including `latest`.
- Metrics: `mg_backup_last_success_timestamp`, `mg_backup_last_index`, `mg_backup_failures_total`, `mg_backup_bytes_total`.

## Testing

- SigV4 against the AWS test vectors, plus proptests for URL parsing and key encoding, credential precedence and redaction.
- `graph-server::testing::FakeS3`: an in-memory hyper S3 with SigV4 checks and fault injection (drop after N bytes, 500 on part k, wrong ETag, 403, slow).
- `ClusterTestbed::with_backup` scenario tests:
  - upload then restore equals the source (`run_differential`);
  - a partial upload is invisible;
  - a corrupt byte is refused;
  - retention keeps N;
  - another cluster's prefix is untouched;
  - a failure does not block purge;
  - `latest` resolution;
  - leader-only upload across a leadership transfer.
- A MinIO CI job on Linux with a service container over plain HTTP.
- The gate stays clean.

## Alternatives rejected

- `aws-sdk-s3`, `object_store` and `rust-s3`: they pull in `ring` or `aws-lc`.
- Shelling out to `aws` or `rclone`.
- Waiting for #104.

## Consequences

- A small hand-maintained S3 client in one module.
- No direct HTTPS to AWS until S5 or #104; the docs must say so.
- The snapshot format is unchanged, so there is no schema bump.
- Wire changes: `last_backup_error` (and the backup metrics' values) in `StatusResponse`, and new Admin RPCs behind `cluster snapshot --upload` and `cluster backups`, regenerated with the xtask. All are additive; `PROTOCOL_VERSION` stays unchanged unless a message's meaning changes.

## Stories (epic 35-39)

| # | Story | Points | Status |
|---|---|---|---|
| 35 | S1: the `file://` sink and verified restore | 5 | |
| 36 | S2: the S3 client over plain HTTP | 8 | |
| 37 | S3: `--restore s3://` + `latest`, `cluster snapshot --upload`, `cluster backups` | 3 | |
| 38 | S4: MinIO e2e CI job + docs | 3 | |
| 39 | S5: native HTTPS spike behind `backup-tls` | 5 | Deferred, shared with #104 |

Acceptance criteria are in the [epic](../epic-code-memory-graph.md).

## Open questions

None blocking. Encryption beyond SSE and IMDS credentials are for future ADRs.
