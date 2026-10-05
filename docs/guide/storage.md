# Storage

What the database file holds, how big it gets, and how to give space back.

- **Format.** One redb file holding an interned dictionary, one compact stream per file with sparse checkpoints, and count postings ([ADR 0003](../adr/0003-data-model.md)).
- **Size.** About 10x the source (measured 10.2x) and 70 bytes per token on the small test corpus (40 bytes per token at 10 M tokens, where page and dictionary overhead amortise). `scripts/measure-size.py` prints the full table and `crates/graph-cli/tests/size_gate.rs` enforces the ratio in CI (13x, 90 B/token; a reindex plus compact must stay within 1.2x of a fresh index).
- **Growth and reclaiming space.** Unchanged files add nothing on a rerun. `--reindex` can double the file until `vacuum --compact`, since redb reuses freed pages but never shrinks the file (and grows a file under 4 GiB by doubling it); an embedded `--reindex` run that replaced files prints a hint when the file ends at 2.5x its live data or more (a fresh or compacted file measures about 1.65x). `vacuum` frees dictionary terms after churn; `--compact` rebuilds the file.
- **Catalog.** `describe` and filter validation read a small counter catalog kept in step with every write, so they cost O(repos), not O(tokens). It is part of the format, written from the first index.
- **Page cache.** redb keeps recently read pages in memory. Without `--cache-bytes` (or serve's `cache-bytes`) the cache is a quarter of the memory available when the store opens, clamped to 64 MiB..4 GiB, or 256 MiB if the platform cannot report memory ([ADR 0008](../adr/0008-read-cache.md) phase 1); the cache is split 9:1 between reads and writes. An explicit size always wins.
- **Versioned on disk.** Any change to the stored bytes bumps the schema version; a file from another version is refused without being written to.

## Schema 12: source encodings (upgrade notes)

Schema 12 ([ADR 0007](../adr/0007-source-encodings.md) C6) lets a File record carry its source `encoding` and `lossy` flag, and adds per-repo encoding and lossy counts to the catalog behind `describe`.

- **Upgraded on open.** A schema 9, 10 or 11 database is upgraded the first time a schema-12 binary opens it, in one small commit, with no re-index: 9 and 10 are only restamped (everything in them is UTF-8); 11 also has its encoding counts recounted from its File nodes. UTF-8 files keep their bytes and fingerprints, so the next `index` re-parses none of them.
- **Older binaries refuse it.** Once upgraded, a binary older than schema 12 refuses the file with a schema mismatch and leaves it untouched. Keep a copy before the first open if you may need to roll back.
- **No rolling cluster upgrade.** The schema change and the decoder version both change the cluster's extractor hash, so old and new nodes cannot share a Raft log. Upgrade every node together, or snapshot, upgrade and restore (`serve --bootstrap --restore`).
- **Backups.** A backup or snapshot taken before the upgrade (schema 9, 10 or 11) restores into a schema-12 server and is upgraded on open, with correct encoding counts.
- **Anything else** (older than 9, newer than 12, or the retired v1 format) is refused without being written to.

## The v1 format is retired (2026-09-25)

The original per-node layout cost about 525 bytes per token (a fresh index of a 10 GB tree reached 420 GB). Opening a v1 file fails with a message naming its schema version and leaves it untouched. Re-index from source into a new file, or convert it with the last v1-capable release, git tag `v1-last`, using `memory-graph migrate <new.redb>`.

`--backend v2` is accepted as a no-op, `--backend v1` is an error; `--v2-chunk-bytes`/`--v2-cache-bytes` are now `--chunk-bytes`/`--cache-bytes` (old spellings still work).
