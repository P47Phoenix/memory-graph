# ADR 0001: Storage engine

**Status:** Accepted (provisional)

> **In plain words**
> - **Problem:** we need somewhere to keep the [symbols](../glossary.md#symbol) and [tokens](../glossary.md#token) we read from code, and to search them fast.
> - **Choice:** use [redb](../glossary.md#redb), a small database written in pure Rust, and build our own [graph](../glossary.md#graph--node--parent) on top of it. Each item remembers its [parent](../glossary.md#graph--node--parent).
> - **Why:** it is [pure Rust](../glossary.md#c-dependency--pure-rust), safe against crashes, and lives in one file. No mature pure-Rust graph database exists.
> - **Cost:** today each token is stored as a big JSON record, so files get large. [ADR 0003](0003-data-model.md) proposes a fix (proposed, not accepted). redb stays either way.

**TL;DR:** redb with a custom graph layer, node rows carrying `parent`. The JSON-node-per-token layout is proposed to be superseded by [ADR 0003](0003-data-model.md).

## Decision

Use `redb` as an embedded key-value store (a database that runs inside our program). We build a graph on top of it with four tables:

- `nodes`: id to node.
- `names`: parent + kind + name to id.
- `children`: one parent to many children (a multimap).
- `tokens_by_text`: token text to many tokens (a multimap).

Each node stores its parent id. So adding up counts from a token to its ancestors ([roll-up](../glossary.md#grain--roll-up)) needs no scan of links.

## Why

- Pure Rust (see [C dependency](../glossary.md#c-dependency--pure-rust)).
- Transactional (see [transaction](../glossary.md#commit--transaction)).
- One file, with an exclusive lock (one process at a time).
- Simple API.
- No mature pure-Rust embedded graph database exists.

## Consequences

- The schema version is stored in a `meta` table and checked before any write (currently 2; version 1 databases are upgraded in place on first open, and older builds refuse version 2).
- Re-indexing a file replaces its whole subtree (the file's symbols and tokens).
- Open: the JSON node encoding is large (see the [storage spike](../spikes/storage.md)).
- Open: a comparison with `fjall` (an LSM-tree database, a different way to lay out data on disk) is pending.

## Details

**Origin marker.**
- File nodes carry an optional `origin`.
- It is `directory` when a directory run wrote the file.
- It is absent for `index-file` and for databases made before `origin` existed. This works through `serde(default)` (a missing field gets its default value), so no schema bump is needed.
- The last ingest wins.
- `index --prune` only removes files marked `directory`. Files added with `index-file` are never pruned, unless a directory run re-indexes them.

**Content fingerprint (skip unchanged files).**
- File nodes carry an optional `fingerprint` (`serde(default)`, no schema bump).
- Its format is `sha256:<hex of source bytes>|<lowercased language>|<extractor version>|<fingerprint format version>`. See [fingerprint](../glossary.md#content-hash--fingerprint).
- SHA-256 comes from the pure-Rust `sha2` crate. We rejected `blake3` because its build script pulls in `cc`, which the C-dependency gate forbids.
- If the stored fingerprint equals the new one, we do not touch the file's subtree. We only refresh `origin`.
- Any difference re-indexes the file fully. That covers content, language, `Extractor::version()` and `FINGERPRINT_FORMAT_VERSION`.
- Files without a fingerprint (older databases, or `ingest_file` with a pre-built extraction) re-index once.

**Deferred: shared content blobs.**
- The fingerprint already identifies content. So a File node could later point to a shared content blob. The blob would be keyed by content hash + language + extractor version, and would own the symbols and tokens.
- Then identical files across repos would store their tokens once.
- Not implemented. It complicates per-file [spans](../glossary.md#span), search roll-ups and pruning. We first need to measure how much duplication real corpora contain.

**Proposed to be superseded in part by [ADR 0003](0003-data-model.md) (proposed, not accepted):** the JSON node-per-token layout. redb stays.
