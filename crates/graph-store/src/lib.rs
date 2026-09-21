//! Embedded graph store on `redb` (pure Rust). Knows nothing about any
//! particular language.
use graph_core::{
    check_contains, detect_language_from_content, normalize_path, Extraction, Extractor, Node,
    NodeId, NodeKind, Registry, Span, SymbolKind, TokenClass,
};
use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadTransaction, ReadableMultimapTable,
    ReadableTable, TableDefinition,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

mod api;
mod codec;
pub mod conformance;
mod v2;
pub use api::{detect_backend, open_store, Backend, Store, StoreRead};
pub use v2::{V2Snapshot, V2Store, VacuumStats};

/// On-disk layout version written by this build. It is 2 because databases
/// that carry the describe catalog must be refused by builds that predate it
/// (they would write without maintaining the catalog and silently make
/// `describe` drift). Builds that predate the catalog only accept 1.
pub const SCHEMA_VERSION: u64 = 2;
/// Oldest layout this build still opens; a version-1 database is upgraded in
/// place on open (catalog backfill, then the version is stamped).
pub const MIN_SCHEMA_VERSION: u64 = 1;
/// Version of the derived symbol-name index. Bump it whenever the index
/// contents or keying change; databases with another value rebuild it on open.
pub const SYMBOL_INDEX_VERSION: u64 = 1;
/// Version of the file fingerprint scheme (what goes into `Node::fingerprint`).
/// Bump it to force every file to re-index once.
pub const FINGERPRINT_FORMAT_VERSION: u64 = 1;
/// Version of the derived describe catalog (per-repo/language counts kept in
/// step with every write). Databases with another value rebuild it on open.
pub const CATALOG_VERSION: u64 = 1;
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

/// Derived counters behind `describe`, so it never decodes nodes. Keys (fields
/// separated by NUL): `r org repo` (repo exists), `f|s|t org repo lang` (files,
/// symbols, tokens), `k org repo lang label` (symbols per kind label),
/// `c org repo class` (tokens per class). Zero counts are absent.
const CATALOG: TableDefinition<&str, u64> = TableDefinition::new("catalog");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database is locked by another process: {0}")]
    Locked(String),
    #[error("incompatible schema version {found} (this build supports {MIN_SCHEMA_VERSION} to {SCHEMA_VERSION}); database left unmodified")]
    SchemaMismatch { found: u64 },
    #[error("cannot open database {path}: {reason}")]
    OpenFailed { path: String, reason: String },
    #[error("database symbol index version {found} is newer than this build supports ({SYMBOL_INDEX_VERSION}); database left unmodified, use a newer build")]
    IndexTooNew { found: u64 },
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

/// Map an open failure; the read-only hint is given only for permission errors.
fn open_failed(path: &Path, e: &DatabaseError) -> StoreError {
    let denied = matches!(
        e,
        DatabaseError::Storage(redb::StorageError::Io(io))
            if matches!(
                io.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            )
    );
    let hint = if denied {
        " (the database file and its directory must be writable; read-only databases are not supported)"
    } else {
        ""
    };
    StoreError::OpenFailed {
        path: path.display().to_string(),
        reason: format!("{e}{hint}"),
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Check that an extraction's symbols and tokens nest properly (no span starts
/// after it ends, none partially overlaps an enclosing symbol). The only span
/// validation: every write path runs it before its first write.
fn validate_spans(ex: &Extraction) -> Result<()> {
    let mut syms: Vec<_> = ex.symbols.iter().map(|s| s.span).collect();
    syms.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut toks: Vec<_> = ex.tokens.iter().map(|t| t.span).collect();
    toks.sort_by_key(|t| t.start);
    let (mut si, mut ti) = (0, 0);
    let mut open: Vec<u32> = Vec::new();
    while si < syms.len() || ti < toks.len() {
        let take_sym = si < syms.len() && (ti >= toks.len() || syms[si].start <= toks[ti].start);
        let sp = if take_sym { syms[si] } else { toks[ti] };
        if take_sym {
            si += 1;
        } else {
            ti += 1;
        }
        while open.last().is_some_and(|&end| end <= sp.start) {
            open.pop();
        }
        if sp.start > sp.end {
            return Err(StoreError::InvalidSpan(format!(
                "start {} > end {}",
                sp.start, sp.end
            )));
        }
        if let Some(&end) = open.last() {
            if sp.end > end {
                return Err(StoreError::InvalidSpan(format!(
                    "bytes {}..{} partially overlap an enclosing symbol ending at {end}",
                    sp.start, sp.end
                )));
            }
        }
        if take_sym {
            open.push(sp.end);
        }
    }
    Ok(())
}

/// Storage format v1 on one redb file: the first backend behind [`Store`].
pub struct RedbStore {
    db: Database,
    registry: Registry,
}

/// A consistent read-only view of a `RedbStore` (one redb read transaction).
pub struct RedbSnapshot {
    rt: ReadTransaction,
}

/// Options for `index_bytes_opts` / `index_batch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexOptions {
    /// Re-index files even when their fingerprint is unchanged.
    pub reindex: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    pub class: Option<TokenClass>,
    pub grain: Grain,
    /// Restrict the symbol grain to this kind: generic (`method`) or
    /// language-specific (`struct`).
    pub symbol_kind: Option<String>,
    /// Keep at most this many rows (after deterministic ordering).
    pub limit: Option<usize>,
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
            limit: None,
        }
    }
}

