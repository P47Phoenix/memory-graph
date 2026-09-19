use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use graph_core::{SymbolKind, TokenClass};
use graph_store::{Grain, Query, Store, SymbolQuery, ORIGIN_DIRECTORY};
use std::io::Read;
use std::path::PathBuf;

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
    /// Find symbols (definitions) by name; a trailing `*` matches a prefix
    Symbols {
        /// Exact name, or `prefix*`
        pattern: String,
        /// Generic kind: module, type, function, method, variable, constant, other
        #[arg(long)]
        kind: Option<SymbolKind>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// Restrict to one file path as indexed (relative to the indexed directory)
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Find tokens by exact text
    Search {
        text: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// Token class: identifier, keyword, literal, operator, punctuation, comment, other
        #[arg(long)]
        kind: Option<TokenClass>,
        /// token, symbol, file, repo or org
        #[arg(long, default_value = "token")]
        grain: Grain,
        /// With --grain symbol: only symbols of this kind (e.g. method)
        #[arg(long)]
        symbol_kind: Option<SymbolKind>,
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

/// Index every text file under `dir`. Paths are stored relative to `dir`.
/// Per-file problems are counted as skips; only database failures abort.
fn index_dir(o: DirOpts) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
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
    let (mut files, mut symbols, mut tokens) = (0usize, 0usize, 0usize);
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut skipped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
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
        match store.index_bytes_with_origin(
            o.org,
            o.repo,
            &rel_s,
            &bytes,
            None,
            Some(ORIGIN_DIRECTORY),
        ) {
            Ok(st) => {
                files += 1;
                symbols += st.symbols;
                tokens += st.tokens;
                *by_lang.entry(st.language).or_default() += 1;
                seen.insert(st.path);
            }
            Err(graph_store::StoreError::NotUtf8(_)) => {
                skipped
                    .entry("not valid UTF-8".into())
                    .or_default()
                    .push(rel_s);
            }
            Err(graph_store::StoreError::TooLarge(_)) => {
                skipped.entry("too large".into()).or_default().push(rel_s);
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "database error while indexing `{rel_s}` ({files} files were already stored)"
                )))
            }
        }
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
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!(
            "indexed {}/{}: files={files} symbols={symbols} tokens={tokens} skipped={skipped_n} pruned={} elapsed={ms}ms",
            o.org, o.repo, pruned.len()
        );
        for (l, n) in &by_lang {
            println!("  {l}: {n}");
        }
        if !pruned.is_empty() {
            println!("  pruned:");
            for p in pruned.iter().take(20) {
                println!("    {p}");
            }
            if pruned.len() > 20 {
                println!(
                    "    ... and {} more (use --json for all)",
                    pruned.len() - 20
                );
            }
        }
        for (r, v) in &skipped {
            println!("  skipped ({r}): {}", v.len());
            for p in v.iter().take(20) {
                println!("    {p}");
            }
            if v.len() > 20 {
                println!("    ... and {} more (use --json for all)", v.len() - 20);
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
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
            println!(
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
        Cmd::Symbols {
            pattern,
            kind,
            language,
            org,
            repo,
            file,
            json,
        } => {
            if !cli.db.is_file() {
                bail!("database `{}` does not exist", cli.db.display());
            }
            let store = Store::open(&cli.db)?;
            let mut q = SymbolQuery::new(&pattern);
            (q.kind, q.language, q.org, q.repo, q.file) = (kind, language, org, repo, file);
            let hits = store.search_symbols(&q)?;
            if json {
                let out = serde_json::json!({ "query": pattern, "results": hits });
                println!("{}", serde_json::to_string(&out)?);
            } else {
                for h in &hits {
                    let loc = h
                        .span
                        .map(|s| format!(":{}:{}", s.start_line, s.start_col))
                        .unwrap_or_default();
                    println!(
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
            json,
        } => {
            if symbol_kind.is_some() && grain != Grain::Symbol {
                bail!("--symbol-kind requires --grain symbol");
            }
            if !cli.db.is_file() {
                bail!("database `{}` does not exist", cli.db.display());
            }
            let store = Store::open(&cli.db)?;
            let mut q = Query::new(&text);
            (q.language, q.org, q.repo, q.class, q.grain, q.symbol_kind) =
                (language, org, repo, kind, grain, symbol_kind);
            let hits = store.search(&q)?;
            if json {
                let out = serde_json::json!({ "query": text, "grain": grain, "results": hits });
                println!("{}", serde_json::to_string(&out)?);
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
                    println!(
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
