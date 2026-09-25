//! The backend-agnostic store interface (ADR 0003 story 1).
//!
//! Two traits, so a consistent read view can be handed out on its own:
//!
//! * [`StoreRead`]: every read operation. Implemented by the stores and by the
//!   snapshot handle returned from [`Store::snapshot`]. A multi-call query
//!   (paging, traversal) takes one snapshot and reads everything from it.
//! * [`Store`]: `StoreRead` plus the write operations and `snapshot()`.
//!
//! Both are object-safe on purpose (no generic methods, no associated types):
//! the CLI holds a `Box<dyn Store>` (the redb file now; the daemon's
//! `RemoteStore` later, ADR Q5), and dynamic dispatch costs one
//! virtual call per store operation, negligible next to a query. Generics would
//! have pushed a type parameter through every CLI function for no gain.
//!
//! Threading: `Store: Send + Sync` so a daemon can share one store between
//! connection threads (all methods take `&self`; the backend serializes
//! writers). This needs `Extractor: Send + Sync` (the registry holds
//! `Box<dyn Extractor>`), which `graph-core` now requires. Snapshots are
//! `Send` (they can move to another thread) but not required to be `Sync`.
//!
//! Sharding (ADR Q4, build deferred): nothing here names a file, table or
//! shard. Every operation is keyed by `(org, repo, path)` or by query filters,
//! and `NodeId`s are opaque and only meaningful within one snapshot, so a
//! partitioned store can implement the same traits (fan-out and merge behind
//! `search`, one manifest version behind `snapshot`).
//!
//! Wire: request and result types (`Query`, `SymbolQuery`, `Hit`, `SymbolHit`,
//! `RepoInfo`, `IngestStats`, `Node`) derive `Serialize + Deserialize`.
//! `StoreError` and `BatchFile` (borrows its input) do not yet; the daemon
//! story will add a wire form for them.
use crate::{
    BatchFile, Hit, IndexOptions, IngestStats, Query, RepoInfo, StoreError, SymbolHit, SymbolQuery,
    VacuumStats,
};
use graph_core::{Extraction, Extractor, Node, NodeId, NodeKind};
use std::collections::HashSet;
use std::path::Path;

type Result<T> = std::result::Result<T, StoreError>;

/// One file made ready by [`Store::prepare`] for [`Store::index_prepared`]:
/// its normalized path, language and fingerprint, plus the extraction (or
/// the per-file rejection, or a note that the stored copy is unchanged).
/// Commit it to the store, org and repo that prepared it: another org/repo
/// is refused, and another store (with other extractor versions) would
/// record a fingerprint its extractors did not produce.
pub struct PreparedFile {
    pub(crate) org: String,
    pub(crate) repo: String,
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) fingerprint: String,
    pub(crate) bytes_len: usize,
    pub(crate) origin: Option<String>,
    pub(crate) work: Prepared,
    /// The backend's share of the per-file work, done while preparing so
    /// the writer only interns and inserts.
    /// Held alongside the extraction until commit, so a prepared v2 file
    /// takes roughly twice the memory of its extraction.
    pub(crate) v2: Option<Box<crate::v2::V2Prep>>,
}

pub(crate) enum Prepared {
    /// The stored copy carried this fingerprint when prepared: not
    /// extracted. The source is kept in case it changed by commit time.
    Unchanged(String),
    Extracted(Extraction),
    /// Not UTF-8, too large, or invalid spans: reported in the file's slot.
    Rejected(StoreError),
}

impl PreparedFile {
    /// The normalized path the file will be stored under.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// The language detected (or given), lowercased.
    pub fn language(&self) -> &str {
        &self.language
    }
    /// Source size in bytes.
    pub fn bytes_len(&self) -> usize {
        self.bytes_len
    }
    /// Whether `prepare` skipped extraction because the stored copy was
    /// unchanged.
    pub fn is_unchanged(&self) -> bool {
        matches!(self.work, Prepared::Unchanged(_))
    }
    /// About how many bytes of heap this prepared file occupies (the
    /// extraction's tokens, symbols and strings, or the kept source, plus
    /// the backend's pre-encoded work), so a caller can budget memory.
    pub fn memory_footprint(&self) -> usize {
        use std::mem::size_of;
        let strings = self.path.capacity()
            + self.language.capacity()
            + self.fingerprint.capacity()
            + self.origin.as_ref().map_or(0, String::capacity);
        let work = match &self.work {
            Prepared::Unchanged(src) => src.capacity(),
            Prepared::Rejected(_) => 0,
            Prepared::Extracted(ex) => {
                ex.tokens.capacity() * size_of::<graph_core::TokenDecl>()
                    + ex.tokens.iter().map(|t| t.text.capacity()).sum::<usize>()
                    + ex.symbols.capacity() * size_of::<graph_core::SymbolDecl>()
                    + ex.symbols
                        .iter()
                        .map(|s| {
                            s.name.capacity() + s.lang_kind.as_ref().map_or(0, String::capacity)
                        })
                        .sum::<usize>()
            }
        };
        size_of::<Self>() + strings + work + self.v2.as_ref().map_or(0, |p| p.footprint())
    }
}