/// Symbol lookup by name: exact, or a prefix with a trailing `*` (`*` alone
/// lists everything). A trailing `\*` is a literal star (exact match on a name
/// ending in `*`). Empty patterns and `**` are rejected as ambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolQuery {
    pub pattern: String,
    /// Generic kind (`method`) or language-specific kind (`struct`, `trait`).
    pub kind: Option<String>,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    /// Restrict to one file path (normalized, relative as indexed).
    pub file: Option<String>,
    /// Keep at most this many rows (after deterministic ordering).
    pub limit: Option<usize>,
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
            limit: None,
        }
    }
}

/// A symbol with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Per-language contents of a repo. `symbol_kinds` keys are `generic` or
/// `generic/language-specific` (e.g. `type/struct`, `method/fn`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageInfo {
    pub files: usize,
    pub symbols: usize,
    pub tokens: usize,
    pub symbol_kinds: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub org: String,
    pub repo: String,
    pub files: usize,
    pub languages: BTreeMap<String, LanguageInfo>,
    pub token_classes: BTreeMap<String, usize>,
}

impl RepoInfo {
    /// Every kind name usable with `--kind` (generic and language-specific),
    /// optionally limited to one language.
    pub fn kind_names(&self, language: Option<&str>) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for (lang, li) in &self.languages {
            if language.is_some_and(|l| !lang.eq_ignore_ascii_case(l)) {
                continue;
            }
            for k in li.symbol_kinds.keys() {
                out.extend(k.split('/').map(str::to_string));
            }
        }
        out
    }
}

/// One result row at the requested grain, with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One input of `Store::index_batch`.
#[derive(Debug, Clone, Copy)]
pub struct BatchFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
    pub language: Option<&'a str>,
    pub origin: Option<&'a str>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestStats {
    pub file_id: NodeId,
    pub symbols: usize,
    pub tokens: usize,
    /// True when an existing file was replaced.
    pub replaced: bool,
    /// The stored file had an identical fingerprint (same content, language,
    /// extractor version and index format), so nothing was re-indexed:
    /// `symbols` and `tokens` are 0 and the existing nodes were left untouched.
    pub unchanged: bool,
    /// The file was flagged `has_errors`.
    pub has_errors: bool,
    /// Normalized path and language actually stored.
    pub path: String,
    pub language: String,
}

/// A symbol matches a kind name if it is its generic kind or its
/// language-specific kind string (ASCII case-insensitive, like `--language`).
fn kind_matches(sym: &Node, kind: &str) -> bool {
    sym.symbol_kind
        .unwrap_or(SymbolKind::Other)
        .as_str()
        .eq_ignore_ascii_case(kind)
        || sym
            .lang_kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case(kind))
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

/// `generic` or `generic/language-specific` label of a symbol node.
fn kind_label(n: &Node) -> String {
    let generic = n.symbol_kind.unwrap_or(SymbolKind::Other).as_str();
    match &n.lang_kind {
        Some(k) if k != generic => format!("{generic}/{k}"),
        _ => generic.to_string(),
    }
}

/// Where a file lives, for catalog bookkeeping.
struct Scope<'a> {
    org: &'a str,
    repo: &'a str,
    lang: &'a str,
}

/// Catalog deltas accumulated during one write transaction and applied to the
/// table just before commit (so an aborted transaction changes nothing).
#[derive(Default)]
struct Tally(BTreeMap<String, i64>);

