//! Directory indexing, shared by the `memory-graph` binary and its tests.
use anyhow::{bail, Context, Result};
use graph_store::{IndexOptions, Store, ORIGIN_DIRECTORY};
use std::io::{Read, Write};

/// Every extractor compiled into this build, one per enabled `lang-*` Cargo
/// feature. Third-party languages register their own `Extractor` the same
/// way (see `docs/adding-a-language.md`).
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn shipped_extractors() -> Vec<Box<dyn graph_core::Extractor>> {
    let mut v: Vec<Box<dyn graph_core::Extractor>> = Vec::new();
    #[cfg(feature = "lang-rust")]
    v.push(Box::new(graph_lang_rust::RustExtractor));
    #[cfg(feature = "lang-csharp")]
    v.push(Box::new(graph_lang_csharp::CSharpExtractor));
    v
}

macro_rules! out {
    ($w:expr, $($a:tt)*) => {
        writeln!($w, $($a)*)?
    };
}

pub struct DirOpts<'a> {
    pub db: &'a std::path::Path,
    pub org: &'a str,
    pub repo: &'a str,
    pub dir: &'a std::path::Path,
    pub json: bool,
    pub max_file_size: u64,
    pub prune: bool,
    pub force: bool,
    pub reindex: bool,
}

/// Whether `path` is the database file itself: (dev, ino) on unix, otherwise a
/// canonical-path comparison limited to entries with the database's file name.
fn is_db_file(
    meta: &std::fs::Metadata,
    path: &std::path::Path,
    db_meta: Option<&std::fs::Metadata>,
    db_canon: Option<&std::path::Path>,
    db_name: Option<&std::ffi::OsStr>,
) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = (path, db_canon, db_name);
        db_meta.is_some_and(|d| d.dev() == meta.dev() && d.ino() == meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, db_meta);
        path.file_name() == db_name
            && db_canon.is_some()
            && path.canonicalize().ok().as_deref() == db_canon
    }
}

const BATCH_FILES: usize = 256;
const BATCH_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct Tally {
    files: usize,
    unchanged: usize,
    symbols: usize,
    tokens: usize,
    by_lang: std::collections::BTreeMap<String, usize>,
    seen: std::collections::HashSet<String>,
    skipped: std::collections::BTreeMap<String, Vec<String>>,
    /// Files whose extraction failed span validation: (path, reason). Not stored.
    failed: Vec<(String, String)>,
}

/// Store the pending files in one transaction and fold the outcomes into `t`.
fn flush_batch(
    store: &dyn Store,
    o: &DirOpts,
    pending: &mut Vec<(String, Vec<u8>)>,
    t: &mut Tally,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let inputs: Vec<graph_store::BatchFile> = pending
        .iter()
        .map(|(p, b)| graph_store::BatchFile {
            path: p,
            bytes: b,
            language: None,
            origin: Some(ORIGIN_DIRECTORY),
        })
        .collect();
    let outcomes = store
        .index_batch(o.org, o.repo, &inputs, IndexOptions { reindex: o.reindex })
        .with_context(|| {
            format!(
                "database error while indexing a batch of {} files ({} files were already stored)",
                inputs.len(),
                t.files
            )
        })?;
    for ((rel, _), r) in pending.iter().zip(outcomes) {
        match r {
            Ok(st) => {
                t.files += 1;
                t.unchanged += usize::from(st.unchanged);
                t.symbols += st.symbols;
                t.tokens += st.tokens;
                *t.by_lang.entry(st.language).or_default() += 1;
                t.seen.insert(st.path);
            }
            Err(graph_store::StoreError::NotUtf8(_)) => t
                .skipped
                .entry("not valid UTF-8".into())
                .or_default()
                .push(rel.clone()),
            Err(graph_store::StoreError::TooLarge(_)) => t
                .skipped
                .entry("too large".into())
                .or_default()
                .push(rel.clone()),
            Err(graph_store::StoreError::InvalidSpan(why)) => {
                // The store prefixes the path; the CLI names it separately.
                let reason = why
                    .strip_prefix('`')
                    .and_then(|w| w.split_once("`: "))
                    .map_or(why.as_str(), |(_, r)| r);
                let reason = format!("invalid span: {reason}");
                t.failed.push((rel.clone(), reason));
            }
            Err(e) => return Err(e.into()),
        }
    }
    pending.clear();
    Ok(())
}

