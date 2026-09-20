# ADR 0001: Storage engine

**TL;DR:** redb with a custom graph layer, node rows carrying `parent`. The JSON-node-per-token layout is proposed to be superseded by [ADR 0003](0003-data-model.md).

**Status:** Accepted (provisional)

**Decision:** Use `redb` as an embedded key-value store, with a custom graph layered on it: `nodes` (id → node), `names` (parent+kind+name → id), `children` (multimap), `tokens_by_text` (multimap). Each node stores its parent id, so ancestor roll-up needs no edge scan.

**Why:** pure Rust, transactional, single file, exclusive lock, simple API. No pure-Rust embedded graph database exists that is mature.

**Consequences:** schema version is stored in a `meta` table and checked before any write. Re-indexing a file replaces its subtree. Open: JSON node encoding is large (see spike); `fjall` (LSM) comparison is pending.

**Origin marker:** file nodes carry an optional `origin` (`directory` when written by a directory run; absent for `index-file` and for databases created before it existed, via `serde(default)`, so no schema bump). The last ingest wins. `index --prune` only removes files marked `directory`, so files added with `index-file` are never pruned unless a directory run re-indexes them.

**Content fingerprint (skip unchanged files):** file nodes carry an optional `fingerprint` (`serde(default)`, no schema bump): `sha256:<hex of source bytes>|<lowercased language>|<extractor version>|<fingerprint format version>`. SHA-256 comes from the pure-Rust `sha2` crate (`blake3` was rejected: its build script pulls in `cc`, which the C-dependency gate forbids). If the stored fingerprint equals the new one the file's subtree is not touched and only `origin` is refreshed; any difference (content, language, `Extractor::version()`, `FINGERPRINT_FORMAT_VERSION`) re-indexes it fully. Files without a fingerprint (older databases, `ingest_file` with a pre-built extraction) re-index once.

**Deferred: shared content blobs.** Because the fingerprint already identifies content, a File node could later point to a shared content blob (keyed by content hash + language + extractor version) that owns the symbols and tokens, so identical files across repos store their tokens once. Not implemented: it complicates per-file spans, search roll-ups and pruning, and needs a measurement of how much duplication real corpora contain first.

**Proposed to be superseded in part by [ADR 0003](0003-data-model.md) (proposed, not accepted):** the JSON node-per-token layout. redb stays.
