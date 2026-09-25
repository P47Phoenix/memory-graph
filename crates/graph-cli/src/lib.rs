//! Directory indexing, shared by the `memory-graph` binary and its tests.
use anyhow::{bail, Context, Result};
use graph_store::{IndexOptions, PreparedFile, Store, ORIGIN_DIRECTORY};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub mod dataflow;
pub mod diskinfo;
use diskinfo::{DiskInputs, DiskPolicy, DiskProbe, MinFree};
pub mod progress;
pub mod report;
pub mod sysinfo;
use dataflow::Trace;
use progress::{Board, Display};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

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
    /// Parsing threads; 0 sizes from the CPUs. Files are committed in walk
    /// order whatever the value, so the stored content is the same.
    pub jobs: usize,
    /// Cap on source bytes in flight (read, not yet committed): a fixed size,
    /// or a share of the free memory followed during the run; `None` means
    /// the default share (see `sysinfo::DEFAULT_FRACTION`).
    pub memory: Option<sysinfo::MemorySpec>,
    /// Commit the fixed batches of earlier releases (256 files / 32 MiB)
    /// instead of everything ready, so the database file is byte-for-byte
    /// the same on any machine.
    pub deterministic: bool,
    /// Print a per-stage time table on stderr at the end (and add `stats` to
    /// the `json` summary).
    pub stats: bool,
    /// Write a Chrome / Perfetto trace of the run here.
    pub trace: Option<&'a std::path::Path>,
    /// Live progress on stderr: `Some(true)` shows it even with `json`,
    /// `Some(false)` disables it, `None` shows it when `json` is off. It is
    /// never drawn when stderr is not a terminal.
    pub progress: Option<bool>,
    /// Where disk readings come from; `None` asks the OS (tests script one).
    pub disk_probe: Option<DiskProbe>,
    /// Free space to keep on the database's volume (`--min-free-disk`).
    pub min_free_disk: MinFree,
    /// Stop before the disk fills (`false` = `--no-disk-check`: report only).
    pub disk_check: bool,
    /// Source bytes per store transaction (`--chunk-bytes`): one group commit
    /// is capped at a few of these so a failure loses little.
    pub chunk_bytes: u64,
}

/// Source bytes per redb transaction unless `--chunk-bytes` says otherwise
/// (the store's own default).
pub const DEFAULT_CHUNK_BYTES: u64 = 64 << 20;