// Prepared on worker threads, committed on the writer's.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<PreparedFile>();
};

/// One offset/limit page of a paged traversal (ADR 0003 story 11:
/// [`StoreRead::children_page`], [`StoreRead::descendants_page`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Whether `offset + items.len()` would return a non-empty page from the
    /// same call, on the same snapshot.
    pub has_more: bool,
}

fn page_slice<T>(mut all: Vec<T>, offset: usize, limit: usize) -> Result<Page<T>> {
    let total = all.len();
    if offset >= total {
        return Ok(Page {
            items: Vec::new(),
            has_more: false,
        });
    }
    let end = offset.saturating_add(limit).min(total);
    let has_more = end < total;
    // Drain instead of clone: `Node` isn't required to be `Copy`/cheap.
    let items = all.drain(offset..end).collect();
    Ok(Page { items, has_more })
}

/// Read operations. All results are plain data.
pub trait StoreRead {
    fn get(&self, id: NodeId) -> Result<Option<Node>>;
    /// Parent pointer lookup (one hop).
    fn parent(&self, id: NodeId) -> Result<Option<Node>>;
    fn count_nodes(&self, kind: NodeKind) -> Result<usize>;
    /// Every top-level (parent-less) node: one per org, in creation order.
    /// Used by `export` (ADR 0003 story 12) to walk the whole graph through
    /// this trait alone, with no backend-specific access: `roots()` plus
    /// `children`/`descendants` reaches every org, repo, file, symbol and
    /// token.
    fn roots(&self) -> Result<Vec<Node>>;
    /// Direct children in creation order: an org's repos, a repo's files, a
    /// file's top-level symbols and tokens outside any symbol (source order),
    /// a symbol's child symbols and direct tokens. Unknown ids and tokens have
    /// none. Files without symbols (fallback tokenizer) list all their tokens.
    fn children(&self, id: NodeId) -> Result<Vec<Node>>;
    /// Everything below `id`: depth first, source order, a node before its
    /// children. Backends may override this to avoid re-reading per level.
    fn descendants(&self, id: NodeId) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut stack: Vec<Node> = self.children(id)?.into_iter().rev().collect();
        let mut seen = HashSet::new();
        while let Some(n) = stack.pop() {
            // A corrupt cyclic parent link must not loop forever.
            if !seen.insert(n.id) {
                return Err(StoreError::Corrupt(format!(
                    "containment cycle at {}",
                    n.id
                )));
            }
            if n.kind != NodeKind::Token {
                stack.extend(self.children(n.id)?.into_iter().rev());
            }
            out.push(n);
        }
        Ok(out)
    }
    /// Parent, grandparent, ... up to the org, nearest first; empty for an
    /// org or an unknown id.
    fn ancestors(&self, id: NodeId) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut cur = self.parent(id)?;
        let mut seen = HashSet::new();
        while let Some(n) = cur {
            if !seen.insert(n.id) {
                return Err(StoreError::Corrupt(format!("parent cycle at {}", n.id)));
            }
            cur = self.parent(n.id)?;
            out.push(n);
        }
        Ok(out)
    }
    /// A page of `children(id)`, ADR 0003 story 11: `offset`/`limit` over the
    /// same deterministic (creation) order `children` already documents.
    /// `has_more` says whether a further page (same `id`, `offset + limit`)
    /// would be non-empty, so a caller can loop without an extra empty call.
    /// A page fetched through the same [`Store::snapshot`](crate::Store::snapshot)
    /// handle as earlier pages reads the same frozen transaction, so a writer
    /// running concurrently cannot change, add to or shrink a page already
    /// handed out or one fetched later in the same paging sequence -- proven
    /// by `v2_tests::paging_is_snapshot_consistent_across_concurrent_writes`.
    /// The default implementation is built on `children`, which is
    /// in-memory today; it is still snapshot-correct, just not yet
    /// lazy/streaming -- a backend may override this to avoid materializing
    /// the full child list per page.
    fn children_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        page_slice(self.children(id)?, offset, limit)
    }

    /// A page of `descendants(id)` (depth-first, source order): same
    /// offset/limit and snapshot-consistency contract as [`children_page`].
    /// Includes the "fallback files" case the epic's hierarchy-traversal
    /// story (story 12) calls out: a File indexed by the generic fallback
    /// tokenizer (no language extractor, so no Symbol nodes) has Tokens as
    /// its direct children, and paging its descendants yields those Tokens
    /// in source order with no error, the same as `descendants` does for one
    /// unpaged call.
    fn descendants_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        page_slice(self.descendants(id)?, offset, limit)
    }

    /// All tokens stored for one file, in source order; `None` if the file is
    /// not indexed.
    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>>;
    /// What is indexed (optionally scoped to an org and/or repo).
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>>;
    /// Reference implementation of `describe` that scans every node. Kept on
    /// this trait (not a separate oracle trait) because the conformance suite
    /// reaches it through `dyn Store` and `dyn StoreRead`; a split needs
    /// supertrait plumbing for no gain yet. Backends
    /// must return the same as `describe`; the conformance suite checks it.
    #[doc(hidden)]
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>>;
    /// Find symbols by name (exact, or prefix with a trailing `*`).
    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>>;
    /// Token-text search with roll-up to `q.grain`.
    fn search(&self, q: &Query) -> Result<Vec<Hit>>;
}

