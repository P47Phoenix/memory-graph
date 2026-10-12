//! The opt-in write tools (epic story 33, ADR 0005 D2, ADR 0012 D9):
//! `index_path` indexes a directory as one repo, the way `memory-graph
//! index` does, and `prune` removes a repo's directory-indexed files that
//! are no longer under a directory (`index --prune`).
//!
//! They exist only when the host turns them on ([`WriteAccess`], given to
//! [`StoreBackend::with_writes`](crate::StoreBackend::with_writes)): `serve
//! --mcp-allow-writes` or `mcp --allow-writes`, each with an index root.
//! Otherwise they are not listed and a call is an unknown tool, so the
//! read-only default is unchanged. Over HTTP with `--auth-tokens` a call
//! needs a `write` token (HTTP 403 for a `read` one).
//!
//! Every path is confined to the index root: the argument is resolved
//! against it, canonicalised (symlinks and `..` resolved), and refused
//! unless the result is a directory inside the root. The walk itself
//! never follows a symlink. Where the bytes go is a [`McpWriter`]:
//! [`StoreWriter`] over any [`Store`] (embedded, or a `RemoteStore`), or
//! the server's in-process Write service inside `serve`.
use crate::tools::{store_error_code, ToolError};
use graph_store::{BatchFile, IndexOptions, IngestStats, Store, StoreError};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The write tool names, in `tools/list` order (after the read tools).
pub const WRITE_TOOL_NAMES: [&str; 2] = ["index_path", "prune"];

/// Source bytes read before one batch is handed to the writer.
pub const WRITE_BATCH_BYTES: usize = 32 << 20;

/// The skip reason of a file over the store's span limit (the CLI's).
pub const TOO_LARGE: &str = "larger than 4 GiB (span limit)";

/// At most this many entries in each list of a write tool's answer
/// (`failed`, `skipped`, `removed`); `truncated` says when one was cut.
pub const MAX_LISTED: usize = 200;

/// `origin` of a file indexed by `index_path`: the same as a directory run
/// of the CLI, so `prune` (and `index --prune`) can remove it.
pub const DIRECTORY_ORIGIN: &str = "directory";

/// One file to index: its path as stored (relative to the indexed
/// directory, `/`-separated) and its bytes.
#[derive(Debug, Clone)]
pub struct WriteFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Where the write tools' writes go.
pub trait McpWriter: Send + Sync {
    /// Index `files` into `org`/`repo` with origin [`DIRECTORY_ORIGIN`],
    /// like [`Store::index_batch`]: one outcome per file, in order; `Err`
    /// only for a failure of the whole call.
    fn index_files(
        &self,
        org: &str,
        repo: &str,
        files: Vec<WriteFile>,
        reindex: bool,
    ) -> Result<Vec<Result<IngestStats, StoreError>>, StoreError>;

    /// [`Store::prune_files`].
    fn prune(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>, StoreError>;
}

/// Writes through any [`Store`]: an embedded one, or a `RemoteStore`.
pub struct StoreWriter(pub Arc<dyn Store>);

impl McpWriter for StoreWriter {
    fn index_files(
        &self,
        org: &str,
        repo: &str,
        files: Vec<WriteFile>,
        reindex: bool,
    ) -> Result<Vec<Result<IngestStats, StoreError>>, StoreError> {
        let batch: Vec<BatchFile<'_>> = files
            .iter()
            .map(|f| BatchFile {
                path: &f.path,
                bytes: &f.bytes,
                language: None,
                origin: Some(DIRECTORY_ORIGIN),
                encoding: None,
                strict_encoding: false,
            })
            .collect();
        let opts = IndexOptions {
            reindex,
            ..Default::default()
        };
        self.0.index_batch(org, repo, &batch, opts)
    }

    fn prune(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>, StoreError> {
        self.0.prune_files(org, repo, keep, dry_run)
    }
}

/// The write tools turned on: the index root they are confined to, and
/// the writer.
pub struct WriteAccess {
    root: PathBuf,
    writer: Box<dyn McpWriter>,
    /// The database's own files, never indexed ([`Self::with_excluded`]).
    excluded: Vec<PathBuf>,
}

/// The uniform refusal of a path that is not a directory inside the root:
/// the same words whether it is outside, missing or a file, so the tool is
/// no oracle for what exists outside the root.
fn not_inside(tool: &str, path: &str) -> ToolError {
    ToolError::InvalidArgument(format!(
        "{tool}: `{path}` is not a directory inside the index root (serve --mcp-index-root / mcp --index-root); give a path relative to the root, or an absolute one inside it"
    ))
}

/// `p` canonicalised, or, when it does not exist (yet), its canonical
/// parent joined with its name.
fn canonical_or_parent(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Some(c);
    }
    let parent = std::fs::canonicalize(p.parent()?).ok()?;
    Some(parent.join(p.file_name()?))
}

/// `root` without a `\\?\` disk prefix (Windows canonical paths carry one;
/// a path a user types does not).
fn plain(root: &Path) -> PathBuf {
    let s = root.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => root.to_path_buf(),
    }
}