/// Source bytes one group commit may take: half the memory budget (as it is
/// right now) so parsing can refill the other half meanwhile, and at most a
/// few store transactions so a failure loses little.
fn group_cap(board: &Board, o: &DirOpts) -> u64 {
    (board.budget.cap() / 2)
        .min(o.chunk_bytes.saturating_mul(8))
        .min(board.disk_group_cap.load(Relaxed))
        .max(1)
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
/// Returns the source bytes of the files the store already had (unchanged).
fn flush_batch(
    store: &dyn Store,
    o: &DirOpts,
    pending: &mut Vec<(String, PreparedFile)>,
    t: &mut Tally,
) -> Result<u64> {
    if pending.is_empty() {
        return Ok(0);
    }
    let lens: Vec<u64> = pending.iter().map(|(_, p)| p.bytes_len() as u64).collect();
    let (rels, prepared): (Vec<String>, Vec<PreparedFile>) = pending.drain(..).unzip();
    let n = prepared.len();
    let outcomes = store
        .index_prepared(o.org, o.repo, prepared, IndexOptions { reindex: o.reindex })
        .map_err(|e| {
            if diskinfo::is_disk_full(&e.to_string()) {
                anyhow::anyhow!(
                    "disk full while indexing a batch of {n} files ({} files were already stored): {e}; free space and rerun to resume (stored files are skipped)",
                    t.files
                )
            } else {
                anyhow::Error::from(e).context(format!(
                    "database error while indexing a batch of {n} files ({} files were already stored)",
                    t.files
                ))
            }
        })?;
    let mut unchanged_bytes = 0u64;
    for ((rel, len), r) in rels.into_iter().zip(lens).zip(outcomes) {
        match r {
            Ok(st) => {
                t.files += 1;
                t.unchanged += usize::from(st.unchanged);
                if st.unchanged {
                    unchanged_bytes += len;
                }
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
            Err(e) => return Err(e.into()),
        }
    }
    Ok(unchanged_bytes)
}

/// One walk entry, kept in walk order.
enum Item {
    /// A regular file to read and prepare: relative path, full path, and the
    /// bytes it will take (its size, capped at `max_file_size`).
    File(String, std::path::PathBuf, u64),
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
/// after the walk cannot exhaust memory), reject binaries, then `prepare` it,
/// reporting each step as parse thread `k`.
fn read_and_prepare(
    store: &dyn Store,
    o: &DirOpts,
    item: &Item,
    board: &Board,
    k: usize,
) -> Result<Outcome> {
    let (rel, path) = match item {
        Item::File(rel, path, _) => (rel, path),
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
    let read = board.parse.busy(k, "reading", rel, || {
        std::fs::File::open(path).and_then(|f| {
            f.take(o.max_file_size.saturating_add(1))
                .read_to_end(&mut bytes)
        })
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
    let p = board
        .parse
        .busy(k, "parsing", rel, || {
            store.prepare(o.org, o.repo, &file, IndexOptions { reindex: o.reindex })
        })
        .with_context(|| format!("database error while preparing `{rel}`"))?;
    Ok(Outcome::Prepared(rel.clone(), p))
}

/// `read_and_prepare` with any panic turned into an error, so one bad file
/// cannot wedge the pipeline.
fn work(store: &dyn Store, o: &DirOpts, item: &Item, board: &Board, k: usize) -> Result<Outcome> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_and_prepare(store, o, item, board, k)
    }))
    .unwrap_or_else(|_| {
        let what = match item {
            Item::File(rel, _, _) => rel.as_str(),
            Item::Skip { what, .. } => what.as_str(),
        };
        Err(anyhow::anyhow!("indexing `{what}` panicked"))
    })
}

impl Tally {
    fn skipped_n(&self) -> usize {
        self.skipped.values().map(Vec::len).sum()
    }
}

/// What the committing stage collected.
#[derive(Default)]
struct Collected {
    tally: Tally,
    skipped: BTreeMap<String, Vec<String>>,
    walk_errors: bool,
}

/// Walk `o.dir` in path order and stream every entry, numbered, to `tx`.
fn walk(
    o: &DirOpts,
    board: &Board,
    tx: crossbeam_channel::Sender<(u64, Item)>,
    cancel: &AtomicBool,
) {
    let db_meta = std::fs::metadata(o.db).ok();
    let db_canon = o.db.canonicalize().ok();
    let db_name = o.db.file_name();
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
    let dir = o.dir.display().to_string();
    board.walk.busy(0, "walking", &dir, || {
        let mut seq = 0u64;
        let mut send = |item: Item| {
            let size = match &item {
                Item::File(_, _, n) => *n,
                Item::Skip { .. } => 0,
            };
            board.found_bytes.fetch_add(size, Relaxed);
            board.found.fetch_add(1, Relaxed);
            board.walk.count(1, size);
            let ok = tx.send((seq, item)).is_ok();
            seq += 1;
            ok
        };
        for entry in walker {
            if cancel.load(Relaxed) {
                return;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    send(Item::Skip {
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
            let item = if ft.is_symlink() {
                skip("symlink")
            } else if ft.is_dir() {
                continue;
            } else if !ft.is_file() {
                skip("not a regular file")
            } else if let Some(rel_s) = rel.to_str().map(str::to_owned) {
                match entry.metadata() {
                    Err(_) => Item::Skip {
                        reason: "unreadable",
                        what: rel_s,
                        unreadable: true,
                    },
                    Ok(meta)
                        if is_db_file(
                            &meta,
                            entry.path(),
                            db_meta.as_ref(),
                            db_canon.as_deref(),
                            db_name,
                        ) =>
                    {
                        Item::Skip {
                            reason: "database file",
                            what: rel_s,
                            unreadable: false,
                        }
                    }
                    Ok(meta) => {
                        let size = meta.len().min(o.max_file_size);
                        Item::File(rel_s, entry.into_path(), size)
                    }
                }
            } else {
                skip("non-UTF-8 path")
            };
            if !send(item) {
                return;
            }
        }
    });
    board
        .walk_done
        .store(true, std::sync::atomic::Ordering::Release);
    board.walk.done(0);
}

/// Streams the directory through walk → admit (memory budget) → parse ×N →
/// commit (one writer, walk order). Stages never wait on each other except
/// through the byte budget: parsed files pile up in memory, up to the
/// budget, while the writer commits, and the writer commits everything that
/// is ready (in walk order) in one transaction.
fn run_pipeline(
    store: &dyn Store,
    o: &DirOpts,
    board: &Board,
    display: &mut Display,
) -> Result<Collected> {
    let cancel = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let (walk_tx, walk_rx) = crossbeam_channel::unbounded::<(u64, Item)>();
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<(u64, Item, u64)>();
    // (walk sequence, outcome, source bytes, heap footprint)
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<(u64, Result<Outcome>, u64, u64)>();
    let inflight = std::sync::Mutex::new(BTreeMap::<u64, String>::new());
    let admit_blocked = AtomicBool::new(false);
    std::thread::scope(|sc| {
        let (cancel, board, inflight) = (&cancel, board, &inflight);
        sc.spawn(move || walk(o, board, walk_tx, cancel));
        // Admission: hold each file's bytes against the budget, in walk
        // order, before it may be read (so the writer can always progress).
        let admit_blocked = &admit_blocked;
        sc.spawn(move || {
            for (seq, item) in walk_rx {
                let size = match &item {
                    Item::File(_, _, n) => *n,
                    Item::Skip { .. } => 0,
                };
                admit_blocked.store(true, Relaxed);
                let ok = board.budget.acquire(size, cancel);
                admit_blocked.store(false, Relaxed);
                if !ok || job_tx.send((seq, item, size)).is_err() {
                    return;
                }
            }
        });
        for k in 0..board.sizing.parse_threads {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            sc.spawn(move || {
                loop {
                    let why = if admit_blocked.load(Relaxed) {
                        "memory"
                    } else {
                        "input"
                    };
                    let next = if why == "memory" {
                        board.parse.blocked(k, "memory budget", || job_rx.recv())
                    } else {
                        board.parse.starved(k, "files to parse", || job_rx.recv())
                    };
                    let Ok((seq, item, size)) = next else { break };
                    if cancel.load(Relaxed) {
                        break;
                    }
                    if let Item::File(rel, _, _) = &item {
                        inflight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(seq, rel.clone());
                    }
                    let oc = work(store, o, &item, board, k);
                    let mut fp = 0;
                    if let Ok(Outcome::Prepared(_, p)) = &oc {
                        board.parse.count(1, size);
                        fp = p.memory_footprint() as u64;
                        board.footprint.fetch_add(fp, Relaxed);
                        board.prepared_bytes.fetch_add(size, Relaxed);
                    }
                    inflight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&seq);
                    if res_tx.send((seq, oc, size, fp)).is_err() {
                        break;
                    }
                }
                board.parse.done(k);
            });
        }
        drop((job_rx, res_tx));
        let finished = &finished;
        // Sampler + display: every 250 ms re-read the machine's memory and
        // move the budget (unless `--memory` fixed it) and re-read the disk;
        // redraw every 125 ms.
        sc.spawn(move || {
            let mut policy = board.sizing.policy.clone();
            let dynamic = matches!(policy.spec, sysinfo::MemorySpec::Fraction(_));
            let disk_policy = DiskPolicy {
                min_free: o.min_free_disk,
                enforce: o.disk_check,
            };
            let probe: DiskProbe = o
                .disk_probe
                .clone()
                .unwrap_or_else(|| std::sync::Arc::new(diskinfo::sample_disk));
            let mut tick = 0u32;
            while !finished.load(Relaxed) {
                if tick.is_multiple_of(2) {
                    let sample = probe(o.db);
                    let db_len = std::fs::metadata(o.db).map_or(0, |m| m.len());
                    // Size the next group to what fits on the volume (at the
                    // ratio measured so far), unless batches are fixed.
                    if let Some(s) = &sample {
                        if o.disk_check && !o.deterministic {
                            let min_free = o.min_free_disk.resolve(Some(s.total));
                            let ratio = f64::from_bits(board.disk_ratio.load(Relaxed));
                            board
                                .disk_group_cap
                                .store(diskinfo::group_fit(s.available, min_free, ratio), Relaxed);
                        }
                    }
                    // `walk_done` first: it publishes the final `found_bytes`.
                    let walk_done = board.walk_done.load(std::sync::atomic::Ordering::Acquire);
                    let d = disk_policy.decide(
                        sample.as_ref(),
                        DiskInputs {
                            db_len,
                            db_len_start: board.db_len_start.load(Relaxed),
                            found_bytes: board.found_bytes.load(Relaxed),
                            handled_bytes: board.handled_bytes.load(Relaxed),
                            unchanged_bytes: board.unchanged_bytes.load(Relaxed),
                            walk_done,
                            group_bytes: if o.deterministic {
                                BATCH_BYTES as u64 + o.max_file_size
                            } else {
                                group_cap(board, o)
                            },
                        },
                    );
                    *board.disk.lock().unwrap_or_else(|e| e.into_inner()) = sample;
                    board.db_len.store(db_len, Relaxed);
                    board.disk_projected.store(d.projected_final, Relaxed);
                    board.disk_ratio.store(d.ratio.to_bits(), Relaxed);
                    board.disk_min_free.store(d.min_free, Relaxed);
                    if let Some(why) = d.stop {
                        let mut stop = board.disk_stop.lock().unwrap_or_else(|e| e.into_inner());
                        if stop.is_none() {
                            *stop = Some(why);
                            // Stop admitting and parsing; the writer commits
                            // what is pending and reports.
                            cancel.store(true, Relaxed);
                            board.budget.wake_all();
                        }
                    }
                }
                if dynamic && tick.is_multiple_of(2) {
                    // A probe that fails mid-run keeps the last good sample
                    // (the budget does not fall back to 512M because one
                    // read of /proc raced a cgroup change) and records why.
                    let probed = sysinfo::sample_memory();
                    let m = {
                        let mut last = board.memory.lock().unwrap_or_else(|e| e.into_inner());
                        let mut err = board.memory_error.lock().unwrap_or_else(|e| e.into_inner());
                        match probed {
                            Ok(m) => {
                                *last = Some(m);
                                *err = None;
                            }
                            Err(e) => *err = Some(e),
                        }
                        *last
                    };
                    let held = board.budget.used();
                    let prepared = board.prepared_bytes.load(Relaxed);
                    let fp = board.footprint.load(Relaxed);
                    if let Some(d) = policy.update_measured(m.as_ref(), held, prepared, fp) {
                        board.budget.set_cap(d.cap, &d.reason, d.under_pressure);
                    }
                    board.expansion.store(policy.expansion.to_bits(), Relaxed);
                }
                tick = tick.wrapping_add(1);
                if !display.is_hidden() {
                    display.draw(&board.view());
                }
                std::thread::sleep(std::time::Duration::from_millis(125));
            }
            display.finish();
        });
        let r = commit_all(store, o, board, res_rx, inflight);
        // Stop every stage (after its current file) and wake budget waiters.
        cancel.store(true, Relaxed);
        board.budget.wake_all();
        finished.store(true, Relaxed);
        r
    })
}

/// The single writer: takes outcomes in walk order and commits them. By
/// default each transaction takes everything that is ready (group commit),
/// so a slow disk gets bigger transactions and a fast one smaller, with no
/// fixed batch size; `deterministic` keeps the fixed batches instead.
fn commit_all(
    store: &dyn Store,
    o: &DirOpts,
    board: &Board,
    res_rx: crossbeam_channel::Receiver<(u64, Result<Outcome>, u64, u64)>,
    inflight: &std::sync::Mutex<BTreeMap<u64, String>>,
) -> Result<Collected> {
    let mut c = Collected::default();
    let mut buf: BTreeMap<u64, (Result<Outcome>, u64, u64)> = BTreeMap::new();
    let mut next = 0u64;
    let mut pending: Vec<(String, PreparedFile)> = Vec::new();
    let (mut pending_bytes, mut held, mut held_fp, mut held_prepared) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let group_cap = group_cap(board, o);
        // Take everything that has arrived.
        for (seq, oc, size, fp) in res_rx.try_iter() {
            buf.insert(seq, (oc, size, fp));
        }
        // The disk guard asked to stop: store what is ready and report.
        let stop = board
            .disk_stop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(why) = stop {
            // What was committed stays. A partial fixed batch (only
            // `--deterministic` leaves one pending here; the adaptive mode
            // flushes every pass) is stored too, bounded by the group cap so
            // it cannot fill the disk by itself. Everything else parsed so
            // far is abandoned and re-parsed on the rerun; the budget and
            // footprint those files hold die with the scope.
            if !pending.is_empty() {
                let n = pending.len();
                let unchanged =
                    board
                        .commit
                        .busy(0, "committing", &format!("last {n} files"), || {
                            flush_batch(store, o, &mut pending, &mut c.tally)
                        })?;
                board.unchanged_bytes.fetch_add(unchanged, Relaxed);
                board.txns.fetch_add(1, Relaxed);
            }
            board.budget.release(held);
            board.footprint.fetch_sub(held_fp, Relaxed);
            board.prepared_bytes.fetch_sub(held_prepared, Relaxed);
            board.commit.done(0);
            bail!(
                "stopped before the disk filled: {why}; {} files stored in {} transactions; free space and rerun to resume (stored files are skipped)",
                c.tally.files,
                board.txns.load(Relaxed)
            );
        }
        while let Some((oc, size, fp)) = buf.remove(&next) {
            next += 1;
            held += size;
            held_fp += fp;
            match oc? {
                Outcome::Skip {
                    reason,
                    what,
                    unreadable,
                } => {
                    c.walk_errors |= unreadable;
                    c.skipped.entry(reason.into()).or_default().push(what);
                    board.skipped.fetch_add(1, Relaxed);
                    board.handled.fetch_add(1, Relaxed);
                }
                Outcome::Prepared(rel, p) => {
                    held_prepared += size;
                    pending_bytes += p.bytes_len() as u64;
                    pending.push((rel, p));
                }
            }
            let full = if o.deterministic {
                pending.len() >= BATCH_FILES || pending_bytes >= BATCH_BYTES as u64
            } else {
                pending_bytes >= group_cap
            };
            if full {
                break;
            }
        }
        let all_in = board.walk_done.load(std::sync::atomic::Ordering::Acquire)
            && next == board.found.load(Relaxed);
        let batch_full = if o.deterministic {
            pending.len() >= BATCH_FILES || pending_bytes >= BATCH_BYTES as u64
        } else {
            !pending.is_empty()
        };
        if batch_full || (all_in && !pending.is_empty()) {
            let n = pending.len();
            let (skipped0, failed0) = (c.tally.skipped_n(), c.tally.failed.len());
            let txn = board.txns.load(Relaxed) + 1;
            let label = format!(
                "txn {txn}: {n} files ({:.1} MB)",
                pending_bytes as f64 / 1048576.0
            );
            let t0 = std::time::Instant::now();
            let unchanged = board.commit.busy(0, "committing", &label, || {
                flush_batch(store, o, &mut pending, &mut c.tally)
            })?;
            board.unchanged_bytes.fetch_add(unchanged, Relaxed);
            board.txns.store(txn, Relaxed);
            board
                .last_txn
                .store(t0.elapsed().as_millis() as u64, Relaxed);
            let (sk, fa) = (
                c.tally.skipped_n() - skipped0,
                c.tally.failed.len() - failed0,
            );
            board.commit.count((n - sk - fa) as u64, pending_bytes);
            board.skipped.fetch_add(sk as u64, Relaxed);
            board.failed.fetch_add(fa as u64, Relaxed);
            board.handled.fetch_add(n as u64, Relaxed);
            pending_bytes = 0;
        }
        if pending.is_empty() {
            board.handled_bytes.fetch_add(held, Relaxed);
            board.budget.release(held);
            board.footprint.fetch_sub(held_fp, Relaxed);
            board.prepared_bytes.fetch_sub(held_prepared, Relaxed);
            (held, held_fp, held_prepared) = (0, 0, 0);
        }
        if all_in && pending.is_empty() && buf.is_empty() {
            // A stop that landed during the last commit lost nothing, but
            // the run must not report success with a stop recorded.
            if let Some(why) = board
                .disk_stop
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                board.commit.done(0);
                bail!(
                    "stopped before the disk filled: {why}; {} files stored in {} transactions; free space and rerun to resume (stored files are skipped)",
                    c.tally.files,
                    board.txns.load(Relaxed)
                );
            }
            board.commit.done(0);
            return Ok(c);
        }
        if buf.contains_key(&next) {
            continue;
        }
        // Wait for the next file in walk order.
        let on = inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&next)
            .cloned()
            .unwrap_or_else(|| {
                if board.walk_done.load(std::sync::atomic::Ordering::Acquire) {
                    "parsed files".into()
                } else {
                    "the walk".into()
                }
            });
        match board.commit.starved(0, on, || {
            res_rx.recv_timeout(std::time::Duration::from_millis(50))
        }) {
            Ok((seq, oc, size, fp)) => {
                buf.insert(seq, (oc, size, fp));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if !(board.walk_done.load(std::sync::atomic::Ordering::Acquire)
                    && next == board.found.load(Relaxed))
                {
                    bail!("indexing workers stopped unexpectedly");
                }
            }
        }
    }
}

/// Index every text file under `dir`. Paths are stored relative to `dir`.
/// Per-file problems are counted as skips; only database failures abort.
///
/// The directory streams through walk → parse (one thread per spare CPU) →
/// commit (one writer, walk order), bounded by a memory budget sized from
/// the machine; a live view on stderr shows what every stage is doing.
pub fn index_dir(
    o: DirOpts,
    open: impl FnOnce(&std::path::Path) -> Result<Box<dyn Store>>,
    out: &mut dyn Write,
) -> Result<()> {
    use std::io::IsTerminal;
    let show = o
        .progress
        .unwrap_or_else(|| !o.json && std::io::stderr().is_terminal());
    let mut display = if show {
        Display::stderr()
    } else {
        Display::hidden()
    };
    index_dir_with(o, open, out, &mut display)
}

/// `index_dir` drawing on a caller-supplied `display`.
pub fn index_dir_with(
    o: DirOpts,
    open: impl FnOnce(&std::path::Path) -> Result<Box<dyn Store>>,
    out: &mut dyn Write,
    display: &mut Display,
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
    if o.deterministic && o.chunk_bytes < BATCH_BYTES as u64 + o.max_file_size {
        bail!(
            "--deterministic needs --chunk-bytes of at least {} (one fixed batch plus one --max-file-size file), got {}; raise --chunk-bytes or lower --max-file-size",
            BATCH_BYTES as u64 + o.max_file_size,
            o.chunk_bytes
        );
    }
    // Pre-flight: refuse when the volume is already below the reserve.
    let probe: DiskProbe = o
        .disk_probe
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(diskinfo::sample_disk));
    if let Some(s) = probe(o.db) {
        let min_free = o.min_free_disk.resolve(Some(s.total));
        if o.disk_check && s.available < min_free {
            bail!(
                "refusing to index: only {} free on the database's volume (keeping {}); free space, lower --min-free-disk, or pass --no-disk-check",
                diskinfo::mb(s.available),
                diskinfo::mb(min_free)
            );
        }
    }
    let start = std::time::Instant::now();
    let store = open(o.db)?;
    let trace: Option<&'static Trace> = o.trace.map(|_| &*Box::leak(Box::new(Trace::new())));
    // A fixed batch holds its files' bytes until it commits, so in
    // deterministic mode the budget must always fit one whole batch plus the
    // file that closes it.
    let floor = if o.deterministic {
        BATCH_BYTES as u64 + o.max_file_size
    } else {
        1
    };
    let sizing = sysinfo::Sizing::detect(o.jobs, o.memory, floor);
    let board = Board::new(&format!("{}/{}", o.org, o.repo), sizing, trace);
    board.disk_check.store(o.disk_check, Relaxed);
    board
        .db_len_start
        .store(std::fs::metadata(o.db).map_or(0, |m| m.len()), Relaxed);
    let run = run_pipeline(&*store, &o, &board, display);
    let view = board.view();
    if let (Some(path), Some(t)) = (o.trace, trace) {
        t.write(path)
            .with_context(|| format!("cannot write trace `{}`", path.display()))?;
    }
    if o.stats && !o.json {
        eprint!("{}", view.stats_table());
    }
    let Collected {
        tally,
        mut skipped,
        walk_errors,
    } = run?;
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
        let mut summary = summary;
        if o.stats {
            summary["stats"] = view.stats_json();
        }
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