/// Snapshot observability (ADR 0003 story 10, [`Store::snapshot_stats`]):
/// how many snapshot handles this store currently has alive, and the age of
/// the oldest one. `store_size_bytes` is the backing store's on-disk size,
/// not a size specific to any one snapshot -- an in-process redb snapshot is
/// a read transaction over the same file the store already has open, not a
/// separate copy, so there is no snapshot-specific size to report; a future
/// backend that did materialize a distinct snapshot copy would report that
/// copy's size here instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Snapshot handles created by `snapshot()` and not yet dropped.
    pub open_count: usize,
    /// Age of the oldest currently open snapshot, if any are open.
    pub oldest_age: Option<std::time::Duration>,
    /// The backing store's on-disk file size (not snapshot-specific; see
    /// the struct doc comment).
    pub store_size_bytes: u64,
}

/// A read-write store. Writers are serialized by the backend. A single-file
/// write is atomic. A batch is atomic per chunk (see `index_batch`).
pub trait Store: StoreRead + Send + Sync {
    /// A consistent, read-only view: everything read through it sees one
    /// committed state, whatever writers do meanwhile. Released on drop.
    /// (redb backend: one read transaction, so a long-lived snapshot delays
    /// page reuse; keep snapshots short-lived.)
    ///
    /// ADR 0003 story 10 (max age/`SnapshotExpired`): a backend may refuse
    /// reads through a handle once it has lived past a configured max age
    /// (default 15 minutes, matching ADR 0003 Q6's decision), returning
    /// [`StoreError::SnapshotExpired`] from that read rather than from this
    /// call -- `snapshot()` itself never fails because the *previous*
    /// snapshot aged out.
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>>;

    /// Snapshot observability (ADR 0003 story 10): count, oldest age and
    /// store size. The default (all zero, `oldest_age: None`) is for a
    /// backend that tracks none of this; `V2Store` overrides it.
    fn snapshot_stats(&self) -> SnapshotStats {
        SnapshotStats::default()
    }

    /// Index raw bytes with an explicit `origin` and options; the primary
    /// single-file entry point.
    #[allow(clippy::too_many_arguments)]
    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats>;

    /// Index a caller-supplied extraction (idempotent: replaces the file).
    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats>;

    /// Index many files of one repo. Per-file failures are reported in their
    /// slot. A storage error makes the whole call return `Err`, and then the
    /// per-file results of any chunks that did commit are lost (only the
    /// error is returned). The store commits per chunk, so an error may
    /// leave earlier chunks committed; what is stored is complete and
    /// consistent (whole files only), and re-running the batch skips stored
    /// files by fingerprint and stores the rest.
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>>;

    /// The parallel-safe half of [`Store::index_batch`] for one file: size
    /// and UTF-8 checks, path normalization, language detection, fingerprint
    /// and extraction with span validation. Takes `&self` and writes nothing,
    /// so many threads may call it while another commits. Without `reindex`
    /// it first reads the last committed state and skips extraction when the
    /// stored copy has the same fingerprint. Per-file rejections are carried
    /// in the result; `Err` is a storage error.
    fn prepare(
        &self,
        org: &str,
        repo: &str,
        file: &BatchFile<'_>,
        opts: IndexOptions,
    ) -> Result<PreparedFile>;

    /// The committing half: store prepared files exactly as `index_batch`
    /// would have stored the same inputs (same transactions, outcomes in
    /// input order, same atomicity). The unchanged check is re-run
    /// authoritatively inside the transaction; a file prepared as unchanged
    /// that has changed since (another writer, or the same path earlier in
    /// the batch) is extracted then. `opts` should match `prepare`'s. Files
    /// prepared for another org/repo make the call fail
    /// ([`StoreError::Rejected`]).
    fn index_prepared(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>>;

    /// Remove directory-run files of `org/repo` not in `keep` (or report them
    /// with `dry_run`). Returns the removed paths.
    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>>;

    /// Reclaim space left behind by replaced and pruned files: removes the
    /// dictionary terms nothing refers to any more (it does not compact the
    /// file; see `V2Store::compact`). Never changes what any read returns.
    fn vacuum(&self) -> Result<VacuumStats>;

    fn index_bytes(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
    ) -> Result<IngestStats> {
        self.index_bytes_opts(
            org,
            repo,
            path,
            bytes,
            language,
            None,
            IndexOptions::default(),
        )
    }

    fn index_bytes_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        self.index_bytes_opts(
            org,
            repo,
            path,
            bytes,
            language,
            origin,
            IndexOptions::default(),
        )
    }

