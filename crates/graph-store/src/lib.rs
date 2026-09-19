//! Embedded graph store on `redb` (pure Rust). Knows nothing about any
//! particular language.
use graph_core::{
    check_contains, detect_language, normalize_path, Extraction, Extractor, Node, NodeId, NodeKind,
    Registry, Span, SymbolKind, TokenClass,
};
use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadableMultimapTable, ReadableTable,
    TableDefinition,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const SCHEMA_VERSION: u64 = 1;
/// `Node::origin` of files written by a directory run; only these are pruned.
pub const ORIGIN_DIRECTORY: &str = "directory";
/// Spans are `u32` byte offsets.
pub const MAX_SOURCE_BYTES: usize = u32::MAX as usize;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const NODES: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");
/// `parent\0kind\0name` -> node id, for idempotent org/repo/file lookup.
const NAMES: TableDefinition<&str, u64> = TableDefinition::new("names");
const CHILDREN: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("children");
const TOKENS: MultimapTableDefinition<&str, u64> = MultimapTableDefinition::new("tokens_by_text");
/// Symbol name -> symbol node ids (exact and prefix lookup).
const SYMBOLS: MultimapTableDefinition<&str, u64> = MultimapTableDefinition::new("symbols_by_name");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database is locked by another process: {0}")]
    Locked(String),
    #[error("incompatible schema version {found} (this build supports {SCHEMA_VERSION}); database left unmodified")]
    SchemaMismatch { found: u64 },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("rejected: {0} is not valid UTF-8")]
    NotUtf8(String),
    #[error("rejected: {0} is larger than 4 GiB")]
    TooLarge(String),
    #[error("invalid span: {0}")]
    InvalidSpan(String),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Schema(graph_core::SchemaError),
    #[error("storage error: {0}")]
    Storage(String),
}

