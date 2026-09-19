use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use graph_core::{Extractor, FallbackExtractor, SymbolKind, TokenClass};
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

/// Language name from extension; unknown extensions use the extension itself.
fn detect_language(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "" => "unknown",
        other => return other.to_string(),
    }
    .to_string()
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
            let src = String::from_utf8(bytes).map_err(|e| {
                anyhow::anyhow!("`{}` rejected: not valid UTF-8 ({e})", path.display())
            })?;
            let lang = language.unwrap_or_else(|| detect_language(&path));
            // No language extractors are registered yet; everything uses the fallback.
            let ex = FallbackExtractor::new(&lang);
            let extraction = ex.extract(&src);
            let store = Store::open(&cli.db)?;
            let st = store.ingest_file(
                &org,
                &repo,
                &path.to_string_lossy(),
                ex.language(),
                &extraction,
            )?;
            println!(
                "indexed {} ({}) tokens={} symbols={}{}",
                path.display(),
                lang,
                st.tokens,
                st.symbols,
                if st.replaced { " [replaced]" } else { "" }
            );
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
            if !cli.db.exists() {
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
                        if h.no_symbols { "\tno_symbols" } else { "" }
                    );
                }
            }
        }
    }
    Ok(())
}