/// Index every text file under `dir`. Paths are stored relative to `dir`.
/// Per-file problems are counted as skips; only database failures abort.
pub fn index_dir(
    o: DirOpts,
    open: impl FnOnce(&std::path::Path) -> Result<Box<dyn Store>>,
    out: &mut dyn Write,
) -> Result<()> {
    use std::collections::BTreeMap;
    if o.org.is_empty() || o.repo.is_empty() {
        bail!("--org and --repo must not be empty");
    }
    if !o.dir.is_dir() {
        bail!("`{}` is not a directory", o.dir.display());
    }
    if o.db.is_dir() {
        bail!(
            "--db `{}` is a directory; give a database file path",
            o.db.display()
        );
    }
    let start = std::time::Instant::now();
    let store = open(o.db)?;
    let db_meta = std::fs::metadata(o.db).ok();
    let db_canon = o.db.canonicalize().ok();
    let db_name = o.db.file_name();
    let mut tally = Tally::default();
    let mut skipped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Files are stored in batches: one write transaction per batch, not per file.
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut walk_errors = false;
    // Only the directory's own .gitignore files (and parents') apply: global
    // git config, .git/info/exclude and .ignore files would make results
    // differ between machines.
    let walker = ignore::WalkBuilder::new(o.dir)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                walk_errors = true;
                skipped
                    .entry("unreadable".into())
                    .or_default()
                    .push(err.to_string());
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        let rel = entry.path().strip_prefix(o.dir).unwrap_or(entry.path());
        let skip = |skipped: &mut BTreeMap<String, Vec<String>>, why: &str| {
            skipped
                .entry(why.into())
                .or_default()
                .push(rel.to_string_lossy().into_owned());
        };
        let Some(ft) = ft else { continue };
        if ft.is_symlink() {
            skip(&mut skipped, "symlink");
            continue;
        }
        if ft.is_dir() {
            continue;
        }
        if !ft.is_file() {
            skip(&mut skipped, "not a regular file");
            continue;
        }
        let Some(rel_s) = rel.to_str().map(str::to_owned) else {
            skip(&mut skipped, "non-UTF-8 path");
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            walk_errors = true;
            skipped.entry("unreadable".into()).or_default().push(rel_s);
            continue;
        };
        if is_db_file(
            &meta,
            entry.path(),
            db_meta.as_ref(),
            db_canon.as_deref(),
            db_name,
        ) {
            skipped
                .entry("database file".into())
                .or_default()
                .push(rel_s);
            continue;
        }
        // Enforce the cap while reading so a file that grows after the walk
        // cannot exhaust memory.
        let mut bytes = Vec::new();
        let read = std::fs::File::open(entry.path()).and_then(|f| {
            f.take(o.max_file_size.saturating_add(1))
                .read_to_end(&mut bytes)
        });
        if read.is_err() {
            walk_errors = true;
            skipped.entry("unreadable".into()).or_default().push(rel_s);
            continue;
        }
        if bytes.len() as u64 > o.max_file_size {
            skipped.entry("too large".into()).or_default().push(rel_s);
            continue;
        }
        if bytes.contains(&0) {
            skipped.entry("binary".into()).or_default().push(rel_s);
            continue;
        }
        pending_bytes += bytes.len();
        pending.push((rel_s, bytes));
        if pending.len() >= BATCH_FILES || pending_bytes >= BATCH_BYTES {
            flush_batch(&*store, &o, &mut pending, &mut tally)?;
            pending_bytes = 0;
        }
    }
    flush_batch(&*store, &o, &mut pending, &mut tally)?;
    let Tally {
        files,
        unchanged,
        symbols,
        tokens,
        by_lang,
        seen,
        skipped: batch_skipped,
        failed,
    } = tally;
    for (r, v) in batch_skipped {
        skipped.entry(r).or_default().extend(v);
    }
    let mut pruned = Vec::new();
    if o.prune {
        if walk_errors {
            eprintln!("warning: --prune skipped because some paths could not be read");
        } else if !failed.is_empty() {
            eprintln!("warning: --prune skipped because some files failed to index");
        } else {
            if files == 0 && !o.force {
                let would = store.prune_files(o.org, o.repo, &seen, true)?;
                if !would.is_empty() {
                    bail!(
                        "--prune refused: no files were indexed but {}/{} has {} directory-indexed file(s) that would be removed; check `{}` or pass --force",
                        o.org,
                        o.repo,
                        would.len(),
                        o.dir.display()
                    );
                }
            }
            pruned = store.prune_files(o.org, o.repo, &seen, false)?;
        }
    }
    let ms = start.elapsed().as_millis();
    let skipped_n: usize = skipped.values().map(Vec::len).sum();
    if o.json {
        let summary = serde_json::json!({
            "org": o.org, "repo": o.repo, "files": files, "unchanged": unchanged, "symbols": symbols, "tokens": tokens,
            "languages": by_lang, "skipped": skipped_n, "skipped_by_reason": skipped,
            "pruned": pruned, "elapsed_ms": ms,
            "failed": failed.len(),
            "failed_files": failed.iter().map(|(p, r)| serde_json::json!({"path": p, "reason": r})).collect::<Vec<_>>(),
        });
        out!(out, "{}", serde_json::to_string(&summary)?);
    } else {
        out!(out,
            "indexed {}/{}: files={files} unchanged={unchanged} symbols={symbols} tokens={tokens} skipped={skipped_n} failed={} pruned={} elapsed={ms}ms",
            o.org, o.repo, failed.len(), pruned.len()
        );
        for (l, n) in &by_lang {
            out!(out, "  {l}: {n}");
        }
        if !pruned.is_empty() {
            out!(out, "  pruned:");
            for p in pruned.iter().take(20) {
                out!(out, "    {p}");
            }
            if pruned.len() > 20 {
                out!(
                    out,
                    "    ... and {} more (use --json for all)",
                    pruned.len() - 20
                );
            }
        }
        for (r, v) in &skipped {
            out!(out, "  skipped ({r}): {}", v.len());
            for p in v.iter().take(20) {
                out!(out, "    {p}");
            }
            if v.len() > 20 {
                out!(
                    out,
                    "    ... and {} more (use --json for all)",
                    v.len() - 20
                );
            }
        }
        for (p, r) in &failed {
            out!(out, "  failed: {p}: {r}");
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} file(s) failed to index (all other files were stored)",
            failed.len()
        );
    }
    Ok(())
}
