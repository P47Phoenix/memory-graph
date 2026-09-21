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
//! the CLI holds a `Box<dyn Store>` chosen at run time (redb file now; the
//! daemon's `RemoteStore` later, ADR Q5), and dynamic dispatch costs one
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
    BatchFile, Hit, IndexOptions, IngestStats, Query, RedbSnapshot, RedbStore, RepoInfo,
    StoreError, SymbolHit, SymbolQuery,
};
use graph_core::{Extraction, Extractor, Node, NodeId, NodeKind};
use std::collections::HashSet;
use std::path::Path;

type Result<T> = std::result::Result<T, StoreError>;

/// Read operations. All results are plain data.
pub trait StoreRead {
    fn get(&self, id: NodeId) -> Result<Option<Node>>;
    /// Parent pointer lookup (one hop).
    fn parent(&self, id: NodeId) -> Result<Option<Node>>;
    fn count_nodes(&self, kind: NodeKind) -> Result<usize>;
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

/// A read-write store. Writers are serialized by the backend; every write is
/// atomic per call (per file, or per batch).
pub trait Store: StoreRead + Send + Sync {
    /// A consistent, read-only view: everything read through it sees one
    /// committed state, whatever writers do meanwhile. Released on drop.
    /// (redb backend: one read transaction, so a long-lived snapshot delays
    /// page reuse; keep snapshots short-lived.)
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>>;

    /// Index raw bytes with an explicit `origin` and options; the primary
    /// single-file entry point. See `RedbStore::index_bytes_opts`.
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

    /// Index many files of one repo in one atomic write; per-file failures
    /// are reported in their slot, storage errors abort the batch.
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
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

impl StoreRead for RedbStore {
    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        RedbStore::get(self, id)
    }
    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        RedbStore::parent(self, id)
    }
    fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        RedbStore::count_nodes(self, kind)
    }
    fn children(&self, id: NodeId) -> Result<Vec<Node>> {
        RedbStore::children_in(&self.db.begin_read()?, id)
    }
    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        RedbStore::file_tokens(self, org, repo, path)
    }
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        RedbStore::describe(self, org, repo)
    }
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        RedbStore::describe_by_scan(self, org, repo)
    }
    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        RedbStore::search_symbols(self, q)
    }
    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        RedbStore::search(self, q)
    }
}

impl Store for RedbStore {
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>> {
        Ok(Box::new(RedbSnapshot {
            rt: self.db.begin_read()?,
        }))
    }
    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats> {
        RedbStore::index_bytes_opts(self, org, repo, path, bytes, language, origin, opts)
    }
    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        RedbStore::ingest_file_with_origin(self, org, repo, path, language, ex, origin)
    }
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        RedbStore::index_batch(self, org, repo, files, opts)
    }
    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        RedbStore::prune_files(self, org, repo, keep, dry_run)
    }
}

impl StoreRead for RedbSnapshot {
    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        RedbStore::get_in(&self.rt, id)
    }
    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        RedbStore::parent_in(&self.rt, id)
    }
    fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        RedbStore::count_nodes_in(&self.rt, kind)
    }
    fn children(&self, id: NodeId) -> Result<Vec<Node>> {
        RedbStore::children_in(&self.rt, id)
    }
    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        RedbStore::file_tokens_in(&self.rt, org, repo, path)
    }
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        RedbStore::describe_in(&self.rt, org, repo)
    }
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        RedbStore::describe_by_scan_in(&self.rt, org, repo)
    }
    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        RedbStore::search_symbols_in(&self.rt, q)
    }
    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        RedbStore::search_in(&self.rt, q)
    }
}

/// Storage engines the CLI can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Storage format v1 on redb, one file (today's format).
    #[default]
    Redb,
    /// Storage format v2 on redb (ADR 0003 stories 2-4, first slice): an
    /// interned dictionary, one compact stream per file and count postings.
    /// Opt-in; not the default, and it cannot open a v1 file.
    RedbV2,
}

/// Open (or create) a store of the chosen backend with `extractors`
/// registered. Register every shipped extractor: the extractor version is part
/// of a file's fingerprint, so a store without one re-indexes that language's
/// files with the token-only fallback.
pub fn open_store(
    backend: Backend,
    path: &Path,
    extractors: Vec<Box<dyn Extractor>>,
) -> Result<Box<dyn Store>> {
    match backend {
        Backend::Redb => {
            let mut s = RedbStore::open(path)?;
            for e in extractors {
                s.register(e);
            }
            Ok(Box::new(s))
        }
        Backend::RedbV2 => {
            let mut s = crate::V2Store::open(path)?;
            for e in extractors {
                s.register(e);
            }
            Ok(Box::new(s))
        }
    }
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
