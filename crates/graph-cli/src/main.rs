use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use graph_core::{SymbolKind, TokenClass};
use graph_store::{Grain, Query, Store};
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
        dir: PathBuf,
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

/// Index every text file under `dir`. Paths are stored relative to `dir`.
fn index_dir(
    db: &std::path::Path,
    org: &str,
    repo: &str,
    dir: &std::path::Path,
    json: bool,
) -> Result<()> {
    use std::collections::BTreeMap;
    if !dir.is_dir() {
        bail!("`{}` is not a directory", dir.display());
    }
    if db.is_dir() {
        bail!(
            "--db `{}` is a directory; give a database file path",
            db.display()
        );
    }
    let start = std::time::Instant::now();
    let mut store = Store::open(db)?;
    store.register(Box::new(graph_lang_rust::RustExtractor));
    let (mut files, mut symbols, mut tokens) = (0usize, 0usize, 0usize);
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut skipped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).unwrap_or(entry.path());
        let rel_s = rel.to_string_lossy().into_owned();
        let bytes = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(e) => {
                skipped
                    .entry(format!("unreadable: {e}"))
                    .or_default()
                    .push(rel_s);
                continue;
            }
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            skipped.entry("binary".into()).or_default().push(rel_s);
            continue;
        }
        match store.index_bytes(org, repo, &rel_s, &bytes, None) {
            Ok(st) => {
                files += 1;
                symbols += st.symbols;
                tokens += st.tokens;
                *by_lang.entry(st.language).or_default() += 1;
            }
            Err(graph_store::StoreError::Rejected(r)) => {
                let reason = if r.contains("UTF-8") {
                    "not valid UTF-8"
                } else {
                    "too large"
                };
                skipped.entry(reason.into()).or_default().push(rel_s);
            }
            Err(e) => return Err(e.into()),
        }
    }
    let ms = start.elapsed().as_millis();
    let skipped_n: usize = skipped.values().map(Vec::len).sum();
    if json {
        let out = serde_json::json!({
            "org": org, "repo": repo, "files": files, "symbols": symbols, "tokens": tokens,
            "languages": by_lang, "skipped": skipped_n, "skipped_by_reason": skipped, "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!("indexed {org}/{repo}: files={files} symbols={symbols} tokens={tokens} skipped={skipped_n} elapsed={ms}ms");
        for (l, n) in &by_lang {
            println!("  {l}: {n}");
        }
        for (r, v) in &skipped {
            println!("  skipped ({r}): {}", v.len());
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
            dir,
        } => index_dir(&cli.db, &org, &repo, &dir, json)?,
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