impl Tally {
    fn add(&mut self, key: String, d: i64) {
        *self.0.entry(key).or_default() += d;
    }
    fn file(&mut self, s: &Scope, d: i64) {
        self.add(format!("f\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
    }
    /// A symbol or token node appearing (`d` = 1) or disappearing (`d` = -1).
    fn node(&mut self, s: &Scope, n: &Node, d: i64) {
        match n.kind {
            NodeKind::Symbol => {
                self.add(format!("s\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
                let label = kind_label(n);
                self.add(format!("k\0{}\0{}\0{}\0{label}", s.org, s.repo, s.lang), d);
            }
            NodeKind::Token => {
                self.add(format!("t\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
                if let Some(c) = n.token_class {
                    self.add(format!("c\0{}\0{}\0{}", s.org, s.repo, c.as_str()), d);
                }
            }
            _ => {}
        }
    }
    fn apply(self, cat: &mut redb::Table<&str, u64>) -> Result<()> {
        for (k, d) in self.0 {
            if d == 0 {
                continue;
            }
            let cur = cat.get(k.as_str())?.map_or(0, |v| v.value()) as i64;
            let new = cur + d;
            if new > 0 {
                cat.insert(k.as_str(), new as u64)?;
            } else {
                cat.remove(k.as_str())?;
            }
        }
        Ok(())
    }
}

/// Delete everything below `root` (not `root` itself), including token postings.
fn remove_descendants(
    nodes: &mut redb::Table<u64, &[u8]>,
    children: &mut redb::MultimapTable<u64, u64>,
    tokens: &mut redb::MultimapTable<&str, u64>,
    symbols: &mut redb::MultimapTable<&str, u64>,
    root: NodeId,
    (tally, scope): (&mut Tally, &Scope),
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
            tally.node(scope, &n, -1);
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

impl RedbStore {
    /// Open or create a database file. The file must be writable: redb 2 has
    /// no read-only open mode. Fails without modifying the file on a schema
    /// mismatch, on a symbol index newer than this build, or when another
    /// process holds it. A database whose symbol index is missing or older is
    /// rebuilt once on open. A version-1 database (written before the describe
    /// catalog) is upgraded in place, which writes once: it needs a writable
    /// file, and afterwards builds that predate the catalog refuse it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path.as_ref()).map_err(|e| match e {
            DatabaseError::DatabaseAlreadyOpen => {
                StoreError::Locked(path.as_ref().display().to_string())
            }
            e => open_failed(path.as_ref(), &e),
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
            Some(v) if !(MIN_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&v) => {
                return Err(StoreError::SchemaMismatch { found: v })
            }
            Some(_) => {
                // Databases written before the symbol index existed, or with an
                // older index layout: rebuild it once. Nothing is written when
                // the index is current. A newer index is never rewritten (that
                // would silently downgrade it).
                let current = {
                    let rt = db.begin_read()?;
                    let ver = match rt.open_table(META) {
                        Ok(t) => t.get("symbol_index_version")?.map(|v| v.value()),
                        Err(redb::TableError::TableDoesNotExist(_)) => None,
                        Err(e) => return Err(e.into()),
                    };
                    if let Some(v) = ver.filter(|&v| v > SYMBOL_INDEX_VERSION) {
                        return Err(StoreError::IndexTooNew { found: v });
                    }
                    ver == Some(SYMBOL_INDEX_VERSION)
                        && !matches!(
                            rt.open_multimap_table(SYMBOLS),
                            Err(redb::TableError::TableDoesNotExist(_))
                        )
                };
                if !current {
                    let wt = db.begin_write()?;
                    {
                        let nodes = wt.open_table(NODES)?;
                        wt.delete_multimap_table(SYMBOLS)?;
                        let mut idx = wt.open_multimap_table(SYMBOLS)?;
                        for r in nodes.iter()? {
                            let (id, v) = r?;
                            let n = dec(v.value())?;
                            if n.kind == NodeKind::Symbol {
                                idx.insert(n.name.as_str(), id.value())?;
                            }
                        }
                        wt.open_table(META)?
                            .insert("symbol_index_version", SYMBOL_INDEX_VERSION)?;
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
                    m.insert("symbol_index_version", SYMBOL_INDEX_VERSION)?;
                    m.insert("catalog_version", CATALOG_VERSION)?;
                    wt.open_table(CATALOG)?;
                    wt.open_table(NODES)?;
                    wt.open_table(NAMES)?;
                    wt.open_multimap_table(CHILDREN)?;
                    wt.open_multimap_table(TOKENS)?;
                    wt.open_multimap_table(SYMBOLS)?;
                }
                wt.commit()?;
            }
        }
        let store = Self {
            db,
            registry: Registry::default(),
        };
        store.ensure_catalog()?;
        Ok(store)
    }

    /// Rebuild the describe catalog once when it is missing or of another
    /// version (databases written before it existed). Nothing is written when
    /// it is current.
    fn ensure_catalog(&self) -> Result<()> {
        let current = {
            let rt = self.db.begin_read()?;
            let ver = rt
                .open_table(META)?
                .get("catalog_version")?
                .map(|v| v.value());
            let schema = rt
                .open_table(META)?
                .get("schema_version")?
                .map(|v| v.value());
            ver == Some(CATALOG_VERSION)
                && schema == Some(SCHEMA_VERSION)
                && !matches!(
                    rt.open_table(CATALOG),
                    Err(redb::TableError::TableDoesNotExist(_))
                )
        };
        if current {
            return Ok(());
        }
        let infos = self.describe_by_scan(None, None)?;
        let wt = self.db.begin_write()?;
        {
            wt.delete_table(CATALOG)?;
            let mut cat = wt.open_table(CATALOG)?;
            for i in &infos {
                let (o, r) = (&i.org, &i.repo);
                cat.insert(format!("r\0{o}\0{r}").as_str(), 0)?;
                for (l, li) in &i.languages {
                    cat.insert(format!("f\0{o}\0{r}\0{l}").as_str(), li.files as u64)?;
                    if li.symbols > 0 {
                        cat.insert(format!("s\0{o}\0{r}\0{l}").as_str(), li.symbols as u64)?;
                    }
                    if li.tokens > 0 {
                        cat.insert(format!("t\0{o}\0{r}\0{l}").as_str(), li.tokens as u64)?;
                    }
                    for (k, n) in &li.symbol_kinds {
                        cat.insert(format!("k\0{o}\0{r}\0{l}\0{k}").as_str(), *n as u64)?;
                    }
                }
                for (c, n) in &i.token_classes {
                    cat.insert(format!("c\0{o}\0{r}\0{c}").as_str(), *n as u64)?;
                }
            }
            let mut m = wt.open_table(META)?;
            m.insert("catalog_version", CATALOG_VERSION)?;
            m.insert("schema_version", SCHEMA_VERSION)?;
        }
        wt.commit()?;
        Ok(())
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
            let mut cat = wt.open_table(CATALOG)?;
            let mut tally = Tally::default();
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
                    let scope = Scope {
                        org,
                        repo,
                        lang: f.language.as_deref().unwrap_or("unknown"),
                    };
                    remove_descendants(
                        &mut nodes,
                        &mut children,
                        &mut tokens,
                        &mut sym_idx,
                        fid,
                        (&mut tally, &scope),
                    )?;
                    tally.file(&scope, -1);
                    nodes.remove(fid)?;
                    names.remove(name_key(Some(repo_id), NodeKind::File, &f.name).as_str())?;
                    children.remove(repo_id, fid)?;
                    removed.push(f.name);
                }
            }
            tally.apply(&mut cat)?;
        }
        if dry_run {
            wt.abort()?;
        } else {
            wt.commit()?;
        }
        removed.sort();
        Ok(removed)
    }

    /// Fingerprint of `bytes` indexed as `lang` with the current extractor.
    fn fingerprint(&self, bytes: &[u8], lang: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!(
            "sha256:{hash}|{}|{}|{FINGERPRINT_FORMAT_VERSION}",
            lang.to_ascii_lowercase(),
            self.registry.version(lang)
        )
    }

    /// If the file already stored under org/repo/path carries `fp`, leave its
    /// nodes alone (only refreshing `origin` if it differs) and return its
    /// stats plus whether anything was written.
    fn check_unchanged(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        path: &str,
        lang: &str,
        fp: &str,
        origin: Option<&str>,
    ) -> Result<Option<(IngestStats, bool)>> {
        let names = wt.open_table(NAMES)?;
        let mut nodes = wt.open_table(NODES)?;
        let find = |parent: Option<NodeId>, kind: NodeKind, name: &str| -> Result<Option<NodeId>> {
            Ok(names
                .get(name_key(parent, kind, name).as_str())?
                .map(|v| v.value()))
        };
        let Some(org_id) = find(None, NodeKind::Org, org)? else {
            return Ok(None);
        };
        let Some(repo_id) = find(Some(org_id), NodeKind::Repo, repo)? else {
            return Ok(None);
        };
        let Some(file_id) = find(Some(repo_id), NodeKind::File, path)? else {
            return Ok(None);
        };
        let Some(raw) = nodes.get(file_id)? else {
            return Ok(None);
        };
        let mut f = dec(raw.value())?;
        drop(raw);
        if f.fingerprint.as_deref() != Some(fp) {
            return Ok(None);
        }
        let dirty = f.origin.as_deref() != origin;
        let stats = IngestStats {
            file_id,
            unchanged: true,
            has_errors: f.has_errors,
            path: path.to_string(),
            language: lang.to_string(),
            ..IngestStats::default()
        };
        if dirty {
            f.origin = origin.map(Into::into);
            nodes.insert(file_id, enc(&f).as_slice())?;
        }
        Ok(Some((stats, dirty)))
    }

    /// Register a language extractor used by `index_bytes`.
    ///
    /// Register every shipped extractor before indexing: the extractor version
    /// is part of a file's fingerprint, so a store without (say) the Rust
    /// extractor treats already-indexed Rust files as changed and re-indexes
    /// them with the token-only fallback, silently dropping their symbols.
    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.registry.register(e);
    }

    /// Like `index_bytes_with_origin` with explicit `opts` (e.g. `reindex`).
    #[allow(clippy::too_many_arguments)]
    pub fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats> {
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(StoreError::TooLarge(format!("`{path}`")));
        }
        let src =
            std::str::from_utf8(bytes).map_err(|_| StoreError::NotUtf8(format!("`{path}`")))?;
        let path = normalize_path(path);
        let lang = language.map_or_else(
            || detect_language_from_content(&path, src),
            str::to_ascii_lowercase,
        );
        let fp = self.fingerprint(bytes, &lang);
        let wt = self.db.begin_write()?;
        if !opts.reindex {
            if let Some((stats, dirty)) =
                Self::check_unchanged(&wt, org, repo, &path, &lang, &fp, origin)?
            {
                if dirty {
                    wt.commit()?;
                }
                return Ok(stats);
            }
        }
        let ex = self.registry.extract(&lang, src);
        let stats = Self::ingest_into(&wt, org, repo, &path, &lang, &ex, (origin, Some(&fp)))?;
        wt.commit()?;
        Ok(stats)
    }

    pub fn get(&self, id: NodeId) -> Result<Option<Node>> {
        Self::get_in(&self.db.begin_read()?, id)
    }

    fn get_in(rt: &ReadTransaction, id: NodeId) -> Result<Option<Node>> {
        let t = rt.open_table(NODES)?;
        let r = t.get(id)?.map(|v| dec(v.value())).transpose();
        r
    }

    /// Direct children in creation order (see `StoreRead::children`).
    fn children_in(rt: &ReadTransaction, id: NodeId) -> Result<Vec<Node>> {
        let kids = rt.open_multimap_table(CHILDREN)?;
        let nodes = rt.open_table(NODES)?;
        let mut out = Vec::new();
        for k in kids.get(id)? {
            let k = k?.value();
            let n = nodes
                .get(k)?
                .ok_or_else(|| StoreError::Corrupt(format!("dangling child {k}")))?;
            out.push(dec(n.value())?);
        }
        Ok(out)
    }

    /// Parent pointer lookup (one hop).
    pub fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        Self::parent_in(&self.db.begin_read()?, id)
    }

