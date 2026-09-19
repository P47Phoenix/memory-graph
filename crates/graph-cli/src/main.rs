use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use graph_core::TokenClass;
use graph_store::{Grain, Query, Store, SymbolQuery, ORIGIN_DIRECTORY};
use std::io::{Read, Write};
use std::path::PathBuf;

/// Write a line to stdout, propagating errors (a closed pipe is handled in `main`).
macro_rules! out {
    ($($a:tt)*) => {
        writeln!(std::io::stdout().lock(), $($a)*)?
    };
}

#[derive(Parser)]
#[command(name = "memory-graph", about = "Language-agnostic code memory graph")]
struct Cli {
    /// Database file
    #[arg(long, global = true, default_value = "./graph.redb")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index one file under an org and repo (re-indexing replaces it)
    IndexFile {
        #[arg(long)]
        org: String,
        #[arg(long)]
        repo: String,
        /// Language override (default: detected from extension)
        #[arg(long)]
        language: Option<String>,
        path: PathBuf,
    },
    /// Index a directory as a repo (honors .gitignore; skips binary files)
    Index {
        #[arg(long)]
        org: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        json: bool,
        /// Skip files larger than this many bytes (lockfiles, minified bundles, dumps)
        #[arg(long, default_value_t = 8 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_size: u64,
        /// Remove files of this repo that were not indexed in this run (deleted, renamed, newly ignored or
        /// skipped). Only files last indexed by a directory run are considered: `index-file` clears that
        /// mark, and a later directory run sets it again. Skipped when some paths were unreadable; refused
        /// when nothing was indexed and files would be removed, unless --force
        #[arg(long)]
        prune: bool,
        /// With --prune: allow removing files even when this run indexed nothing
        #[arg(long, requires = "prune")]
        force: bool,
        dir: PathBuf,
    },
    /// Show what is indexed: per repo, the languages present and each language's symbol kinds
    Describe {
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Find symbols (definitions) by name; a trailing `*` matches a prefix
    Symbols {
        /// Exact name, `prefix*` (a prefix), `*` (everything) or `name\*` (a name ending in a literal `*`).
        /// Edges: `**` at the end is rejected as ambiguous (so `a\**` is too), and a trailing backslash not
        /// followed by `*` is an ordinary character (`a\` matches the name `a\` exactly)
        pattern: String,
        /// Only symbols of this symbol kind (case-insensitive): generic (module, type, function, method,
        /// variable, constant, other) or language-specific (struct, trait, impl, ...). `describe` lists the
        /// kinds present. (`search --kind` is a token class, a different thing)
        #[arg(long)]
        kind: Option<String>,
        /// Only files of this language (case-insensitive, e.g. rust); `describe` lists the languages present
        #[arg(long)]
        language: Option<String>,
        /// Only this organisation (exact name)
        #[arg(long)]
        org: Option<String>,
        /// Only this repo (exact name)
        #[arg(long)]
        repo: Option<String>,
        /// Restrict to one file path as indexed (relative to the indexed directory)
        #[arg(long)]
        file: Option<String>,
        /// Show at most this many results (ordered by org, repo, file, position)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Find tokens by exact text
    Search {
        /// Exact token text
        text: String,
        /// Only files of this language (case-insensitive); `describe` lists the languages present
        #[arg(long)]
        language: Option<String>,
        /// Only this organisation (exact name)
        #[arg(long)]
        org: Option<String>,
        /// Only this repo (exact name)
        #[arg(long)]
        repo: Option<String>,
        /// Token class (NOT a symbol kind; see --symbol-kind): identifier, keyword, literal, operator, punctuation, comment, other
        #[arg(long)]
        kind: Option<TokenClass>,
        /// Level results are rolled up to: token, symbol, file, repo or org
        #[arg(long, default_value = "token")]
        grain: Grain,
        /// With --grain symbol: only symbols of this symbol kind (case-insensitive; generic or
        /// language-specific; see `describe`)
        #[arg(long)]
        symbol_kind: Option<String>,
        /// Show at most this many rows (ordered by org, repo, file, position)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

struct DirOpts<'a> {
    db: &'a std::path::Path,
    org: &'a str,
    repo: &'a str,
    dir: &'a std::path::Path,
    json: bool,
    max_file_size: u64,
    prune: bool,
    force: bool,
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
    symbols: usize,
    tokens: usize,
    by_lang: std::collections::BTreeMap<String, usize>,
    seen: std::collections::HashSet<String>,
    skipped: std::collections::BTreeMap<String, Vec<String>>,
}

/// Store the pending files in one transaction and fold the outcomes into `t`.
fn flush_batch(
    store: &Store,
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
    let outcomes = store.index_batch(o.org, o.repo, &inputs).with_context(|| {
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
            Err(e) => return Err(e.into()),
        }
    }
    pending.clear();
    Ok(())
}

/// Index every text file under `dir`. Paths are stored relative to `dir`.
/// Per-file problems are counted as skips; only database failures abort.
fn index_dir(o: DirOpts) -> Result<()> {
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
    let mut store = Store::open(o.db)?;
    store.register(Box::new(graph_lang_rust::RustExtractor));
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
            flush_batch(&store, &o, &mut pending, &mut tally)?;
            pending_bytes = 0;
        }
    }
    flush_batch(&store, &o, &mut pending, &mut tally)?;
    let Tally {
        files,
        symbols,
        tokens,
        by_lang,
        seen,
        skipped: batch_skipped,
    } = tally;
    for (r, v) in batch_skipped {
        skipped.entry(r).or_default().extend(v);
    }
    let mut pruned = Vec::new();
    if o.prune {
        if walk_errors {
            eprintln!("warning: --prune skipped because some paths could not be read");
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
        let out = serde_json::json!({
            "org": o.org, "repo": o.repo, "files": files, "symbols": symbols, "tokens": tokens,
            "languages": by_lang, "skipped": skipped_n, "skipped_by_reason": skipped,
            "pruned": pruned, "elapsed_ms": ms,
        });
        out!("{}", serde_json::to_string(&out)?);
    } else {
        out!(
            "indexed {}/{}: files={files} symbols={symbols} tokens={tokens} skipped={skipped_n} pruned={} elapsed={ms}ms",
            o.org, o.repo, pruned.len()
        );
        for (l, n) in &by_lang {
            out!("  {l}: {n}");
        }
        if !pruned.is_empty() {
            out!("  pruned:");
            for p in pruned.iter().take(20) {
                out!("    {p}");
            }
            if pruned.len() > 20 {
                out!(
                    "    ... and {} more (use --json for all)",
                    pruned.len() - 20
                );
            }
        }
        for (r, v) in &skipped {
            out!("  skipped ({r}): {}", v.len());
            for p in v.iter().take(20) {
                out!("    {p}");
            }
            if v.len() > 20 {
                out!("    ... and {} more (use --json for all)", v.len() - 20);
            }
        }
    }
    Ok(())
}

fn open_existing(db: &std::path::Path) -> Result<Store> {
    if !db.is_file() {
        bail!("database `{}` does not exist", db.display());
    }
    Store::open(db).with_context(|| format!("opening database `{}`", db.display()))
}

/// Filters are checked against what is actually indexed (in the org/repo
/// scope), so a typo fails loudly and lists the valid choices rather than
/// returning nothing.
fn validate_filters(
    store: &Store,
    org: Option<&str>,
    repo: Option<&str>,
    language: Option<&str>,
    kind: Option<&str>,
) -> Result<()> {
    for (name, v) in [
        ("--org", org),
        ("--repo", repo),
        ("--language", language),
        ("--kind/--symbol-kind", kind),
    ] {
        if v == Some("") {
            bail!("{name} must not be empty");
        }
    }
    let infos = store.describe(org, repo)?;
    if infos.is_empty() {
        if org.is_some() || repo.is_some() {
            bail!("no indexed repo matches --org/--repo; run `describe` to list what is indexed");
        }
        return Ok(());
    }
    if let Some(l) = language {
        let langs: std::collections::BTreeSet<&String> =
            infos.iter().flat_map(|i| i.languages.keys()).collect();
        if !langs.iter().any(|x| x.eq_ignore_ascii_case(l)) {
            bail!(
                "no files of language `{l}` in scope; languages present: {}",
                join(langs.iter().copied())
            );
        }
    }
    if let Some(k) = kind {
        let kinds: std::collections::BTreeSet<String> =
            infos.iter().flat_map(|i| i.kind_names(language)).collect();
        // Generic kinds are always valid (`other` covers symbols without a kind).
        let known = kinds.iter().any(|x| x.eq_ignore_ascii_case(k));
        if !known
            && k.to_ascii_lowercase()
                .parse::<graph_core::SymbolKind>()
                .is_err()
        {
            bail!(
                "no symbols of kind `{k}` in scope; kinds present: {}",
                join(kinds.iter())
            );
        }
    }
    Ok(())
}

fn join<'a>(it: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = it.map(String::as_str).collect();
    if v.is_empty() {
        "(none)".into()
    } else {
        v.join(", ")
    }
}

fn main() {
    if let Err(e) = run() {
        // `symbols | head` closes the pipe early: that is not an error.
        let broken = e
            .chain()
            .filter_map(|c| c.downcast_ref::<std::io::Error>())
            .any(|io| io.kind() == std::io::ErrorKind::BrokenPipe);
        if broken {
            std::process::exit(0);
        }
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::IndexFile {
            org,
            repo,
            language,
            path,
        } => {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("cannot read `{}`", path.display()))?;
            let path_str = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("`{}` is not a valid UTF-8 path", path.display()))?;
            if cli.db.is_dir() {
                bail!(
                    "--db `{}` is a directory; give a database file path",
                    cli.db.display()
                );
            }
            let mut store = Store::open(&cli.db)?;
            store.register(Box::new(graph_lang_rust::RustExtractor));
            let st = store.index_bytes(&org, &repo, path_str, &bytes, language.as_deref())?;
            let lang = st.language.clone();
            out!(
                "indexed {} ({}) tokens={} symbols={}{}{}",
                path.display(),
                lang,
                st.tokens,
                st.symbols,
                if st.replaced { " [replaced]" } else { "" },
                if st.has_errors { " [has_errors]" } else { "" }
            );
        }
        Cmd::Index {
            org,
            repo,
            json,
            max_file_size,
            prune,
            force,
            dir,
        } => index_dir(DirOpts {
            db: &cli.db,
            org: &org,
            repo: &repo,
            dir: &dir,
            json,
            max_file_size,
            prune,
            force,
        })?,
        Cmd::Describe { org, repo, json } => {
            let store = open_existing(&cli.db)?;
            let infos = store.describe(org.as_deref(), repo.as_deref())?;
            if json {
                out!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({ "repos": infos }))?
                );
            } else {
                for i in &infos {
                    out!("{}/{}: {} files", i.org, i.repo, i.files);
                    for (l, li) in &i.languages {
                        out!(
                            "  {l}: {} files, {} symbols, {} tokens",
                            li.files,
                            li.symbols,
                            li.tokens
                        );
                        for (k, n) in &li.symbol_kinds {
                            out!("    {k}: {n}");
                        }
                    }
                }
            }
        }
        Cmd::Symbols {
            pattern,
            kind,
            language,
            org,
            repo,
            file,
            limit,
            json,
        } => {
            let store = open_existing(&cli.db)?;
            validate_filters(
                &store,
                org.as_deref(),
                repo.as_deref(),
                language.as_deref(),
                kind.as_deref(),
            )?;
            let mut q = SymbolQuery::new(&pattern);
            (q.kind, q.language, q.org, q.repo, q.file) = (kind, language, org, repo, file);
            q.limit = limit.map(|l| l as usize);
            let hits = store.search_symbols(&q)?;
            if json {
                let out = serde_json::json!({ "query": pattern, "results": hits });
                out!("{}", serde_json::to_string(&out)?);
            } else {
                for h in &hits {
                    let loc = h
                        .span
                        .map(|s| format!(":{}:{}", s.start_line, s.start_col))
                        .unwrap_or_default();
                    out!(
                        "{}/{}/{}{loc}\t{}\t{} ({})\t{}",
                        h.org,
                        h.repo,
                        h.file,
                        h.language.as_deref().unwrap_or("-"),
                        h.kind.as_str(),
                        h.lang_kind.as_deref().unwrap_or("-"),
                        h.qualified
                    );
                }
            }
        }
        Cmd::Search {
            text,
            language,
            org,
            repo,
            kind,
            grain,
            symbol_kind,
            limit,
            json,
        } => {
            if symbol_kind.is_some() && grain != Grain::Symbol {
                bail!("--symbol-kind requires --grain symbol");
            }
            let store = open_existing(&cli.db)?;
            validate_filters(
                &store,
                org.as_deref(),
                repo.as_deref(),
                language.as_deref(),
                symbol_kind.as_deref(),
            )?;
            let mut q = Query::new(&text);
            (q.language, q.org, q.repo, q.class, q.grain, q.symbol_kind) =
                (language, org, repo, kind, grain, symbol_kind);
            q.limit = limit.map(|l| l as usize);
            let hits = store.search(&q)?;
            if json {
                let out = serde_json::json!({ "query": text, "grain": grain, "results": hits });
                out!("{}", serde_json::to_string(&out)?);
            } else {
                for h in &hits {
                    let loc = h
                        .span
                        .map(|s| format!(":{}:{}", s.start_line, s.start_col))
                        .unwrap_or_default();
                    let path = [Some(h.org.as_str()), h.repo.as_deref(), h.file.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("/");
                    out!(
                        "{path}{loc}\t{}\t{}\thits={}{}",
                        h.language.as_deref().unwrap_or("-"),
                        h.symbol.as_deref().unwrap_or("-"),
                        h.count,
                        if h.no_symbols {
                            "\tno_symbols"
                        } else if h.no_matching_symbol {
                            "\tno_matching_symbol"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }
    Ok(())
}
