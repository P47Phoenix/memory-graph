use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use graph_cli::{index_dir, DirOpts};
use graph_core::{Extractor, TokenClass};
use graph_store::{open_store, Grain, IndexOptions, Query, Store, SymbolQuery, V2Store};
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
    /// Deprecated, hidden: there is one storage format now. `--backend v2` is accepted as a no-op for old
    /// scripts; `--backend v1` is an error that says where the retired format went
    #[arg(long, global = true, hide = true, value_enum)]
    backend: Option<LegacyBackend>,
    /// Commit a write transaction every this many source bytes ingested, continuing the batch in a new one
    /// (default 64 MiB). A soft cap: one file larger than it is still a chunk of its own
    #[arg(long, global = true, alias = "v2-chunk-bytes", value_parser = clap::value_parser!(u64).range(1..))]
    chunk_bytes: Option<u64>,
    /// Cache size in bytes for the database (redb's default is 1 GiB, split 9:1 between its read and write
    /// caches)
    #[arg(long, global = true, alias = "v2-cache-bytes", value_parser = clap::value_parser!(u64).range(1..))]
    cache_bytes: Option<u64>,
    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    fn overrides(&self) -> Overrides {
        Overrides {
            chunk_bytes: self.chunk_bytes,
            cache_bytes: self.cache_bytes,
        }
    }
}

/// Values the retired `--backend` flag still parses. Kept so `--backend v2`
/// in an existing script keeps working and `--backend v1` fails with a
/// pointer to the migration path instead of an unknown-flag error.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum LegacyBackend {
    V1,
    V2,
}