impl<E: Into<redb::Error>> From<E> for StoreError {
    fn from(e: E) -> Self {
        match e.into() {
            redb::Error::DatabaseAlreadyOpen => StoreError::Locked("already open".into()),
            other => StoreError::Storage(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    db: Database,
    registry: Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grain {
    Token,
    Symbol,
    File,
    Repo,
    Org,
}

impl std::str::FromStr for Grain {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Ok(match s {
            "token" => Self::Token,
            "symbol" => Self::Symbol,
            "file" => Self::File,
            "repo" => Self::Repo,
            "org" => Self::Org,
            _ => return Err(format!("unknown grain `{s}` (token|symbol|file|repo|org)")),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    pub class: Option<TokenClass>,
    pub grain: Grain,
    /// Restrict the symbol grain to this kind (e.g. methods).
    pub symbol_kind: Option<SymbolKind>,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: None,
            org: None,
            repo: None,
            class: None,
            grain: Grain::Token,
            symbol_kind: None,
        }
    }
}

/// Symbol lookup by name (exact, or prefix with a trailing `*`).
#[derive(Debug, Clone)]
pub struct SymbolQuery {
    pub pattern: String,
    pub kind: Option<SymbolKind>,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    /// Restrict to one file path (normalized, relative as indexed).
    pub file: Option<String>,
}

impl SymbolQuery {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: None,
            language: None,
            org: None,
            repo: None,
            file: None,
        }
    }
}

/// A symbol with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolHit {
    pub org: String,
    pub repo: String,
    pub file: String,
    pub language: Option<String>,
    pub name: String,
    /// Qualified path from the outermost enclosing symbol, e.g. `S::a`.
    pub qualified: String,
    pub kind: SymbolKind,
    /// Language-specific kind string (`struct`, `impl`, `fn`, ...).
    pub lang_kind: Option<String>,
    pub span: Option<Span>,
}

/// One result row at the requested grain, with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub grain: Grain,
    pub org: String,
    pub repo: Option<String>,
    pub file: Option<String>,
    pub language: Option<String>,
    /// Qualified enclosing symbol path, e.g. `Foo::bar`.
    pub symbol: Option<String>,
    pub symbol_kind: Option<SymbolKind>,
    pub lang_kind: Option<String>,
    /// Token grain: the token's class.
    pub token_class: Option<TokenClass>,
    /// Token grain: the token; symbol grain: the symbol.
    pub span: Option<Span>,
    /// Number of matching tokens contained in this node.
    pub count: usize,
    /// Symbol grain only: the file has no symbols at all (e.g. fallback language).
    pub no_symbols: bool,
    /// Symbol grain only: the file has symbols, but none enclosing the match
    /// (of the requested kind); rolled up to the file.
    pub no_matching_symbol: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct IngestStats {
    pub file_id: NodeId,
    pub symbols: usize,
    pub tokens: usize,
    /// True when an existing file was replaced.
    pub replaced: bool,
    /// The file was flagged `has_errors`.
    pub has_errors: bool,
    /// Normalized path and language actually stored.
    pub path: String,
    pub language: String,
}

fn enc(n: &Node) -> Vec<u8> {
    serde_json::to_vec(n).expect("node serializes")
}

fn dec(b: &[u8]) -> Result<Node> {
    serde_json::from_slice(b).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn name_key(parent: Option<NodeId>, kind: NodeKind, name: &str) -> String {
    format!("{}\0{:?}\0{}", parent.unwrap_or(0), kind, name)
}

/// Delete everything below `root` (not `root` itself), including token postings.
fn remove_descendants(
    nodes: &mut redb::Table<u64, &[u8]>,
    children: &mut redb::MultimapTable<u64, u64>,
    tokens: &mut redb::MultimapTable<&str, u64>,
    symbols: &mut redb::MultimapTable<&str, u64>,
    root: NodeId,
) -> Result<()> {
    let ids = |it: redb::MultimapValue<u64>| -> Result<Vec<NodeId>> {
        Ok(it
            .map(|v| v.map(|g| g.value()))
            .collect::<std::result::Result<_, _>>()?)
    };
    let mut stack = ids(children.remove_all(root)?)?;
    while let Some(id) = stack.pop() {
        stack.extend(ids(children.remove_all(id)?)?);
        if let Some(old) = nodes.remove(id)? {
            let n = dec(old.value())?;
            match n.kind {
                NodeKind::Token => {
                    tokens.remove(n.name.as_str(), id)?;
                }
                NodeKind::Symbol => {
                    symbols.remove(n.name.as_str(), id)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

impl Store {
    /// Open or create a database file. Fails without modifying the file on a
    /// schema mismatch or when another process holds it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path.as_ref()).map_err(|e| match e {
            DatabaseError::DatabaseAlreadyOpen => {
                StoreError::Locked(path.as_ref().display().to_string())
            }
            e => StoreError::Storage(format!("{}: {e}", path.as_ref().display())),
        })?;
        let found = {
            let rt = db.begin_read()?;
            match rt.open_table(META) {
                Ok(t) => t.get("schema_version")?.map(|v| v.value()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        };
        match found {
            Some(v) if v != SCHEMA_VERSION => return Err(StoreError::SchemaMismatch { found: v }),
            Some(_) => {
                // Databases written before the symbol index existed: build it once.
                let missing = matches!(
                    db.begin_read()?.open_multimap_table(SYMBOLS),
                    Err(redb::TableError::TableDoesNotExist(_))
                );
                if missing {
                    let wt = db.begin_write()?;
                    {
                        let nodes = wt.open_table(NODES)?;
                        let mut idx = wt.open_multimap_table(SYMBOLS)?;
                        for r in nodes.iter()? {
                            let (id, v) = r?;
                            let n = dec(v.value())?;
                            if n.kind == NodeKind::Symbol {
                                idx.insert(n.name.as_str(), id.value())?;
                            }
                        }
                    }
                    wt.commit()?;
                }
            }
            None => {
                let wt = db.begin_write()?;
                {
                    let mut m = wt.open_table(META)?;
                    m.insert("schema_version", SCHEMA_VERSION)?;
                    m.insert("next_id", 1)?;
                    wt.open_table(NODES)?;
                    wt.open_table(NAMES)?;
                    wt.open_multimap_table(CHILDREN)?;
                    wt.open_multimap_table(TOKENS)?;
                    wt.open_multimap_table(SYMBOLS)?;
                }
                wt.commit()?;
            }
        }
        Ok(Self {
            db,
            registry: Registry::default(),
        })
    }

    /// Remove files of `org/repo` whose (normalized) path is not in `keep`,
    /// considering only files whose last ingest came from a directory run
    /// (`ORIGIN_DIRECTORY`). Returns the removed paths. Nothing else is
    /// touched. With `dry_run` nothing is changed and the paths that would be
    /// removed are returned.
    pub fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &std::collections::HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        let wt = self.db.begin_write()?;
        let mut removed = Vec::new();
        {
            let mut nodes = wt.open_table(NODES)?;
            let mut names = wt.open_table(NAMES)?;
            let mut children = wt.open_multimap_table(CHILDREN)?;
            let mut tokens = wt.open_multimap_table(TOKENS)?;
            let mut sym_idx = wt.open_multimap_table(SYMBOLS)?;
            let org_id = names
                .get(name_key(None, NodeKind::Org, org).as_str())?
                .map(|v| v.value());
            let repo_id = match org_id {
                Some(o) => names
                    .get(name_key(Some(o), NodeKind::Repo, repo).as_str())?
                    .map(|v| v.value()),
                None => None,
            };
            if let Some(repo_id) = repo_id {
                let files: Vec<NodeId> = children
                    .get(repo_id)?
                    .map(|v| v.map(|g| g.value()))
                    .collect::<std::result::Result<_, _>>()?;
                for fid in files {
                    let f = dec(nodes
                        .get(fid)?
                        .ok_or_else(|| StoreError::Corrupt("dangling file".into()))?
                        .value())?;
                    if keep.contains(&f.name) || f.origin.as_deref() != Some(ORIGIN_DIRECTORY) {
                        continue;
                    }
                    if dry_run {
                        removed.push(f.name);
                        continue;
                    }
                    remove_descendants(&mut nodes, &mut children, &mut tokens, &mut sym_idx, fid)?;
                    nodes.remove(fid)?;
                    names.remove(name_key(Some(repo_id), NodeKind::File, &f.name).as_str())?;
                    children.remove(repo_id, fid)?;
                    removed.push(f.name);
                }
            }
        }
        if dry_run {
            wt.abort()?;
        } else {
            wt.commit()?;
        }
        removed.sort();
        Ok(removed)
    }

    /// Register a language extractor used by `index_bytes`.
    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.registry.register(e);
    }

    /// Index raw file bytes: the single entry point shared by the CLI and
    /// library users. Rejects non-UTF-8 and oversized input (nothing stored),
    /// normalizes the path, lowercases the language (default: detected from the
    /// extension) and uses the fallback tokenizer.
    pub fn index_bytes(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
    ) -> Result<IngestStats> {
        self.index_bytes_with_origin(org, repo, path, bytes, language, None)
    }

    /// Like `index_bytes`, recording `origin` on the file node (replacing any
    /// earlier value: the last ingest wins).
    pub fn index_bytes_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(StoreError::TooLarge(format!("`{path}`")));
        }
        let src =
            std::str::from_utf8(bytes).map_err(|_| StoreError::NotUtf8(format!("`{path}`")))?;
        let path = normalize_path(path);
        let lang = language.map_or_else(|| detect_language(&path), str::to_ascii_lowercase);
        let ex = self.registry.extract(&lang, src);
        self.ingest_file_with_origin(org, repo, &path, &lang, &ex, origin)
    }

    pub fn get(&self, id: NodeId) -> Result<Option<Node>> {
        let rt = self.db.begin_read()?;
        let t = rt.open_table(NODES)?;
        let r = t.get(id)?.map(|v| dec(v.value())).transpose();
        r
    }

    /// Parent pointer lookup (one hop).
    pub fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        match self.get(id)? {
            Some(Node {
                parent: Some(p), ..
            }) => self.get(p),
            _ => Ok(None),
        }
    }

    pub fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        let rt = self.db.begin_read()?;
        let t = rt.open_table(NODES)?;
        let mut n = 0;
        for r in t.iter()? {
            if dec(r?.1.value())?.kind == kind {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Index one file (idempotent: re-indexing replaces the file's subtree).
    /// The extraction's symbols must have spans; parents are derived from
    /// span containment and tokens attach to their innermost symbol.
    pub fn ingest_file(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
    ) -> Result<IngestStats> {
        self.ingest_file_with_origin(org, repo, path, language, ex, None)
    }

    /// `ingest_file` that also sets the file's `origin` (see `Node::origin`).
    pub fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        if org.is_empty() || repo.is_empty() {
            return Err(StoreError::Rejected(
                "org and repo must not be empty".into(),
            ));
        }
        let wt = self.db.begin_write()?;
        let mut stats = IngestStats::default();
        {
            let mut meta = wt.open_table(META)?;
            let mut nodes = wt.open_table(NODES)?;
            let mut names = wt.open_table(NAMES)?;
            let mut children = wt.open_multimap_table(CHILDREN)?;
            let mut tokens = wt.open_multimap_table(TOKENS)?;
            let mut sym_idx = wt.open_multimap_table(SYMBOLS)?;
            let mut next = meta.get("next_id")?.map(|v| v.value()).unwrap_or(1);

            let mut ensure = |parent: Option<NodeId>,
                              parent_kind: Option<NodeKind>,
                              kind: NodeKind,
                              name: &str,
                              language: Option<&str>|
             -> Result<(NodeId, bool)> {
                if let Some(pk) = parent_kind {
                    check_contains(pk, kind).map_err(StoreError::Schema)?;
                }
                let key = name_key(parent, kind, name);
                let existing = names.get(key.as_str())?.map(|v| v.value());
                if let Some(id) = existing {
                    return Ok((id, true));
                }
                let id = next;
                next += 1;
                let node = Node {
                    id,
                    parent,
                    kind,
                    name: name.into(),
                    language: language.map(Into::into),
                    symbol_kind: None,
                    lang_kind: None,
                    token_class: None,
                    has_errors: false,
                    origin: None,
                    span: None,
                };
                nodes.insert(id, enc(&node).as_slice())?;
                names.insert(key.as_str(), id)?;
                if let Some(p) = parent {
                    children.insert(p, id)?;
                }
                Ok((id, false))
            };
            let (org_id, _) = ensure(None, None, NodeKind::Org, org, None)?;
            let (repo_id, _) = ensure(
                Some(org_id),
                Some(NodeKind::Org),
                NodeKind::Repo,
                repo,
                None,
            )?;
            let (file_id, existed) = ensure(
                Some(repo_id),
                Some(NodeKind::Repo),
                NodeKind::File,
                path,
                Some(language),
            )?;
            stats.file_id = file_id;
            if !existed {
                let mut f = dec(nodes.get(file_id)?.expect("file node").value())?;
                f.has_errors = ex.has_errors;
                f.origin = origin.map(Into::into);
                nodes.insert(file_id, enc(&f).as_slice())?;
            }
            stats.replaced = existed;
            stats.has_errors = ex.has_errors;
            stats.path = path.to_string();
            stats.language = language.to_string();

            if existed {
                // Drop the old subtree.
                remove_descendants(
                    &mut nodes,
                    &mut children,
                    &mut tokens,
                    &mut sym_idx,
                    file_id,
                )?;
                // Refresh language.
                let mut f = dec(nodes.get(file_id)?.expect("file node").value())?;
                f.language = Some(language.into());
                f.has_errors = ex.has_errors;
                f.origin = origin.map(Into::into);
                nodes.insert(file_id, enc(&f).as_slice())?;
            }

            let mut syms: Vec<_> = ex.symbols.iter().collect();
            syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
            let mut toks: Vec<_> = ex.tokens.iter().collect();
            toks.sort_by_key(|t| t.span.start);

            // Open-symbol stack: (id, end).
            let mut open: Vec<(NodeId, u32)> = Vec::new();
            let mut si = 0;
            let alloc = |next: &mut u64| {
                let id = *next;
                *next += 1;
                id
            };
            let insert = |node: Node,
                          nodes: &mut redb::Table<u64, &[u8]>,
                          children: &mut redb::MultimapTable<u64, u64>|
             -> Result<()> {
                nodes.insert(node.id, enc(&node).as_slice())?;
                if let Some(p) = node.parent {
                    children.insert(p, node.id)?;
                }
                Ok(())
            };
            let mut ti = 0;
            // Merge symbols and tokens in source order (symbols first on ties).
            while si < syms.len() || ti < toks.len() {
                let take_sym = si < syms.len()
                    && (ti >= toks.len() || syms[si].span.start <= toks[ti].span.start);
                let pos = if take_sym {
                    syms[si].span.start
                } else {
                    toks[ti].span.start
                };
                while open.last().is_some_and(|&(_, end)| end <= pos) {
                    open.pop();
                }
                let (parent, pkind) = match open.last() {
                    Some(&(id, _)) => (id, NodeKind::Symbol),
                    None => (file_id, NodeKind::File),
                };
                let (span_start, span_end) = if take_sym {
                    (syms[si].span.start, syms[si].span.end)
                } else {
                    (toks[ti].span.start, toks[ti].span.end)
                };
                if span_start > span_end {
                    return Err(StoreError::InvalidSpan(format!(
                        "start {span_start} > end {span_end}"
                    )));
                }
                if let Some(&(_, end)) = open.last() {
                    if span_end > end {
                        return Err(StoreError::InvalidSpan(format!(
                            "bytes {span_start}..{span_end} partially overlap an enclosing symbol ending at {end}"
                        )));
                    }
                }
                if take_sym {
                    let s = syms[si];
                    si += 1;
                    check_contains(pkind, NodeKind::Symbol).map_err(StoreError::Schema)?;
                    let id = alloc(&mut next);
                    insert(
                        Node {
                            id,
                            parent: Some(parent),
                            kind: NodeKind::Symbol,
                            name: s.name.clone(),
                            language: None,
                            symbol_kind: Some(s.kind),
                            lang_kind: s.lang_kind.clone(),
                            token_class: None,
                            has_errors: false,
                            origin: None,
                            span: Some(s.span),
                        },
                        &mut nodes,
                        &mut children,
                    )?;
                    sym_idx.insert(s.name.as_str(), id)?;
                    open.push((id, s.span.end));
                    stats.symbols += 1;
                } else {
                    let t = toks[ti];
                    ti += 1;
                    check_contains(pkind, NodeKind::Token).map_err(StoreError::Schema)?;
                    let id = alloc(&mut next);
                    insert(
                        Node {
                            id,
                            parent: Some(parent),
                            kind: NodeKind::Token,
                            name: t.text.clone(),
                            language: None,
                            symbol_kind: None,
                            lang_kind: None,
                            token_class: Some(t.class),
                            has_errors: false,
                            origin: None,
                            span: Some(t.span),
                        },
                        &mut nodes,
                        &mut children,
                    )?;
                    tokens.insert(t.text.as_str(), id)?;
                    stats.tokens += 1;
                }
            }
            meta.insert("next_id", next)?;
        }
        wt.commit()?;
        Ok(stats)
    }

    /// Find symbols by name. `pattern` is exact, or a prefix when it ends in `*`.
    /// Results are ordered by org, repo, file and byte offset.
    pub fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        let rt = self.db.begin_read()?;
        let nodes = rt.open_table(NODES)?;
        let idx = rt.open_multimap_table(SYMBOLS)?;
        let load = |id: NodeId| -> Result<Node> {
            dec(nodes
                .get(id)?
                .ok_or_else(|| StoreError::Corrupt(format!("dangling node {id}")))?
                .value())
        };
        let mut ids: Vec<NodeId> = Vec::new();
        match q.pattern.strip_suffix('*') {
            Some(prefix) => {
                for r in idx.range(prefix..)? {
                    let (k, vals) = r?;
                    if !k.value().starts_with(prefix) {
                        break;
                    }
                    for v in vals {
                        ids.push(v?.value());
                    }
                }
            }
            None => {
                for v in idx.get(q.pattern.as_str())? {
                    ids.push(v?.value());
                }
            }
        }
        let want_file = q.file.as_deref().map(normalize_path);
        let want_lang = q.language.as_deref().map(str::to_ascii_lowercase);
        let mut out: Vec<SymbolHit> = Vec::new();
        for id in ids {
            let sym = load(id)?;
            if q.kind.is_some() && sym.symbol_kind != q.kind {
                continue;
            }
            let mut quals = vec![sym.name.clone()];
            let mut cur = sym.parent;
            let mut file = None;
            while let Some(pid) = cur {
                let n = load(pid)?;
                cur = n.parent;
                if n.kind == NodeKind::Symbol {
                    quals.push(n.name);
                } else {
                    file = Some(n);
                    break;
                }
            }
            let file = file.ok_or_else(|| StoreError::Corrupt("symbol without file".into()))?;
            let repo = load(
                file.parent
                    .ok_or_else(|| StoreError::Corrupt("file without repo".into()))?,
            )?;
            let org = load(
                repo.parent
                    .ok_or_else(|| StoreError::Corrupt("repo without org".into()))?,
            )?;
            if want_lang
                .as_ref()
                .is_some_and(|l| file.language.as_ref() != Some(l))
                || q.org.as_ref().is_some_and(|o| &org.name != o)
                || q.repo.as_ref().is_some_and(|r| &repo.name != r)
                || want_file.as_ref().is_some_and(|f| &file.name != f)
            {
                continue;
            }
            quals.reverse();
            out.push(SymbolHit {
                org: org.name,
                repo: repo.name,
                file: file.name,
                language: file.language,
                name: sym.name,
                qualified: quals.join("::"),
                kind: sym.symbol_kind.unwrap_or(SymbolKind::Other),
                lang_kind: sym.lang_kind,
                span: sym.span,
            });
        }
        out.sort_by(|a, b| {
            (
                &a.org,
                &a.repo,
                &a.file,
                a.span.map(|s| s.start),
                &a.qualified,
            )
                .cmp(&(
                    &b.org,
                    &b.repo,
                    &b.file,
                    b.span.map(|s| s.start),
                    &b.qualified,
                ))
        });
        Ok(out)
    }

    /// Token-text search with roll-up to the requested grain.
    pub fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        let rt = self.db.begin_read()?;
        let nodes = rt.open_table(NODES)?;
        let tokens = rt.open_multimap_table(TOKENS)?;
        let kids = rt.open_multimap_table(CHILDREN)?;
        let mut sym_memo: HashMap<NodeId, bool> = HashMap::new();
        let mut cache: HashMap<NodeId, Node> = HashMap::new();
        let mut load = |id: NodeId| -> Result<Node> {
            if let Some(n) = cache.get(&id) {
                return Ok(n.clone());
            }
            let n = dec(nodes
                .get(id)?
                .ok_or_else(|| StoreError::Corrupt(format!("dangling node {id}")))?
                .value())?;
            cache.insert(id, n.clone());
            Ok(n)
        };

        let mut has_syms = |file: NodeId| -> Result<bool> {
            if let Some(&b) = sym_memo.get(&file) {
                return Ok(b);
            }
            let mut found = false;
            for c in kids.get(file)? {
                let n = dec(nodes
                    .get(c?.value())?
                    .ok_or_else(|| StoreError::Corrupt("dangling child".into()))?
                    .value())?;
                if n.kind == NodeKind::Symbol {
                    found = true;
                    break;
                }
            }
            sym_memo.insert(file, found);
            Ok(found)
        };

        type Key = (String, String, String, u32, u64);
        let mut rows: BTreeMap<Key, Hit> = BTreeMap::new();
        for id in tokens.get(q.text.as_str())? {
            let tok = load(id?.value())?;
            if q.class.is_some() && tok.token_class != q.class {
                continue;
            }
            // Ancestor chain, innermost first: symbols..., file, repo, org.
            let mut symbols: Vec<Node> = Vec::new();
            let mut cur = tok.parent;
            let mut file = None;
            while let Some(pid) = cur {
                let n = load(pid)?;
                cur = n.parent;
                if n.kind == NodeKind::Symbol {
                    symbols.push(n);
                } else {
                    file = Some(n);
                    break;
                }
            }
            let file = file.ok_or_else(|| StoreError::Corrupt("token without file".into()))?;
            let repo = load(
                file.parent
                    .ok_or_else(|| StoreError::Corrupt("file without repo".into()))?,
            )?;
            let org = load(
                repo.parent
                    .ok_or_else(|| StoreError::Corrupt("repo without org".into()))?,
            )?;
            if q.language
                .as_deref()
                .is_some_and(|l| file.language.as_deref() != Some(l.to_ascii_lowercase().as_str()))
                || q.org.as_deref().is_some_and(|o| org.name != o)
                || q.repo.as_deref().is_some_and(|r| repo.name != r)
            {
                continue;
            }
            let qual = |syms: &[Node]| {
                (!syms.is_empty()).then(|| {
                    syms.iter()
                        .rev()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join("::")
                })
            };
            let mut hit = Hit {
                grain: q.grain,
                org: org.name.clone(),
                repo: Some(repo.name.clone()),
                file: Some(file.name.clone()),
                language: file.language.clone(),
                symbol: None,
                symbol_kind: None,
                lang_kind: None,
                token_class: None,
                span: None,
                count: 1,
                no_symbols: false,
                no_matching_symbol: false,
            };
            let base = (org.name.clone(), repo.name.clone(), file.name.clone());
            let key: Key;
            match q.grain {
                Grain::Token => {
                    hit.symbol = qual(&symbols);
                    hit.symbol_kind = symbols.first().and_then(|s| s.symbol_kind);
                    hit.lang_kind = symbols.first().and_then(|s| s.lang_kind.clone());
                    hit.token_class = tok.token_class;
                    hit.span = tok.span;
                    key = (
                        base.0,
                        base.1,
                        base.2,
                        tok.span.map_or(0, |s| s.start),
                        tok.id,
                    );
                }
                Grain::Symbol => {
                    // Innermost enclosing symbol of the requested kind.
                    let pick = symbols
                        .iter()
                        .position(|s| q.symbol_kind.is_none() || s.symbol_kind == q.symbol_kind);
                    match pick {
                        Some(i) => {
                            let s = &symbols[i];
                            hit.symbol = qual(&symbols[i..]);
                            hit.symbol_kind = s.symbol_kind;
                            hit.lang_kind = s.lang_kind.clone();
                            hit.span = s.span;
                            key = (base.0, base.1, base.2, s.span.map_or(0, |x| x.start), s.id);
                        }
                        None => {
                            if !has_syms(file.id)? {
                                hit.no_symbols = true;
                            } else {
                                hit.no_matching_symbol = true;
                            }
                            key = (base.0, base.1, base.2, 0, 0);
                        }
                    }
                }
                Grain::File => key = (base.0, base.1, base.2, 0, 0),
                Grain::Repo => {
                    hit.file = None;
                    hit.language = None;
                    key = (base.0, base.1, String::new(), 0, 0);
                }
                Grain::Org => {
                    hit.repo = None;
                    hit.file = None;
                    hit.language = None;
                    key = (base.0, String::new(), String::new(), 0, 0);
                }
            }
            rows.entry(key).and_modify(|h| h.count += 1).or_insert(hit);
        }
        Ok(rows.into_values().collect())
    }
}

#[cfg(test)]
mod tests;