/// A file the walk found, or why it was left out.
enum Walked {
    File { rel: String, path: PathBuf },
    Skip { rel: String, reason: &'static str },
}

struct Walk {
    items: Vec<Walked>,
    /// Entries the walk could not read (permissions, a vanished file).
    errors: Vec<String>,
}

impl WriteAccess {
    /// Writes confined to `index_root`, which must be an existing
    /// directory; it is canonicalised here.
    pub fn new(index_root: &Path, writer: Box<dyn McpWriter>) -> Result<Self, StoreError> {
        let root = check_index_root(index_root)?;
        Ok(Self {
            root,
            writer,
            excluded: Vec::new(),
        })
    }

    /// Never index these paths (the database's own files: the store file,
    /// a data directory, the Raft log, snapshots, the LOCK sidecar). A file
    /// or directory listed here is skipped as `database file`, and so is a
    /// sibling whose name extends it (`<db>.raft.redb`, `<db>.LOCK`).
    pub fn with_excluded(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.excluded = paths
            .into_iter()
            .filter_map(|p| canonical_or_parent(&p))
            .collect();
        self
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.excluded.iter().any(|x| {
            if path.starts_with(x) {
                return true;
            }
            // A sibling `<name>.*`, or anything under one (`<db>.snapshots/`).
            path.ancestors().any(
                |a| match (a.parent(), x.parent(), a.file_name(), x.file_name()) {
                    (Some(ap), Some(xp), Some(an), Some(xn)) if ap == xp => {
                        let (an, xn) = (an.to_string_lossy(), xn.to_string_lossy());
                        an.strip_prefix(xn.as_ref())
                            .is_some_and(|rest| rest.starts_with('.'))
                    }
                    _ => false,
                },
            )
        })
    }

    /// The canonical index root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` (relative to the root, or absolute) as a canonical directory
    /// inside the root, and how it reads relative to the root.
    fn resolve(&self, tool: &str, path: &str) -> Result<(PathBuf, String), ToolError> {
        let joined = self.lexical(path).ok_or_else(|| not_inside(tool, path))?;
        // Only now touch the file system, for a path that is lexically
        // inside the root; canonicalising resolves symlinks, so check again.
        #[cfg(test)]
        tests::CANONICALIZED.with(|c| c.set(c.get() + 1));
        let canon = std::fs::canonicalize(&joined).map_err(|_| not_inside(tool, path))?;
        let Ok(rel) = canon.strip_prefix(&self.root) else {
            return Err(not_inside(tool, path));
        };
        if !canon.is_dir() {
            return Err(not_inside(tool, path));
        }
        let rel = graph_core::normalize_path(&rel.to_string_lossy());
        let shown = if rel.is_empty() { ".".to_string() } else { rel };
        Ok((canon, shown))
    }