/// The retired v1 format cannot be selected: say where it went.
fn reject_legacy_backend(backend: Option<LegacyBackend>) -> Result<()> {
    if backend == Some(LegacyBackend::V1) {
        bail!(
            "`--backend v1` is not supported: the v1 format was retired; this release reads only the v2 \
             format. Re-index from source into a new file (recommended), or convert an existing v1 database \
             with the last v1-capable release (git tag `v1-last`): `memory-graph migrate <new.redb>`"
        );
    }
    Ok(())
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
        /// Parse threads (default, or 0: one per CPU but one, left for the database writer). Files are still
        /// committed in walk order by one writer, so the stored content is the same for any value
        #[arg(long, short = 'j', default_value_t = 0, hide_default_value = true)]
        jobs: usize,
        /// Cap on source bytes read but not yet committed: a fixed size (2G), or the share of the free
        /// memory this process may grow into (70%, the default), divided by the measured growth per source
        /// byte, re-sampled during the run and lowered under memory pressure; at least 20% of RAM is always
        /// left for the OS. Also read from MEMORY_GRAPH_MEMORY
        #[arg(long, env = "MEMORY_GRAPH_MEMORY", value_parser = graph_cli::sysinfo::parse_memory_spec)]
        memory: Option<graph_cli::sysinfo::MemorySpec>,
        /// Commit fixed batches (256 files / 32 MiB) instead of everything ready, so the database file is
        /// byte-for-byte reproducible on any machine (slower when the writer is the bottleneck)
        #[arg(long)]
        deterministic: bool,
        /// Print how busy each stage (walk, parse, commit) was, and the bottleneck, on stderr at the end
        /// (with --json: a `stats` object in the summary)
        #[arg(long)]
        stats: bool,
        /// Write a Chrome/Perfetto trace of the run (one span per file per stage) to this file
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Show live progress on stderr even with --json (default: only when --json is off). Never drawn
        /// when stderr is not a terminal
        #[arg(long, conflicts_with = "no_progress")]
        progress: bool,
        /// Never show live progress
        #[arg(long)]
        no_progress: bool,
        /// Free space to keep on the database's volume, e.g. 4G or 5% (default: 5%, between 2G and 32G).
        /// The run refuses to start below it and stops cleanly, resumably, if it would go below it
        #[arg(long, value_parser = graph_cli::diskinfo::parse_min_free)]
        min_free_disk: Option<graph_cli::diskinfo::MinFree>,
        /// Report disk space but never stop for it
        #[arg(long)]
        no_disk_check: bool,
        dir: PathBuf,
    },
    /// Show what `index` sizes itself from on this machine: CPUs, memory (and where the reading came
    /// from, or why there is none), the budget it would start with, and the free space on --db's volume
    Sysinfo {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
        /// The `--memory` setting to size the budget with (default: 70%). Also read from MEMORY_GRAPH_MEMORY
        #[arg(long, env = "MEMORY_GRAPH_MEMORY", value_parser = graph_cli::sysinfo::parse_memory_spec)]
        memory: Option<graph_cli::sysinfo::MemorySpec>,
        /// The `--min-free-disk` setting to report the reserve for
        #[arg(long, value_parser = graph_cli::diskinfo::parse_min_free)]
        min_free_disk: Option<graph_cli::diskinfo::MinFree>,
    },
    /// Drop dictionary terms that no file refers to any more
    Vacuum {
        /// Also rebuild the file to reclaim the space `vacuum` frees but redb does not return on its own
        /// (single-process rebuild-then-rename)
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
        /// Skip this many results (same order as --limit) before collecting --limit of them;
        /// paired with --limit to page through a large result set (ADR 0003 story 11)
        #[arg(long)]
        offset: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Dump the database's full node graph as newline-delimited JSON, one
    /// JSON object per node (org, repo, file, symbol, token, each with its
    /// span): a portable escape hatch, not an importer -- there is no
    /// `import` command yet
    Export {
        /// Write to this file instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
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
        /// Skip this many rows (same order as --limit) before collecting --limit of them;
        /// paired with --limit to page through a large result set (ADR 0003 story 11)
        #[arg(long)]
        offset: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// Store settings taken from CLI flags. Both default to redb/`V2Store`
/// defaults (`None`).
#[derive(Clone, Copy, Default)]
struct Overrides {
    chunk_bytes: Option<u64>,
    cache_bytes: Option<u64>,
}

/// Open the store at `db` with `extractors` registered, applying
/// `overrides`. Goes through `open_store` whenever no override applies, so it
/// only diverges from `open_store`'s construction path (`V2Store::open` +
/// register) when an override needs a `V2Store` method
/// (`open_with_cache_bytes`, `set_chunk_bytes`) that isn't on the `Store`
/// trait; mirror any future change to `open_store` here too. A file in the
/// retired v1 format is refused (`StoreError::LegacyFormat`) before anything
/// is written, with the migration hint in the error.
fn open_with_overrides(
    db: &std::path::Path,
    extractors: Vec<Box<dyn Extractor>>,
    overrides: Overrides,
) -> Result<Box<dyn Store>> {
    if overrides.chunk_bytes.is_some() || overrides.cache_bytes.is_some() {
        let mut s = open_v2(db, overrides)?;
        for e in extractors {
            s.register(e);
        }
        return Ok(Box::new(s));
    }
    Ok(open_store(db, extractors)?)
}

/// A concrete `V2Store` with `overrides` applied (for `vacuum --compact`,
/// which needs an owned store).
fn open_v2(db: &std::path::Path, overrides: Overrides) -> Result<V2Store> {
    let mut s = V2Store::open_with_cache_bytes(db, overrides.cache_bytes.map(|b| b as usize))?;
    if let Some(bytes) = overrides.chunk_bytes {
        s.set_chunk_bytes(bytes as usize);
    }
    Ok(s)
}

/// Open the store for a command that indexes, with every shipped extractor
/// registered: the extractor version is part of a file's fingerprint, so
/// indexing without one would downgrade already-indexed files to tokens only.
fn open_for_indexing(db: &std::path::Path, overrides: Overrides) -> Result<Box<dyn Store>> {
    open_with_overrides(db, graph_cli::shipped_extractors(), overrides)
}

fn open_existing(db: &std::path::Path, overrides: Overrides) -> Result<Box<dyn Store>> {
    if !db.is_file() {
        bail!("database `{}` does not exist", db.display());
    }
    open_with_overrides(db, vec![], overrides)
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
    reject_legacy_backend(cli.backend)?;
    let overrides = cli.overrides();
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
            let store = open_for_indexing(&cli.db, overrides)?;
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
            jobs,
            memory,
            deterministic,
            stats,
            trace,
            progress,
            no_progress,
            min_free_disk,
            no_disk_check,
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
                jobs,
                memory,
                deterministic,
                stats,
                trace: trace.as_deref(),
                progress: match (progress, no_progress) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                disk_probe: None,
                min_free_disk: min_free_disk.unwrap_or(graph_cli::diskinfo::MinFree::Default),
                disk_check: !no_disk_check,
                chunk_bytes: cli.chunk_bytes.unwrap_or(graph_cli::DEFAULT_CHUNK_BYTES),
            },
            |db| open_for_indexing(db, overrides),
            &mut std::io::stdout().lock(),
        )?,
        Cmd::Sysinfo {
            json,
            memory,
            min_free_disk,
        } => {
            let r = graph_cli::report::Report::detect(
                &cli.db,
                memory,
                min_free_disk.unwrap_or(graph_cli::diskinfo::MinFree::Default),
            );
            if json {
                out!("{}", serde_json::to_string_pretty(&r.json())?);
            } else {
                write!(std::io::stdout().lock(), "{}", r.text())?;
            }
        }
        Cmd::Vacuum { compact } => {
            if !cli.db.is_file() {
                bail!("database `{}` does not exist", cli.db.display());
            }
            // `compact` is `V2Store`-only (it consumes and replaces `self`,
            // which `Store`'s `&self`-only shape can't express), so this
            // needs a concrete, owned `V2Store` rather than the `Box<dyn
            // Store>` the other arms use.
            let s = open_v2(&cli.db, overrides)
                .with_context(|| format!("opening database `{}`", cli.db.display()))?;
            let st = s.vacuum()?;
            out!(
                "vacuum: removed {} unused dictionary terms, kept {}",
                st.terms_removed,
                st.terms_kept
            );
            if compact {
                let (_, cst) = s.compact()?;
                out!(
                    "compact: {} bytes -> {} bytes",
                    cst.before_bytes,
                    cst.after_bytes
                );
            }
        }
        Cmd::Describe { org, repo, json } => {
            let store = open_existing(&cli.db, overrides)?;
            let infos = store.describe(org.as_deref(), repo.as_deref())?;
            if json {
                out!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({ "repos": infos }))?
                );
            } else {
                for i in &infos {
                    out!("{}/{}: {} files", i.org, i.repo, i.files);
                    if i.open_batch {
                        out!(
                            "  WARNING: repo {}/{} has an incomplete ingest batch \
                             (crashed or in-progress); re-index to repair",
                            i.org,
                            i.repo
                        );
                    }
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
            offset,
            json,
        } => {
            let store = open_existing(&cli.db, overrides)?;
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
            q.offset = offset.map(|o| o as usize);
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
        Cmd::Export { out } => {
            let store = open_existing(&cli.db, overrides)?;
            let mut file_writer;
            let mut stdout_writer;
            let writer: &mut dyn std::io::Write = match &out {
                Some(p) => {
                    file_writer = std::io::BufWriter::new(
                        std::fs::File::create(p)
                            .with_context(|| format!("creating `{}`", p.display()))?,
                    );
                    &mut file_writer
                }
                None => {
                    stdout_writer = std::io::stdout().lock();
                    &mut stdout_writer
                }
            };
            let n = graph_store::export::export_ndjson(store.as_ref(), writer)?;
            if out.is_some() {
                out!("exported {n} nodes");
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
            offset,
            json,
        } => {
            if symbol_kind.is_some() && grain != Grain::Symbol {
                bail!("--symbol-kind requires --grain symbol");
            }
            let store = open_existing(&cli.db, overrides)?;
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
            q.offset = offset.map(|o| o as usize);
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
mod legacy_backend_flag_tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `--backend v1` must never silently become a no-op: the retired
    /// format is refused with the migration hint. `--backend v2` and no flag
    /// are accepted, and other values are rejected by clap.
    #[test]
    fn backend_v1_is_refused_with_the_migration_hint() {
        let err = reject_legacy_backend(Some(LegacyBackend::V1))
            .unwrap_err()
            .to_string();
        for needle in ["retired", "v1-last", "Re-index", "memory-graph migrate"] {
            assert!(err.contains(needle), "{needle}: {err}");
        }
        reject_legacy_backend(Some(LegacyBackend::V2)).unwrap();
        reject_legacy_backend(None).unwrap();
        let cli = parse(&["memory-graph", "--backend", "v1", "describe"]);
        assert_eq!(cli.backend, Some(LegacyBackend::V1));
        assert!(reject_legacy_backend(cli.backend).is_err());
        let e = Cli::try_parse_from(["memory-graph", "--backend", "v3", "describe"])
            .err()
            .expect("v3 is rejected");
        assert!(e.to_string().contains("invalid value 'v3'"), "{e}");
    }

    /// The old flag names stay usable as hidden aliases.
    #[test]
    fn old_v2_flag_names_are_aliases() {
        let cli = parse(&[
            "memory-graph",
            "--v2-chunk-bytes",
            "7",
            "--v2-cache-bytes",
            "9",
            "describe",
        ]);
        assert_eq!((cli.chunk_bytes, cli.cache_bytes), (Some(7), Some(9)));
        let cli = parse(&[
            "memory-graph",
            "--chunk-bytes",
            "7",
            "--cache-bytes",
            "9",
            "describe",
        ]);
        assert_eq!((cli.chunk_bytes, cli.cache_bytes), (Some(7), Some(9)));
    }
}

#[cfg(test)]
mod chunk_bytes_flag_tests {
    use super::*;
    use graph_core::{Extraction, Span, SymbolDecl, SymbolKind};
    use graph_store::BatchFile;

    /// Same technique as `chunked_batches_are_atomic_per_chunk`
    /// (`crates/graph-store/src/v2_policy_tests.rs`): a symbol `lang_kind`
    /// containing a NUL makes storage reject that one file, poisoning
    /// whichever chunk it lands in.
    struct Poison;
    impl Extractor for Poison {
        fn language(&self) -> &str {
            "poison"
        }
        fn extract(&self, source: &str) -> Extraction {
            let span = Span {
                start: 0,
                end: source.len() as u32,
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: source.len() as u32 + 1,
            };
            let mut ex = Extraction {
                has_errors: false,
                symbols: vec![SymbolDecl {
                    name: "S".into(),
                    kind: SymbolKind::Function,
                    lang_kind: None,
                    span,
                }],
                tokens: vec![],
            };
            if source.contains("BAD") {
                ex.symbols[0].lang_kind = Some("nul\0".into());
            }
            ex
        }
    }

    fn batch<'a>(srcs: &'a [String], paths: &'a [String]) -> Vec<BatchFile<'a>> {
        srcs.iter()
            .zip(paths)
            .map(|(s, p)| BatchFile {
                path: p,
                bytes: s.as_bytes(),
                language: Some("poison"),
                origin: None,
            })
            .collect()
    }

    /// Issue #28: `--chunk-bytes` is plumbed through `open_with_overrides`
    /// (the function `open_for_indexing` calls for `index`/`index-file`) into
    /// `V2Store::set_chunk_bytes`. A prior e2e test only checked that search
    /// results were identical chunked vs. unchunked, which is true by design
    /// regardless of whether chunking actually happened -- QA confirmed by
    /// mutation that silently dropping the `set_chunk_bytes` call left the
    /// suite green.
    ///
    /// This test calls `open_with_overrides` directly -- the exact function
    /// the CLI's flag parsing invokes once `--chunk-bytes` is parsed into
    /// `Overrides` -- then runs a batch with one poisoned file through it
    /// (mirroring `chunked_batches_are_atomic_per_chunk`): a small chunk cap
    /// must commit the chunks before the failure, while the default
    /// (unchunked) cap commits nothing, since the whole batch is one
    /// transaction. If the override became a no-op, both runs would behave
    /// like the default cap and report the same file count.
    #[test]
    fn chunk_bytes_override_changes_commit_granularity() {
        let d = tempfile::tempdir().unwrap();
        // Each source is 10 bytes; f2 is poisoned.
        let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
            .map(String::from)
            .to_vec();
        let paths: Vec<String> = (0..4).map(|i| format!("f{i}.p")).collect();
        let opts = graph_store::IndexOptions::default();

        // Cap of 20 bytes: {f0, f1} commit as one chunk, then f2 fails and
        // only its own chunk is lost; f3 is never reached.
        let small = open_with_overrides(
            &d.path().join("small.redb"),
            vec![Box::new(Poison)],
            Overrides {
                chunk_bytes: Some(20),
                cache_bytes: None,
            },
        )
        .unwrap();
        assert!(small
            .index_batch("o", "r", &batch(&srcs, &paths), opts)
            .is_err());
        let files_small: usize = small
            .describe(None, None)
            .unwrap()
            .iter()
            .map(|r| r.files)
            .sum();
        assert_eq!(
            files_small, 2,
            "small --chunk-bytes: earlier chunks stay committed"
        );

        // No override: the whole batch is one transaction, all or nothing.
        let big = open_with_overrides(
            &d.path().join("big.redb"),
            vec![Box::new(Poison)],
            Overrides {
                chunk_bytes: None,
                cache_bytes: None,
            },
        )
        .unwrap();
        assert!(big
            .index_batch("o", "r", &batch(&srcs, &paths), opts)
            .is_err());
        let files_big: usize = big
            .describe(None, None)
            .unwrap()
            .iter()
            .map(|r| r.files)
            .sum();
        assert_eq!(
            files_big, 0,
            "no --chunk-bytes override: one chunk, nothing stored"
        );

        assert_ne!(
            files_small, files_big,
            "the --chunk-bytes override must change commit granularity, \
             or the flag has become a silent no-op (issue #28)"
        );
    }
}
