//! Directory indexing, shared by the `memory-graph` binary and its tests.
use anyhow::{bail, Context, Result};
use graph_store::{IndexOptions, PreparedFile, Store, ORIGIN_DIRECTORY};
use std::collections::BTreeMap;
use std::io::{Read, Write};

mod progress;
pub use progress::Progress;

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
    #[cfg(feature = "lang-javascript")]
    v.push(Box::new(graph_lang_javascript::JavaScriptExtractor));
    #[cfg(feature = "lang-aspx")]
    v.push(Box::new(graph_lang_aspx::AspxExtractor));
    #[cfg(feature = "lang-html")]
    v.push(Box::new(graph_lang_html::HtmlExtractor));
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
    /// Parsing threads; 0 means one per available CPU. The database is the
    /// same for any value: files are committed in walk order.
    pub jobs: usize,
    /// Live progress on stderr: `Some(true)` forces it, `Some(false)`
    /// disables it, `None` shows it when stderr is a terminal and `json` is off.
    pub progress: Option<bool>,
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
    pending: &mut Vec<(String, PreparedFile)>,
    t: &mut Tally,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let (rels, prepared): (Vec<String>, Vec<PreparedFile>) = pending.drain(..).unzip();
    let n = prepared.len();
    let outcomes = store
        .index_prepared(o.org, o.repo, prepared, IndexOptions { reindex: o.reindex })
        .with_context(|| {
            format!(
                "database error while indexing a batch of {n} files ({} files were already stored)",
                t.files
            )
        })?;
    for (rel, r) in rels.into_iter().zip(outcomes) {
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
                .push(rel),
            Err(graph_store::StoreError::TooLarge(_)) => {
                t.skipped.entry("too large".into()).or_default().push(rel)
            }
            Err(graph_store::StoreError::InvalidSpan(why)) => {
                // The store prefixes the path; the CLI names it separately.
                let reason = why
                    .strip_prefix('`')
                    .and_then(|w| w.split_once("`: "))
                    .map_or(why.as_str(), |(_, r)| r);
                let reason = format!("invalid span: {reason}");
                t.failed.push((rel, reason));
            }
            // Includes `Stale`, which cannot happen here: a run walks each
            // path once and holds the database lock.
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// One walk entry, kept in walk order.
enum Item {
    /// A regular file to read and prepare: its relative path and full path.
    File(String, std::path::PathBuf),
    /// Skipped during the walk; `unreadable` blocks `--prune`.
    Skip {
        reason: &'static str,
        what: String,
        unreadable: bool,
    },
}

/// What a worker made of one `Item`.
enum Outcome {
    Prepared(String, PreparedFile),
    Skip {
        reason: &'static str,
        what: String,
        unreadable: bool,
    },
}

/// Read one file (enforcing the size cap while reading, so a file that grows
/// after the walk cannot exhaust memory), reject binaries, then `prepare` it.
fn read_and_prepare(store: &dyn Store, o: &DirOpts, item: &Item) -> Result<Outcome> {
    let (rel, path) = match item {
        Item::File(rel, path) => (rel, path),
        Item::Skip {
            reason,
            what,
            unreadable,
        } => {
            return Ok(Outcome::Skip {
                reason,
                what: what.clone(),
                unreadable: *unreadable,
            })
        }
    };
    let skip = |reason, unreadable| {
        Ok(Outcome::Skip {
            reason,
            what: rel.clone(),
            unreadable,
        })
    };
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| {
        f.take(o.max_file_size.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    if read.is_err() {
        return skip("unreadable", true);
    }
    if bytes.len() as u64 > o.max_file_size {
        return skip("too large", false);
    }
    if bytes.contains(&0) {
        return skip("binary", false);
    }
    let file = graph_store::BatchFile {
        path: rel,
        bytes: &bytes,
        language: None,
        origin: Some(ORIGIN_DIRECTORY),
    };
    let p = store.prepare(o.org, o.repo, &file, IndexOptions { reindex: o.reindex })?;
    Ok(Outcome::Prepared(rel.clone(), p))
}

/// `read_and_prepare` with a panicking extractor turned into an error, so one
/// bad file cannot wedge the pipeline.
fn work(store: &dyn Store, o: &DirOpts, item: &Item) -> Result<Outcome> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_and_prepare(store, o, item)
    }))
    .unwrap_or_else(|_| {
        let what = match item {
            Item::File(rel, _) => rel.as_str(),
            Item::Skip { what, .. } => what.as_str(),
        };
        Err(anyhow::anyhow!("indexing `{what}` panicked"))
    })
}