    fn parent_in(rt: &ReadTransaction, id: NodeId) -> Result<Option<Node>> {
        match Self::get_in(rt, id)? {
            Some(Node {
                parent: Some(p), ..
            }) => Self::get_in(rt, p),
            _ => Ok(None),
        }
    }

    pub fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        Self::count_nodes_in(&self.db.begin_read()?, kind)
    }

    fn count_nodes_in(rt: &ReadTransaction, kind: NodeKind) -> Result<usize> {
        let t = rt.open_table(NODES)?;
        let mut n = 0;
        for r in t.iter()? {
            if dec(r?.1.value())?.kind == kind {
                n += 1;
            }
        }
        Ok(n)
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
        let wt = self.db.begin_write()?;
        let stats = Self::ingest_into(&wt, org, repo, path, language, ex, (origin, None))?;
        wt.commit()?;
        Ok(stats)
    }

    /// Index many files of one repo in a single write transaction (one commit
    /// instead of one per file). Each file is extracted just before it is
    /// stored and its extraction dropped right after, so memory use is bounded
    /// by the sources, not by the number of tokens across the batch. Files
    /// that are not UTF-8, too large, or whose extraction has invalid spans
    /// (`InvalidSpan`, message prefixed with the path) yield a per-file `Err`
    /// and are not stored (an already-stored version is left untouched); a
    /// storage error aborts the whole batch (nothing is stored, later files are
    /// not extracted). Outcomes are in input order.
    pub fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        let wt = self.db.begin_write()?;
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            if f.bytes.len() > MAX_SOURCE_BYTES {
                out.push(Err(StoreError::TooLarge(format!("`{}`", f.path))));
                continue;
            }
            let Ok(src) = std::str::from_utf8(f.bytes) else {
                out.push(Err(StoreError::NotUtf8(format!("`{}`", f.path))));
                continue;
            };
            let path = normalize_path(f.path);
            let lang = f.language.map_or_else(
                || detect_language_from_content(&path, src),
                str::to_ascii_lowercase,
            );
            let fp = self.fingerprint(f.bytes, &lang);
            if !opts.reindex {
                if let Some((stats, _)) =
                    Self::check_unchanged(&wt, org, repo, &path, &lang, &fp, f.origin)?
                {
                    out.push(Ok(stats));
                    continue;
                }
            }
            let ex = self.registry.extract(&lang, src);
            // Validate before touching the transaction so a bad file leaves
            // no partial writes and only fails itself.
            if let Err(StoreError::InvalidSpan(why)) = validate_spans(&ex) {
                out.push(Err(StoreError::InvalidSpan(format!("`{path}`: {why}"))));
                continue;
            }
            out.push(Ok(Self::ingest_validated(
                &wt,
                org,
                repo,
                &path,
                &lang,
                &ex,
                (f.origin, Some(&fp)),
            )?));
        }
        wt.commit()?;
        Ok(out)
    }

    /// Validate spans (the single `validate_spans` check), then write.
    fn ingest_into(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        meta: (Option<&str>, Option<&str>),
    ) -> Result<IngestStats> {
        validate_spans(ex)?;
        Self::ingest_validated(wt, org, repo, path, language, ex, meta)
    }

    /// Write an extraction whose spans `validate_spans` accepted (it relies on
    /// proper nesting when deriving parents).
    fn ingest_validated(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        (origin, fingerprint): (Option<&str>, Option<&str>),
    ) -> Result<IngestStats> {
        if org.is_empty() || repo.is_empty() {
            return Err(StoreError::Rejected(
                "org and repo must not be empty".into(),
            ));
        }
        // NUL separates the fields of catalog and name keys.
        let nul = |what: &str, v: &str| {
            if v.contains('\0') {
                Err(StoreError::Rejected(format!(
                    "{what} must not contain NUL bytes"
                )))
            } else {
                Ok(())
            }
        };
        nul("org", org)?;
        nul("repo", repo)?;
        nul("language", language)?;
        for sd in &ex.symbols {
            if let Some(k) = &sd.lang_kind {
                nul("symbol lang_kind", k)?;
            }
        }
        let language = language.to_ascii_lowercase();
        let language = language.as_str();
        let mut stats = IngestStats::default();
        {
            let mut meta = wt.open_table(META)?;
            let mut nodes = wt.open_table(NODES)?;
            let mut names = wt.open_table(NAMES)?;
            let mut children = wt.open_multimap_table(CHILDREN)?;
            let mut tokens = wt.open_multimap_table(TOKENS)?;
            let mut sym_idx = wt.open_multimap_table(SYMBOLS)?;
            let mut cat = wt.open_table(CATALOG)?;
            let mut tally = Tally::default();
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
                    fingerprint: None,
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
            cat.insert(format!("r\0{org}\0{repo}").as_str(), 0)?;
            let scope = Scope {
                org,
                repo,
                lang: language,
            };
            tally.file(&scope, 1);
            if !existed {
                let mut f = dec(nodes.get(file_id)?.expect("file node").value())?;
                f.has_errors = ex.has_errors;
                f.origin = origin.map(Into::into);
                f.fingerprint = fingerprint.map(Into::into);
                nodes.insert(file_id, enc(&f).as_slice())?;
            }
            stats.replaced = existed;
            stats.has_errors = ex.has_errors;
            stats.path = path.to_string();
            stats.language = language.to_string();

            if existed {
                // Drop the old subtree (counted under the file's old language).
                let old = dec(nodes.get(file_id)?.expect("file node").value())?;
                let old_scope = Scope {
                    org,
                    repo,
                    lang: old.language.as_deref().unwrap_or("unknown"),
                };
                tally.file(&old_scope, -1);
                remove_descendants(
                    &mut nodes,
                    &mut children,
                    &mut tokens,
                    &mut sym_idx,
                    file_id,
                    (&mut tally, &old_scope),
                )?;
                // Refresh language.
                let mut f = dec(nodes.get(file_id)?.expect("file node").value())?;
                f.language = Some(language.into());
                f.has_errors = ex.has_errors;
                f.origin = origin.map(Into::into);
                f.fingerprint = fingerprint.map(Into::into);
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
                if take_sym {
                    let s = syms[si];
                    si += 1;
                    check_contains(pkind, NodeKind::Symbol).map_err(StoreError::Schema)?;
                    let id = alloc(&mut next);
                    let node = Node {
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
                        fingerprint: None,
                        span: Some(s.span),
                    };
                    tally.node(&scope, &node, 1);
                    insert(node, &mut nodes, &mut children)?;
                    sym_idx.insert(s.name.as_str(), id)?;
                    open.push((id, s.span.end));
                    stats.symbols += 1;
                } else {
                    let t = toks[ti];
                    ti += 1;
                    check_contains(pkind, NodeKind::Token).map_err(StoreError::Schema)?;
                    let id = alloc(&mut next);
                    let node = Node {
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
                        fingerprint: None,
                        span: Some(t.span),
                    };
                    tally.node(&scope, &node, 1);
                    insert(node, &mut nodes, &mut children)?;
                    tokens.insert(t.text.as_str(), id)?;
                    stats.tokens += 1;
                }
            }
            meta.insert("next_id", next)?;
            tally.apply(&mut cat)?;
        }
        Ok(stats)
    }

    /// All tokens stored for one file, in source order. `None` if the file is
    /// not indexed. Used to verify that everything parsed was stored.
    pub fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        let rt = self.db.begin_read()?;
        Self::file_tokens_in(&rt, org, repo, path)
    }

    fn file_tokens_in(
        rt: &ReadTransaction,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<Option<Vec<Node>>> {
        let names = rt.open_table(NAMES)?;
        let nodes = rt.open_table(NODES)?;
        let kids = rt.open_multimap_table(CHILDREN)?;
        let find = |parent: Option<NodeId>, kind: NodeKind, name: &str| -> Result<Option<NodeId>> {
            Ok(names
                .get(name_key(parent, kind, name).as_str())?
                .map(|v| v.value()))
        };
        let Some(o) = find(None, NodeKind::Org, org)? else {
            return Ok(None);
        };
        let Some(r) = find(Some(o), NodeKind::Repo, repo)? else {
            return Ok(None);
        };
        let Some(f) = find(Some(r), NodeKind::File, &normalize_path(path))? else {
            return Ok(None);
        };
        let mut out = Vec::new();
        let mut stack = vec![f];
        while let Some(id) = stack.pop() {
            for c in kids.get(id)? {
                let cid = c?.value();
                let n = dec(nodes
                    .get(cid)?
                    .ok_or_else(|| StoreError::Corrupt(format!("dangling node {cid}")))?
                    .value())?;
                if n.kind == NodeKind::Token {
                    out.push(n);
                } else {
                    stack.push(cid);
                }
            }
        }
        out.sort_by_key(|n| n.span.map_or(0, |s| s.start));
        Ok(Some(out))
    }

    /// What is actually in the database (optionally scoped to an org and/or
    /// repo): per repo, the languages present and, per language, its symbol
    /// kinds and token counts. Callers use this to discover valid filter
    /// values instead of guessing them; a repo may be polyglot.
    pub fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        let rt = self.db.begin_read()?;
        Self::describe_in(&rt, org, repo)
    }

    fn describe_in(
        rt: &ReadTransaction,
        org: Option<&str>,
        repo: Option<&str>,
    ) -> Result<Vec<RepoInfo>> {
        let cat = rt.open_table(CATALOG)?;
        let mut infos: BTreeMap<(String, String), RepoInfo> = BTreeMap::new();
        let corrupt = || StoreError::Corrupt("bad catalog key".into());
        let mut seen_repo = std::collections::HashSet::new();
        for r in cat.iter()? {
            let (k, v) = r?;
            let (k, v) = (k.value(), v.value() as usize);
            let f: Vec<&str> = k.split('\0').collect();
            if f.len() < 3 {
                return Err(corrupt());
            }
            if org.is_some_and(|o| f[1] != o) || repo.is_some_and(|r| f[2] != r) {
                continue;
            }
            if f[0] == "r" {
                seen_repo.insert((f[1].to_string(), f[2].to_string()));
            }
            let info = infos
                .entry((f[1].to_string(), f[2].to_string()))
                .or_insert_with(|| RepoInfo {
                    org: f[1].to_string(),
                    repo: f[2].to_string(),
                    files: 0,
                    languages: BTreeMap::new(),
                    token_classes: BTreeMap::new(),
                });
            match (f[0], f.len()) {
                ("r", 3) => {}
                ("c", 4) => {
                    info.token_classes.insert(f[3].to_string(), v);
                }
                ("f", 4) => {
                    info.files += v;
                    info.languages.entry(f[3].to_string()).or_default().files = v;
                }
                ("s", 4) => info.languages.entry(f[3].to_string()).or_default().symbols = v,
                ("t", 4) => info.languages.entry(f[3].to_string()).or_default().tokens = v,
                ("k", 5) => {
                    info.languages
                        .entry(f[3].to_string())
                        .or_default()
                        .symbol_kinds
                        .insert(f[4].to_string(), v);
                }
                _ => return Err(corrupt()),
            }
        }
        if infos.keys().any(|k| !seen_repo.contains(k)) {
            return Err(StoreError::Corrupt(
                "catalog has counters for a repo without its marker row".into(),
            ));
        }
        Ok(infos.into_values().collect())
    }

    /// `describe` computed by decoding every node (O(tokens)). Used to build
    /// the catalog and as the reference the catalog is tested against.
    #[doc(hidden)]
    pub fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        let rt = self.db.begin_read()?;
        Self::describe_by_scan_in(&rt, org, repo)
    }

    fn describe_by_scan_in(
        rt: &ReadTransaction,
        org: Option<&str>,
        repo: Option<&str>,
    ) -> Result<Vec<RepoInfo>> {
        use std::collections::HashMap;
        let nodes = rt.open_table(NODES)?;
        let mut all: Vec<Node> = Vec::new();
        for r in nodes.iter()? {
            all.push(dec(r?.1.value())?);
        }
        all.sort_by_key(|n| n.id); // parents are always created before children

        let mut org_name: HashMap<NodeId, String> = HashMap::new();
        let mut repo_key: HashMap<NodeId, (String, String)> = HashMap::new();
        let mut file_of: HashMap<NodeId, NodeId> = HashMap::new(); // symbol -> its file
        let mut file_info: HashMap<NodeId, ((String, String), String)> = HashMap::new();
        let mut infos: BTreeMap<(String, String), RepoInfo> = BTreeMap::new();
        for n in &all {
            match n.kind {
                NodeKind::Org => {
                    org_name.insert(n.id, n.name.clone());
                }
                NodeKind::Repo => {
                    let o = n
                        .parent
                        .and_then(|p| org_name.get(&p))
                        .cloned()
                        .unwrap_or_default();
                    let key = (o.clone(), n.name.clone());
                    infos.entry(key.clone()).or_insert_with(|| RepoInfo {
                        org: o,
                        repo: n.name.clone(),
                        files: 0,
                        languages: BTreeMap::new(),
                        token_classes: BTreeMap::new(),
                    });
                    repo_key.insert(n.id, key);
                }
                NodeKind::File => {
                    let Some(key) = n.parent.and_then(|p| repo_key.get(&p)).cloned() else {
                        continue;
                    };
                    let lang = n.language.clone().unwrap_or_else(|| "unknown".into());
                    let info = infos.get_mut(&key).expect("repo registered");
                    info.files += 1;
                    info.languages.entry(lang.clone()).or_default().files += 1;
                    file_info.insert(n.id, (key, lang));
                }
                NodeKind::Symbol | NodeKind::Token => {
                    let Some(parent) = n.parent else { continue };
                    let file = if file_info.contains_key(&parent) {
                        parent
                    } else {
                        *file_of.get(&parent).unwrap_or(&0)
                    };
                    let Some((key, lang)) = file_info.get(&file) else {
                        continue;
                    };
                    let info = infos.get_mut(key).expect("repo registered");
                    let l = info.languages.entry(lang.clone()).or_default();
                    if n.kind == NodeKind::Symbol {
                        file_of.insert(n.id, file);
                        l.symbols += 1;
                        *l.symbol_kinds.entry(kind_label(n)).or_default() += 1;
                    } else {
                        l.tokens += 1;
                        if let Some(c) = n.token_class {
                            *info
                                .token_classes
                                .entry(c.as_str().to_string())
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
        Ok(infos
            .into_values()
            .filter(|i| org.is_none_or(|o| i.org == o) && repo.is_none_or(|r| i.repo == r))
            .collect())
    }

    /// Find symbols by name. `pattern` is exact, or a prefix when it ends in `*`.
    /// Results are ordered by org, repo, file and byte offset.
    pub fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        let rt = self.db.begin_read()?;
        Self::search_symbols_in(&rt, q)
    }

    fn search_symbols_in(rt: &ReadTransaction, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        let nodes = rt.open_table(NODES)?;
        let idx = rt.open_multimap_table(SYMBOLS)?;
        let load = |id: NodeId| -> Result<Node> {
            dec(nodes
                .get(id)?
                .ok_or_else(|| StoreError::Corrupt(format!("dangling node {id}")))?
                .value())
        };
        for (what, v) in [
            ("org", &q.org),
            ("repo", &q.repo),
            ("language", &q.language),
            ("file", &q.file),
            ("kind", &q.kind),
        ] {
            if v.as_deref() == Some("") {
                return Err(StoreError::Rejected(format!("empty {what} filter")));
            }
        }
        let mut ids: Vec<NodeId> = Vec::new();
        if q.pattern.is_empty() {
            return Err(StoreError::Rejected(
                "empty symbol pattern (use `*` to list all symbols)".into(),
            ));
        }
        if q.pattern.ends_with("**") {
            return Err(StoreError::Rejected(
                "pattern ending in `**` is ambiguous: use `prefix*` for a prefix or `name\\*` for a literal `*`".into(),
            ));
        }
        if let Some(lit) = q.pattern.strip_suffix("\\*") {
            let name = format!("{lit}*");
            for v in idx.get(name.as_str())? {
                ids.push(v?.value());
            }
        } else {
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
        }
        let want_file = q.file.as_deref().map(normalize_path);
        let want_lang = q.language.as_deref().map(str::to_ascii_lowercase);
        let mut out: Vec<(NodeId, SymbolHit)> = Vec::new();
        for id in ids {
            // A dangling index entry (stale index) is skipped, not an error.
            let Some(raw) = nodes.get(id)? else { continue };
            let sym = dec(raw.value())?;
            drop(raw);
            if sym.kind != NodeKind::Symbol {
                continue;
            }
            if q.kind.as_deref().is_some_and(|k| !kind_matches(&sym, k)) {
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
            out.push((
                id,
                SymbolHit {
                    org: org.name,
                    repo: repo.name,
                    file: file.name,
                    language: file.language,
                    name: sym.name,
                    qualified: quals.join("::"),
                    kind: sym.symbol_kind.unwrap_or(SymbolKind::Other),
                    lang_kind: sym.lang_kind,
                    span: sym.span,
                },
            ));
        }
        // Total order: the node id breaks any remaining tie.
        out.sort_by(|(ia, a), (ib, b)| {
            (
                &a.org,
                &a.repo,
                &a.file,
                a.span.map(|s| s.start),
                &a.qualified,
                ia,
            )
                .cmp(&(
                    &b.org,
                    &b.repo,
                    &b.file,
                    b.span.map(|s| s.start),
                    &b.qualified,
                    ib,
                ))
        });
        let mut out: Vec<SymbolHit> = out.into_iter().map(|(_, h)| h).collect();
        if let Some(n) = q.limit {
            out.truncate(n);
        }
        Ok(out)
    }

    /// Token-text search with roll-up to the requested grain.
    pub fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        let rt = self.db.begin_read()?;
        Self::search_in(&rt, q)
    }

    fn search_in(rt: &ReadTransaction, q: &Query) -> Result<Vec<Hit>> {
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
                        .position(|s| q.symbol_kind.as_deref().is_none_or(|k| kind_matches(s, k)));
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
        // BTreeMap order: org, repo, file, offset, node id (deterministic).
        let n = q.limit.unwrap_or(usize::MAX);
        Ok(rows.into_values().take(n).collect())
    }
}

#[cfg(test)]
mod detect_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod v2_random;
#[cfg(test)]
mod v2_tests;
