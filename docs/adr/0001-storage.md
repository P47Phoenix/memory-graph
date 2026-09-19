# ADR 0001: Storage engine

**Status:** Accepted (provisional)

**Decision:** Use `redb` as an embedded key-value store, with a custom graph layered on it: `nodes` (id → node), `names` (parent+kind+name → id), `children` (multimap), `tokens_by_text` (multimap). Each node stores its parent id, so ancestor roll-up needs no edge scan.

**Why:** pure Rust, transactional, single file, exclusive lock, simple API. No pure-Rust embedded graph database exists that is mature.

**Consequences:** schema version is stored in a `meta` table and checked before any write. Re-indexing a file replaces its subtree. Open: JSON node encoding is large (see spike); `fjall` (LSM) comparison is pending.

**Origin marker:** file nodes carry an optional `origin` (`directory` when written by a directory run; absent for `index-file` and for databases created before it existed, via `serde(default)`, so no schema bump). The last ingest wins. `index --prune` only removes files marked `directory`, so files added with `index-file` are never pruned unless a directory run re-indexes them.