/// The single writer: takes outcomes in walk order, groups them into batches
/// and commits each batch in one `index_prepared` call.
struct Writer<'a> {
    store: &'a dyn Store,
    o: &'a DirOpts<'a>,
    tally: Tally,
    skipped: BTreeMap<String, Vec<String>>,
    walk_errors: bool,
    pending: Vec<(String, PreparedFile)>,
    pending_bytes: usize,
    progress: &'a Progress,
}

impl Writer<'_> {
    fn take(&mut self, oc: Outcome) -> Result<()> {
        match oc {
            Outcome::Skip {
                reason,
                what,
                unreadable,
            } => {
                self.walk_errors |= unreadable;
                self.skipped.entry(reason.into()).or_default().push(what);
                self.progress.skipped(1);
            }
            Outcome::Prepared(rel, p) => {
                self.progress.read_bytes(p.bytes_len() as u64);
                self.pending_bytes += p.bytes_len();
                self.pending.push((rel, p));
                if self.pending.len() >= BATCH_FILES || self.pending_bytes >= BATCH_BYTES {
                    self.flush()?;
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let n = self.pending.len();
        let (skipped, failed) = (self.tally.skipped_n(), self.tally.failed.len());
        flush_batch(self.store, self.o, &mut self.pending, &mut self.tally)?;
        self.pending_bytes = 0;
        self.progress.committed(
            n,
            self.tally.skipped_n() - skipped,
            self.tally.failed.len() - failed,
        );
        Ok(())
    }
}

impl Tally {
    fn skipped_n(&self) -> usize {
        self.skipped.values().map(Vec::len).sum()
    }
}

/// Resolve `--jobs`: 0 means one per available CPU.
pub fn resolve_jobs(jobs: usize) -> usize {
    if jobs > 0 {
        jobs
    } else {
        std::thread::available_parallelism().map_or(1, usize::from)
    }
}

/// Prepare `items` on `jobs` threads and hand the outcomes to `w` in walk
/// order. Workers pull sequence numbers from a queue the writer refills as it
/// consumes, so at most `window` outcomes exist at once (memory stays
/// bounded however far a slow file holds up the reorder buffer).
fn run_pipeline(
    store: &dyn Store,
    o: &DirOpts,
    items: &[Item],
    jobs: usize,
    w: &mut Writer,
) -> Result<()> {
    let progress = w.progress;
    if jobs <= 1 {
        for item in items {
            progress.worker(0, item_name(item));
            let oc = work(store, o, item);
            progress.worker(0, None);
            w.take(oc?)?;
        }
        return Ok(());
    }
    let window = jobs * 4;
    let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<usize>(window);
    let job_rx = std::sync::Mutex::new(job_rx);
    let (res_tx, res_rx) = std::sync::mpsc::channel::<(usize, Result<Outcome>)>();
    std::thread::scope(|sc| {
        for k in 0..jobs {
            let (job_rx, res_tx) = (&job_rx, res_tx.clone());
            sc.spawn(move || loop {
                let next = job_rx.lock().map(|rx| rx.recv());
                let Ok(Ok(i)) = next else { break };
                progress.worker(k, item_name(&items[i]));
                let oc = work(store, o, &items[i]);
                progress.worker(k, None);
                if res_tx.send((i, oc)).is_err() {
                    break;
                }
            });
        }
        drop(res_tx);
        let mut sent = 0;
        while sent < items.len().min(window) {
            job_tx.send(sent).expect("workers outlive the queue");
            sent += 1;
        }
        let mut buf: BTreeMap<usize, Result<Outcome>> = BTreeMap::new();
        let mut next = 0;
        let result = (|| -> Result<()> {
            while next < items.len() {
                let Ok((i, oc)) = res_rx.recv() else {
                    bail!("indexing workers stopped unexpectedly");
                };
                buf.insert(i, oc);
                while let Some(oc) = buf.remove(&next) {
                    next += 1;
                    if sent < items.len() {
                        // Never blocks: at most `window` jobs are outstanding.
                        job_tx.send(sent).expect("workers outlive the queue");
                        sent += 1;
                    }
                    w.take(oc?)?;
                }
            }
            Ok(())
        })();
        // Closing the queue stops the workers (after their current file).
        drop(job_tx);
        result
    })
}

fn item_name(item: &Item) -> Option<&str> {
    match item {
        Item::File(rel, _) => Some(rel),
        Item::Skip { .. } => None,
    }
}

/// Index every text file under `dir`. Paths are stored relative to `dir`.
/// Per-file problems are counted as skips; only database failures abort.
///
/// The directory is walked first (paths and sizes only), then files are read
/// and prepared (parsed) on `jobs` threads while this thread commits them in
/// walk order, in the same batches whatever `jobs` is.
pub fn index_dir(
    o: DirOpts,
    open: impl FnOnce(&std::path::Path) -> Result<Box<dyn Store>>,
    out: &mut dyn Write,
) -> Result<()> {
    use std::io::IsTerminal;
    let show = o
        .progress
        .unwrap_or_else(|| !o.json && std::io::stderr().is_terminal());
    let progress = if show {
        Progress::stderr(&format!("{}/{}", o.org, o.repo))
    } else {
        Progress::hidden()
    };
    let r = index_dir_with(o, open, out, &progress);
    progress.finish();
    r
}

/// `index_dir` reporting to a caller-supplied `progress`.
pub fn index_dir_with(
    o: DirOpts,
    open: impl FnOnce(&std::path::Path) -> Result<Box<dyn Store>>,
    out: &mut dyn Write,
    progress: &Progress,
) -> Result<()> {
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
    let mut items: Vec<Item> = Vec::new();
    let mut total_bytes = 0u64;
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
                items.push(Item::Skip {
                    reason: "unreadable",
                    what: err.to_string(),
                    unreadable: true,
                });
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        let rel = entry.path().strip_prefix(o.dir).unwrap_or(entry.path());
        let skip = |reason| Item::Skip {
            reason,
            what: rel.to_string_lossy().into_owned(),
            unreadable: false,
        };
        let Some(ft) = ft else { continue };
        if ft.is_symlink() {
            items.push(skip("symlink"));
            continue;
        }
        if ft.is_dir() {
            continue;
        }
        if !ft.is_file() {
            items.push(skip("not a regular file"));
            continue;
        }
        let Some(rel_s) = rel.to_str().map(str::to_owned) else {
            items.push(skip("non-UTF-8 path"));
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            items.push(Item::Skip {
                reason: "unreadable",
                what: rel_s,
                unreadable: true,
            });
            continue;
        };
        if is_db_file(
            &meta,
            entry.path(),
            db_meta.as_ref(),
            db_canon.as_deref(),
            db_name,
        ) {
            items.push(Item::Skip {
                reason: "database file",
                what: rel_s,
                unreadable: false,
            });
            continue;
        }
        total_bytes += meta.len().min(o.max_file_size);
        items.push(Item::File(rel_s, entry.into_path()));
        progress.walked(items.len());
    }
    progress.start(items.len(), total_bytes);
    let mut w = Writer {
        store: &*store,
        o: &o,
        tally: Tally::default(),
        skipped: BTreeMap::new(),
        walk_errors: false,
        pending: Vec::new(),
        pending_bytes: 0,
        progress,
    };
    let jobs = resolve_jobs(o.jobs);
    progress.set_workers(jobs);
    run_pipeline(&*store, &o, &items, jobs, &mut w)?;
    w.flush()?;
    let Writer {
        tally,
        mut skipped,
        walk_errors,
        ..
    } = w;
    drop(items);
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