    /// `path` joined to the root, if it is lexically inside it: no UNC,
    /// device or drive-relative prefix, an absolute path only under the
    /// root, and no `..` climbing above it. Nothing is opened here (a UNC
    /// path canonicalised would reach out to its host).
    fn lexical(&self, path: &str) -> Option<PathBuf> {
        use std::path::{Component, Prefix};
        let p = Path::new(path);
        let rel: PathBuf = if p.is_absolute() || p.has_root() {
            if let Some(Component::Prefix(pre)) = p.components().next() {
                if !matches!(pre.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
                    return None;
                }
            }
            let plain_root = plain(&self.root);
            let plain_p = plain(p);
            plain_p
                .strip_prefix(&plain_root)
                .or_else(|_| plain_p.strip_prefix(&self.root))
                .ok()?
                .to_path_buf()
        } else {
            p.to_path_buf()
        };
        let mut depth = 0usize;
        for c in rel.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => return None,
                Component::CurDir => {}
                Component::ParentDir => depth = depth.checked_sub(1)?,
                Component::Normal(_) => depth += 1,
            }
        }
        Some(self.root.join(rel))
    }

    /// The bytes of a walked file, read only when it is still inside the
    /// root (a symlink swapped in since the walk is not followed out) and
    /// its length is within the store's limit; `Err` is the skip reason.
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, &'static str> {
        use std::io::Read;
        let canon = std::fs::canonicalize(path).map_err(|_| "unreadable")?;
        if !canon.starts_with(&self.root) {
            return Err("outside the index root");
        }
        let cap = graph_store::MAX_SOURCE_BYTES as u64;
        let f = std::fs::File::open(&canon).map_err(|_| "unreadable")?;
        let len = f.metadata().map_err(|_| "unreadable")?.len();
        if len > cap {
            return Err(TOO_LARGE);
        }
        let mut bytes = Vec::with_capacity(len as usize);
        f.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "unreadable")?;
        if bytes.len() as u64 > cap {
            return Err(TOO_LARGE);
        }
        Ok(bytes)
    }

    /// The `index_path` tool.
    pub fn index_path(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        reindex: bool,
    ) -> Result<Value, ToolError> {
        let (dir, shown) = self.resolve("index_path", path)?;
        let walk = walk(&dir, &|p: &Path| self.is_excluded(p));
        let mut indexed = 0usize;
        let mut unchanged = 0usize;
        let mut symbols = 0usize;
        let mut tokens = 0usize;
        let mut failed: Vec<Value> = Vec::new();
        let mut failed_n = 0usize;
        let mut skipped: Vec<Value> = Vec::new();
        let mut skipped_n = 0usize;
        let mut note_skip = |rel: String, reason: &str| {
            skipped_n += 1;
            if skipped.len() < MAX_LISTED {
                skipped.push(json!({"path": rel, "reason": reason}));
            }
        };
        let mut batch: Vec<WriteFile> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut flush = |batch: Vec<WriteFile>| -> Result<(), ToolError> {
            if batch.is_empty() {
                return Ok(());
            }
            let paths: Vec<String> = batch.iter().map(|f| f.path.clone()).collect();
            let results = self.writer.index_files(org, repo, batch, reindex)?;
            for (p, r) in paths.into_iter().zip(results) {
                match r {
                    Ok(st) if st.unchanged => unchanged += 1,
                    Ok(st) => {
                        indexed += 1;
                        symbols += st.symbols;
                        tokens += st.tokens;
                    }
                    Err(e) => {
                        failed_n += 1;
                        if failed.len() < MAX_LISTED {
                            failed.push(json!({
                                "path": p, "code": store_error_code(&e), "message": e.to_string()
                            }));
                        }
                    }
                }
            }
            Ok(())
        };
        for item in walk.items {
            match item {
                Walked::Skip { rel, reason } => note_skip(rel, reason),
                Walked::File { rel, path } => match self.read_file(&path) {
                    Err(reason) => note_skip(rel, reason),
                    Ok(bytes) => {
                        if !batch.is_empty() && batch_bytes + bytes.len() > WRITE_BATCH_BYTES {
                            flush(std::mem::take(&mut batch))?;
                            batch_bytes = 0;
                        }
                        batch_bytes += bytes.len();
                        batch.push(WriteFile { path: rel, bytes });
                    }
                },
            }
        }
        flush(batch)?;
        for e in walk.errors {
            note_skip(e, "unreadable");
        }
        Ok(json!({
            "org": org,
            "repo": repo,
            "path": shown,
            "indexed": indexed,
            "unchanged": unchanged,
            "symbols": symbols,
            "tokens": tokens,
            "failed_count": failed_n,
            "failed": failed,
            "skipped_count": skipped_n,
            "skipped": skipped,
            "truncated": failed_n > failed.len() || skipped_n > skipped.len(),
        }))
    }

    /// The `prune` tool.
    pub fn prune(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        dry_run: bool,
    ) -> Result<Value, ToolError> {
        let (dir, shown) = self.resolve("prune", path)?;
        let walk = walk(&dir, &|p: &Path| self.is_excluded(p));
        if !walk.errors.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "prune: {} path(s) under `{shown}` could not be read (first: {}), so what is still there is not known; nothing was removed",
                walk.errors.len(),
                walk.errors[0]
            )));
        }
        // Every file the walk sees is kept, indexable or not, as
        // `index --prune` keeps every file it saw.
        let keep: HashSet<String> = walk
            .items
            .into_iter()
            .map(|w| match w {
                Walked::File { rel, .. } | Walked::Skip { rel, .. } => rel,
            })
            .collect();
        if keep.is_empty() && !dry_run {
            let would = self.writer.prune(org, repo, &keep, true)?;
            if !would.is_empty() {
                return Err(ToolError::InvalidArgument(format!(
                    "prune: `{shown}` has no files, but {org}/{repo} has {} directory-indexed file(s) that would all be removed; nothing was removed (check the path)",
                    would.len()
                )));
            }
        }
        let removed = self.writer.prune(org, repo, &keep, dry_run)?;
        let n = removed.len();
        let listed: Vec<String> = removed.into_iter().take(MAX_LISTED).collect();
        Ok(json!({
            "org": org,
            "repo": repo,
            "path": shown,
            "dry_run": dry_run,
            "removed_count": n,
            "removed": listed,
            "truncated": n > MAX_LISTED,
        }))
    }
}