    fn ingest_file(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
    ) -> Result<IngestStats> {
        self.ingest_file_with_origin(org, repo, path, language, ex, None)
    }
}

/// Schema version stamped in the database file at `path`, without opening a
/// store. `Ok(None)` when there is no file or it holds no schema yet (empty
/// or freshly created). `Ok(Some(v))` is the current layout. A file in the
/// retired per-node format is [`StoreError::LegacyFormat`]; any other
/// version is [`StoreError::SchemaMismatch`]. Reads only; it fails with
/// `Locked` while another process holds the file.
///
/// It opens the file with `Database::create`, so it needs write permission on
/// the file, and it can never be strictly read-only: redb may repair a file
/// left by a crash when it opens it.
pub fn detect_format(path: &Path) -> Result<Option<u64>> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() > 0 => {}
        _ => return Ok(None),
    }
    let db = redb::Database::create(path).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => StoreError::Locked(path.display().to_string()),
        e => StoreError::OpenFailed {
            path: path.display().to_string(),
            reason: e.to_string(),
        },
    })?;
    let rt = db.begin_read()?;
    let found = match rt.open_table(crate::META) {
        Ok(t) => t.get("schema_version")?.map(|v| v.value()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    match found {
        None => Ok(None),
        Some(v) if v == crate::v2::V2_SCHEMA_VERSION => Ok(Some(v)),
        Some(v) if crate::LEGACY_SCHEMA_VERSIONS.contains(&v) => Err(StoreError::LegacyFormat {
            path: path.display().to_string(),
            version: v,
        }),
        Some(v) => Err(StoreError::SchemaMismatch { found: v }),
    }
}

/// Open (or create) the store at `path` with `extractors` registered.
/// Register every shipped extractor: the extractor version is part of a
/// file's fingerprint, so a store without one re-indexes that language's
/// files with the token-only fallback. A file in the retired v1 format is
/// refused ([`StoreError::LegacyFormat`]) and left untouched.
pub fn open_store(path: &Path, extractors: Vec<Box<dyn Extractor>>) -> Result<Box<dyn Store>> {
    let mut s = crate::V2Store::open(path)?;
    for e in extractors {
        s.register(e);
    }
    Ok(Box::new(s))
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    /// Two nodes that name each other as parent and child.
    struct Loop;
    fn node(id: u64, parent: u64) -> Node {
        Node {
            id,
            parent: Some(parent),
            kind: NodeKind::Symbol,
            name: id.to_string(),
            language: None,
            symbol_kind: None,
            lang_kind: None,
            token_class: None,
            has_errors: false,
            origin: None,
            fingerprint: None,
            span: None,
        }
    }
    impl StoreRead for Loop {
        fn get(&self, id: NodeId) -> Result<Option<Node>> {
            Ok(Some(node(id, 3 - id)))
        }
        fn parent(&self, id: NodeId) -> Result<Option<Node>> {
            self.get(3 - id)
        }
        fn count_nodes(&self, _: NodeKind) -> Result<usize> {
            Ok(0)
        }
        fn roots(&self) -> Result<Vec<Node>> {
            Ok(vec![])
        }
        fn children(&self, id: NodeId) -> Result<Vec<Node>> {
            Ok(vec![node(3 - id, id)])
        }
        fn file_tokens(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<Node>>> {
            Ok(None)
        }
        fn describe(&self, _: Option<&str>, _: Option<&str>) -> Result<Vec<RepoInfo>> {
            Ok(vec![])
        }
        fn describe_by_scan(&self, _: Option<&str>, _: Option<&str>) -> Result<Vec<RepoInfo>> {
            Ok(vec![])
        }
        fn search_symbols(&self, _: &SymbolQuery) -> Result<Vec<SymbolHit>> {
            Ok(vec![])
        }
        fn search(&self, _: &crate::Query) -> Result<Vec<crate::Hit>> {
            Ok(vec![])
        }
    }

    #[test]
    fn cyclic_containment_is_an_error_not_a_hang() {
        assert!(matches!(Loop.descendants(1), Err(StoreError::Corrupt(_))));
        assert!(matches!(Loop.ancestors(1), Err(StoreError::Corrupt(_))));
    }
}
