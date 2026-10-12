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
        Ok(Self { root, writer })
    }

    /// The canonical index root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` (relative to the root, or absolute) as a canonical directory
    /// inside the root, and how it reads relative to the root.
    fn resolve(&self, tool: &str, path: &str) -> Result<(PathBuf, String), ToolError> {
        let joined = self.root.join(path);
        let canon = std::fs::canonicalize(&joined).map_err(|e| {
            ToolError::InvalidArgument(format!(
                "{tool}: `{path}` cannot be resolved under the index root: {e}"
            ))
        })?;
        let Ok(rel) = canon.strip_prefix(&self.root) else {
            return Err(ToolError::InvalidArgument(format!(
                "{tool}: `{path}` is outside the index root (serve --mcp-index-root / mcp --index-root); paths are resolved, symlinks included, before the check"
            )));
        };
        if !canon.is_dir() {
            return Err(ToolError::InvalidArgument(format!(
                "{tool}: `{path}` is not a directory; index_path and prune take a directory"
            )));
        }
        let rel = graph_core::normalize_path(&rel.to_string_lossy());
        let shown = if rel.is_empty() { ".".to_string() } else { rel };
        Ok((canon, shown))
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
        let walk = walk(&dir);
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
                Walked::File { rel, path } => match std::fs::read(&path) {
                    Err(_) => note_skip(rel, "unreadable"),
                    Ok(bytes) if bytes.len() > graph_store::MAX_SOURCE_BYTES => {
                        note_skip(rel, "too large")
                    }
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
        let walk = walk(&dir);
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
fn walk(dir: &Path) -> Walk {
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
        let Some(rel_s) = rel_path.to_str() else {
            errors.push(format!("{} (not UTF-8)", rel_path.to_string_lossy()));
            continue;
        };
        if !cfg!(windows) && rel_s.contains('\\') {
            items.push(Walked::Skip {
                rel: rel_s.to_string(),
                reason: "`\\` in file name (a path separator in stored paths)",
            });
            continue;
        }
        let rel = graph_core::normalize_path(rel_s);
        if ft.is_symlink() {
            items.push(Walked::Skip {
                rel,
                reason: "symlink",
            });
        } else if !ft.is_file() {
            items.push(Walked::Skip {
                rel,
                reason: "not a regular file",
            });
        } else {
            items.push(Walked::File {
                rel,
                path: entry.into_path(),
            });
        }
    }
    Walk { items, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(bad("../secret").contains("outside the index root"));
        assert!(bad("sub/../../secret").contains("outside the index root"));
        let abs = outer.path().join("secret");
        assert!(bad(abs.to_str().unwrap()).contains("outside the index root"));
        assert!(bad("missing").contains("cannot be resolved"));
        assert!(bad("sub/a.txt").contains("not a directory"));
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
}
