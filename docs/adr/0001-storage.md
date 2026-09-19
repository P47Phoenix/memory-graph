# ADR 0001: Storage engine

**Status:** Accepted (provisional)

**Decision:** Use `redb` as an embedded key-value store, with a custom graph layered on it: `nodes` (id → node), `names` (parent+kind+name → id), `children` (multimap), `tokens_by_text` (multimap). Each node stores its parent id, so ancestor roll-up needs no edge scan.

**Why:** pure Rust, transactional, single file, exclusive lock, simple API. No pure-Rust embedded graph database exists that is mature.

**Consequences:** schema version is stored in a `meta` table and checked before any write. Re-indexing a file replaces its subtree. Open: JSON node encoding is large (see spike); `fjall` (LSM) comparison is pending.