/// Check an index root (`serve --mcp-index-root`, `mcp --index-root`) and
/// return it canonicalised.
pub fn check_index_root(index_root: &Path) -> Result<PathBuf, StoreError> {
    let root = std::fs::canonicalize(index_root).map_err(|e| {
        StoreError::Rejected(format!(
            "the MCP index root `{}` cannot be resolved: {e}",
            index_root.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(StoreError::Rejected(format!(
            "the MCP index root `{}` is not a directory",
            index_root.display()
        )));
    }
    Ok(root)
}

/// The files under `dir`, in path order, as `memory-graph index` walks
/// them: `.gitignore` honoured, `.git` skipped, symlinks never followed.
fn walk(dir: &Path, excluded: &dyn Fn(&Path) -> bool) -> Walk {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            continue;
        }
        let rel_path = entry.path().strip_prefix(dir).unwrap_or(entry.path());
        // The classification order of `memory-graph index` (graph-cli
        // `index_dir`), so both skip the same files for the same reasons.
        let lossy = graph_core::normalize_path(&rel_path.to_string_lossy());
        let skip = |rel: String, reason: &'static str| Walked::Skip { rel, reason };
        let item = if ft.is_symlink() {
            skip(lossy, "symlink")
        } else if !ft.is_file() {
            skip(lossy, "not a regular file")
        } else if !cfg!(windows) && rel_path.to_str().is_some_and(|s| s.contains('\\')) {
            skip(
                rel_path.to_string_lossy().into_owned(),
                "`\\` in file name (a path separator in stored paths)",
            )
        } else if let Some(rel) = rel_path.to_str().map(graph_core::normalize_path) {
            if excluded(entry.path()) {
                skip(rel, "database file")
            } else {
                Walked::File {
                    rel,
                    path: entry.into_path(),
                }
            }
        } else {
            // A skip, not an error, so prune still runs.
            skip(lossy, "non-UTF-8 path")
        };
        items.push(item);
    }
    Walk { items, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// How many times `resolve` reached the file system.
        pub(super) static CANONICALIZED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn lexical_refusals_never_touch_the_file_system() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let w = WriteAccess::new(&root, Box::new(Nothing)).unwrap();
        let abs_out = outer.path().join("elsewhere");
        let mut bad = vec![
            "..".to_string(),
            "../x".into(),
            "sub/../../x".into(),
            abs_out.to_string_lossy().into_owned(),
        ];
        if cfg!(windows) {
            bad.extend([
                r"\\host\share\x".to_string(),
                r"\\?\UNC\host\share\x".into(),
                r"\\.\pipe\x".into(),
                r"C:foo".into(),
            ]);
        }
        for p in &bad {
            let before = CANONICALIZED.with(|c| c.get());
            assert!(w.resolve("index_path", p).is_err(), "{p}");
            assert_eq!(
                CANONICALIZED.with(|c| c.get()),
                before,
                "{p}: refused lexically, before canonicalize"
            );
        }
        // A good path does reach it.
        let before = CANONICALIZED.with(|c| c.get());
        w.resolve("index_path", "sub").unwrap();
        assert_eq!(CANONICALIZED.with(|c| c.get()), before + 1);
    }

    struct Nothing;
    impl McpWriter for Nothing {
        fn index_files(
            &self,
            _: &str,
            _: &str,
            files: Vec<WriteFile>,
            _: bool,
        ) -> Result<Vec<Result<IngestStats, StoreError>>, StoreError> {
            Ok(files.iter().map(|_| Ok(IngestStats::default())).collect())
        }
        fn prune(
            &self,
            _: &str,
            _: &str,
            _: &HashSet<String>,
            _: bool,
        ) -> Result<Vec<String>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn paths_are_confined_to_the_index_root() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), "a").unwrap();
        std::fs::create_dir_all(outer.path().join("secret")).unwrap();
        let w = WriteAccess::new(&root, Box::new(Nothing)).unwrap();
        let bad = |p: &str| match w.resolve("index_path", p) {
            Err(ToolError::InvalidArgument(m)) => m,
            other => panic!("{p}: {other:?}"),
        };
        // One uniform refusal, whatever the reason: no existence oracle.
        let abs = outer.path().join("secret");
        let uniform = bad("../secret");
        assert!(uniform.contains("not a directory inside the index root"));
        for p in [
            "sub/../../secret",
            abs.to_str().unwrap(),
            "missing",
            "sub/a.txt",
            "../nope-at-all",
        ] {
            assert_eq!(bad(p), uniform.replace("../secret", p), "{p}");
        }
        // Lexical refusals happen before anything is opened.
        assert!(w.lexical("../x").is_none());
        assert!(w.lexical("sub/../../x").is_none());
        assert!(w.lexical("sub/../sub").is_some());
        assert!(w.lexical(abs.to_str().unwrap()).is_none());
        if cfg!(windows) {
            for p in [
                r"\\host\share\x",
                r"\\?\UNC\host\share",
                r"\\.\pipe\x",
                r"C:foo",
                r"\x",
            ] {
                assert!(w.lexical(p).is_none(), "{p}");
            }
        }
        assert_eq!(w.resolve("index_path", ".").unwrap().1, ".");
        assert_eq!(w.resolve("index_path", "sub").unwrap().1, "sub");
        assert_eq!(w.resolve("index_path", "sub/../sub").unwrap().1, "sub");
        let inside = root.join("sub");
        assert_eq!(
            w.resolve("index_path", inside.to_str().unwrap()).unwrap().1,
            "sub"
        );
        assert!(WriteAccess::new(&outer.path().join("nope"), Box::new(Nothing)).is_err());
        assert!(WriteAccess::new(&root.join("sub/a.txt"), Box::new(Nothing)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_is_refused_and_never_walked() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(outer.path().join("secret")).unwrap();
        std::fs::write(outer.path().join("secret/k.txt"), "k").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret"), root.join("link")).unwrap();
        let w = WriteAccess::new(&root, Box::new(Nothing)).unwrap();
        assert!(matches!(
            w.resolve("index_path", "link"),
            Err(ToolError::InvalidArgument(_))
        ));
        let v = w.index_path("o", "r", ".", false).unwrap();
        assert_eq!(v["indexed"], 0, "{v}");
        assert_eq!(v["skipped"][0]["reason"], "symlink", "{v}");
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_out_of_the_root_is_refused_and_never_walked() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(outer.path().join("secret")).unwrap();
        std::fs::write(outer.path().join("secret/k.txt"), "k").unwrap();
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(outer.path().join("secret"))
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let w = WriteAccess::new(&root, Box::new(Nothing)).unwrap();
        assert!(matches!(
            w.resolve("index_path", "link"),
            Err(ToolError::InvalidArgument(_))
        ));
        let v = w.index_path("o", "r", ".", false).unwrap();
        assert_eq!(v["indexed"], 0, "{v}");
        assert!(!v.to_string().contains("k.txt"), "{v}");
    }
    #[test]
    fn database_files_are_never_indexed() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("data/snapshots")).unwrap();
        std::fs::create_dir_all(r.join("g.redb.snapshots")).unwrap();
        for f in [
            "g.redb",
            "g.redb.raft.redb",
            "g.redb.LOCK",
            "g.redbx",
            "data/graph.redb",
            "data/snapshots/s",
            "g.redb.snapshots/s1",
            "keep.rs",
        ] {
            std::fs::write(r.join(f), b"x").unwrap();
        }
        let w = WriteAccess::new(r, Box::new(Nothing))
            .unwrap()
            .with_excluded([r.join("g.redb"), r.join("data")]);
        let v = w.index_path("o", "r", ".", false).unwrap();
        let mut db: Vec<&str> = v["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["reason"] == "database file")
            .map(|s| s["path"].as_str().unwrap())
            .collect();
        db.sort();
        assert_eq!(
            db,
            [
                "data/graph.redb",
                "data/snapshots/s",
                "g.redb",
                "g.redb.LOCK",
                "g.redb.raft.redb",
                "g.redb.snapshots/s1"
            ]
        );
        assert_eq!(v["indexed"], 2, "keep.rs and g.redbx: {v}");
    }
}
