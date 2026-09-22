use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use graph_cli::{index_dir, DirOpts};
use graph_core::{Extractor, TokenClass};
use graph_store::{
    detect_backend, open_store, Backend, Grain, IndexOptions, Query, Store, SymbolQuery,
};
use std::io::Write;
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
    /// Storage backend: v1 (default) or v2 (opt-in, much smaller). A database is bound to the backend that
    /// created it: opening a v2 file without `--backend v2` (or a v1 file with it) is an error
    #[arg(long, global = true, value_enum)]
    backend: Option<BackendArg>,
    /// v2 only: commit a write transaction every this many source bytes ingested, continuing the batch in a
    /// new one (default 64 MiB). A soft cap: one file larger than it is still a chunk of its own. Ignored
    /// with `--backend v1` (or no `--backend`)
    #[arg(long, global = true, value_parser = clap::value_parser!(u64).range(1..))]
    v2_chunk_bytes: Option<u64>,
    /// v2 only: cache size in bytes for the database (redb's default is 1 GiB, split 9:1 between its read
    /// and write caches). Ignored with `--backend v1` (or no `--backend`)
    #[arg(long, global = true, value_parser = clap::value_parser!(u64).range(1..))]
    v2_cache_bytes: Option<u64>,
    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    fn v2_overrides(&self) -> V2Overrides {
        V2Overrides {
            chunk_bytes: self.v2_chunk_bytes,
            cache_bytes: self.v2_cache_bytes,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendArg {
    V1,
    V2,
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
        /// Re-index even when the file is unchanged since it was last indexed
        #[arg(long)]
        reindex: bool,
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
        /// Re-index every file even when unchanged since it was last indexed (by default files with the
        /// same content, language and extractor version are skipped). Does not affect --prune's safety checks
        #[arg(long)]
        reindex: bool,
        dir: PathBuf,
    },
    /// Drop dictionary terms that no file refers to any more (v2 databases; a no-op for v1)
    Vacuum {
        /// v2 only: also rebuild the file to reclaim the space `vacuum` frees but redb does not
        /// return on its own (single-process rebuild-then-rename). Ignored (not an error) on v1
        #[arg(long)]
        compact: bool,
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

/// Choose the backend for `db`. The default is v1. The database's stamped
/// schema version validates the choice: a file written by the other backend is
/// refused with a message that says how to open it. A file that cannot be
/// inspected here (locked, unreadable) is left to the normal open, which
/// reports its own error, so v1 behaviour is unchanged.
fn resolve_backend(db: &std::path::Path, requested: Option<BackendArg>) -> Result<Backend> {
    let want = match requested {
        Some(BackendArg::V2) => Backend::RedbV2,
        Some(BackendArg::V1) | None => Backend::Redb,
    };
    if let Ok(Some((found, version))) = detect_backend(db) {
        if found != want {
            // v1 is the default, so "drop --backend" only helps for a v1 file.
            let hint = if found == Backend::Redb {
                "drop --backend or pass `--backend v1`".to_string()
            } else {
                format!("pass `--backend {}`", found.name())
            };
            bail!(
                "database `{}` is a {} database (schema version {version}), but the {} backend was selected; {hint}",
                db.display(),
                found.name(),
                want.name()
            );
        }
    }
    Ok(want)
}

/// v2-only overrides taken from CLI flags. Both default to redb/`V2Store`
/// defaults (`None`) and are ignored on v1.
#[derive(Clone, Copy, Default)]
struct V2Overrides {
    chunk_bytes: Option<u64>,
    cache_bytes: Option<u64>,
}

/// Open a store of `backend` at `db` with `extractors` registered, applying
/// `overrides` when `backend` is v2. Goes through `open_store` whenever no
/// override applies, so it only diverges from `open_store`'s construction
/// path (`V2Store::open` + register) when an override needs a `V2Store`
/// method (`open_with_cache_bytes`, `set_chunk_bytes`) that isn't on the
/// `Store` trait; mirror any future change to `open_store`'s v2 arm here too.
fn open_with_overrides(
    backend: Backend,
    db: &std::path::Path,
    extractors: Vec<Box<dyn Extractor>>,
    overrides: V2Overrides,
) -> Result<Box<dyn Store>> {
    if backend == Backend::RedbV2
        && (overrides.chunk_bytes.is_some() || overrides.cache_bytes.is_some())
    {
        let mut s = graph_store::V2Store::open_with_cache_bytes(
            db,
            overrides.cache_bytes.map(|b| b as usize),
        )?;
        if let Some(bytes) = overrides.chunk_bytes {
            s.set_chunk_bytes(bytes as usize);
        }
        for e in extractors {
            s.register(e);
        }
        return Ok(Box::new(s));
    }
    Ok(open_store(backend, db, extractors)?)
}

/// Open the store for a command that indexes, with every shipped extractor
/// registered: the extractor version is part of a file's fingerprint, so
/// indexing without one would downgrade already-indexed files to tokens only.
fn open_for_indexing(
    db: &std::path::Path,
    backend: Option<BackendArg>,
    overrides: V2Overrides,
) -> Result<Box<dyn Store>> {
    open_with_overrides(
        resolve_backend(db, backend)?,
        db,
        vec![Box::new(graph_lang_rust::RustExtractor)],
        overrides,
    )
}

fn open_existing(
    db: &std::path::Path,
    backend: Option<BackendArg>,
    overrides: V2Overrides,
) -> Result<Box<dyn Store>> {
    if !db.is_file() {
        bail!("database `{}` does not exist", db.display());
    }
    open_with_overrides(resolve_backend(db, backend)?, db, vec![], overrides)
        .with_context(|| format!("opening database `{}`", db.display()))
}

/// Filters are checked against what is actually indexed (in the org/repo
/// scope), so a typo fails loudly and lists the valid choices rather than
/// returning nothing.
fn validate_filters(
    store: &dyn Store,
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
    let v2_overrides = cli.v2_overrides();
    match cli.cmd {
        Cmd::IndexFile {
            org,
            repo,
            language,
            reindex,
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
            let store = open_for_indexing(&cli.db, cli.backend, v2_overrides)?;
            let st = store.index_bytes_opts(
                &org,
                &repo,
                path_str,
                &bytes,
                language.as_deref(),
                None,
                IndexOptions { reindex },
            )?;
            let lang = st.language.clone();
            out!(
                "indexed {} ({}) tokens={} symbols={}{}{}{}",
                path.display(),
                lang,
                st.tokens,
                st.symbols,
                if st.replaced { " [replaced]" } else { "" },
                if st.unchanged { " [unchanged]" } else { "" },
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
            reindex,
            dir,
        } => index_dir(
            DirOpts {
                db: &cli.db,
                org: &org,
                repo: &repo,
                dir: &dir,
                json,
                max_file_size,
                prune,
                force,
                reindex,
            },
            |db| open_for_indexing(db, cli.backend, v2_overrides),
            &mut std::io::stdout().lock(),
        )?,
        Cmd::Vacuum { compact } => {
            if !cli.db.is_file() {
                bail!("database `{}` does not exist", cli.db.display());
            }
            let backend = resolve_backend(&cli.db, cli.backend)?;
            if backend == Backend::RedbV2 && compact {
                // `compact` is `V2Store`-only (it consumes and replaces
                // `self`, which `Store`'s `&self`-only shape can't express),
                // so this needs a concrete, owned `V2Store` rather than the
                // `Box<dyn Store>` the other arms use.
                let mut s = graph_store::V2Store::open_with_cache_bytes(
                    &cli.db,
                    v2_overrides.cache_bytes.map(|b| b as usize),
                )
                .with_context(|| format!("opening database `{}`", cli.db.display()))?;
                if let Some(bytes) = v2_overrides.chunk_bytes {
                    s.set_chunk_bytes(bytes as usize);
                }
                let st = s.vacuum()?;
                let (_, cst) = s.compact()?;
                out!(
                    "vacuum: removed {} unused dictionary terms, kept {}",
                    st.terms_removed,
                    st.terms_kept
                );
                out!(
                    "compact: {} bytes -> {} bytes",
                    cst.before_bytes,
                    cst.after_bytes
                );
            } else {
                let store = open_with_overrides(backend, &cli.db, vec![], v2_overrides)
                    .with_context(|| format!("opening database `{}`", cli.db.display()))?;
                let st = store.vacuum()?;
                if backend == Backend::Redb {
                    out!("vacuum: nothing to do (a v1 database has no dictionary)");
                } else {
                    out!(
                        "vacuum: removed {} unused dictionary terms, kept {}",
                        st.terms_removed,
                        st.terms_kept
                    );
                }
            }
        }
        Cmd::Describe { org, repo, json } => {
            let store = open_existing(&cli.db, cli.backend, v2_overrides)?;
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
            let store = open_existing(&cli.db, cli.backend, v2_overrides)?;
            validate_filters(
                &*store,
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
            let store = open_existing(&cli.db, cli.backend, v2_overrides)?;
            validate_filters(
                &*store,
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

#[cfg(test)]
mod resolve_tests {
    use super::*;

    /// A file `detect_backend` cannot classify (garbage, or locked by another
    /// handle) is not an error here: the requested backend is used and the
    /// real open reports its own error. Propagating would change v1's messages.
    #[test]
    fn resolve_backend_swallows_a_detect_error() {
        let d = tempfile::tempdir().unwrap();
        let g = d.path().join("garbage.redb");
        std::fs::write(&g, vec![0x5a; 4096]).unwrap();
        assert!(graph_store::detect_backend(&g).is_err(), "premise");
        assert_eq!(
            resolve_backend(&g, Some(BackendArg::V2)).unwrap(),
            Backend::RedbV2
        );
        assert_eq!(resolve_backend(&g, None).unwrap(), Backend::Redb);

        let l = d.path().join("locked.redb");
        let held = graph_store::open_store(Backend::Redb, &l, vec![]).unwrap();
        assert!(graph_store::detect_backend(&l).is_err(), "premise");
        assert_eq!(
            resolve_backend(&l, Some(BackendArg::V2)).unwrap(),
            Backend::RedbV2
        );
        drop(held);
    }
}
