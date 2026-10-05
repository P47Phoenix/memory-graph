//! Read-path benchmark for read cache phase 0 (ADR 0008, epic story 45; see
//! `docs/spikes/read-cache-measure.md`). Not built by `cargo test`; run with
//! `--release`:
//!
//! ```sh
//! cargo run --release -p graph-store --example readbench -- \
//!     <workdir> <corpus-dir> [warm-reps] [cold-cache-bytes]
//! ```
//!
//! Indexes every direct subdirectory of `<corpus-dir>` (e.g. `testdata/corpus`)
//! as one repo, then runs an agent-like workload -- searches at token, symbol,
//! method, class and file grain over hot and cold terms, `search_symbols`,
//! `file_tokens` and `describe` -- cold (fresh open, small redb cache), warm
//! (repeated) and concurrently (1/8/32 threads), and reports p50/p95 latency,
//! queries per second and the share of query time spent decoding, from
//! `graph_store::read_stats`.
use graph_store::{
    read_stats, set_read_timing, BatchFile, Grain, IndexOptions, Query, ReadStats, Store,
    StoreRead, SymbolQuery, V2Store,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ORG: &str = "corpus";
const HOT_TERMS: usize = 6;
const COLD_TERMS: usize = 18;
const FILE_TOKEN_READS: usize = 8;
const QUERY_LIMIT: usize = 50;
const THREAD_COUNTS: [usize; 3] = [1, 8, 32];

struct SourceFile {
    repo: String,
    path: String,
    text: String,
}

/// One workload step.
enum Op {
    Search(Query),
    Symbols(SymbolQuery),
    FileTokens { repo: String, path: String },
    Describe,
}

impl Op {
    fn run(&self, store: &dyn StoreRead) -> usize {
        match self {
            Op::Search(q) => store.search(q).expect("search").len(),
            Op::Symbols(q) => store.search_symbols(q).expect("search_symbols").len(),
            Op::FileTokens { repo, path } => store
                .file_tokens(ORG, repo, path)
                .expect("file_tokens")
                .map_or(0, |t| t.len()),
            Op::Describe => store.describe(None, None).expect("describe").len(),
        }
    }
}

fn walk(dir: &Path, root: &Path, repo: &str, out: &mut Vec<SourceFile>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, root, repo, out);
        } else if let Ok(text) = std::fs::read_to_string(&p) {
            let path = p
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push(SourceFile {
                repo: repo.to_string(),
                path,
                text,
            });
        }
    }
}

fn load_corpus(dir: &Path) -> Vec<SourceFile> {
    let mut repos: Vec<_> = std::fs::read_dir(dir)
        .expect("corpus dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    repos.sort();
    let mut out = vec![];
    for r in repos {
        let name = r.file_name().expect("name").to_string_lossy().into_owned();
        walk(&r, &r, &name, &mut out);
    }
    out
}

fn open(path: &Path, cache_bytes: Option<usize>) -> V2Store {
    let mut s = V2Store::open_with_cache_bytes(path, cache_bytes).expect("open store");
    s.register(Box::new(graph_lang_rust::RustExtractor));
    s
}

fn index(store: &V2Store, files: &[SourceFile]) -> usize {
    let mut by_repo: HashMap<&str, Vec<&SourceFile>> = HashMap::new();
    for f in files {
        by_repo.entry(&f.repo).or_default().push(f);
    }
    let mut tokens = 0;
    for (repo, fs) in by_repo {
        let batch: Vec<BatchFile> = fs
            .iter()
            .map(|f| BatchFile {
                path: &f.path,
                bytes: f.text.as_bytes(),
                language: None,
                origin: Some("directory"),
                ..Default::default()
            })
            .collect();
        for r in store
            .index_batch(ORG, repo, &batch, IndexOptions::default())
            .expect("index_batch")
        {
            tokens += r.expect("index file").tokens;
        }
    }
    tokens
}

/// Identifier-like words with their corpus frequency, most frequent first,
/// ties by text so the pick is deterministic.
fn ranked_words(files: &[SourceFile]) -> Vec<(String, usize)> {
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for f in files {
        for w in f
            .text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| w.len() >= 4 && w.starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            *freq.entry(w).or_default() += 1;
        }
    }
    let mut words: Vec<_> = freq.into_iter().map(|(w, n)| (w.to_string(), n)).collect();
    words.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    words
}

fn workload(files: &[SourceFile]) -> Vec<Op> {
    let words = ranked_words(files);
    let hot = words.iter().take(HOT_TERMS).map(|w| w.0.clone());
    let cold_pool: Vec<_> = words.iter().filter(|w| w.1 == 3).collect();
    let stride = (cold_pool.len() / COLD_TERMS).max(1);
    let cold = cold_pool
        .iter()
        .step_by(stride)
        .take(COLD_TERMS)
        .map(|w| w.0.clone());
    let mut ops = vec![];
    for term in hot.chain(cold) {
        for grain in [
            Grain::Token,
            Grain::Symbol,
            Grain::Method,
            Grain::Class,
            Grain::File,
        ] {
            let mut q = Query::new(&term);
            q.grain = grain;
            q.limit = Some(QUERY_LIMIT);
            ops.push(Op::Search(q));
        }
        let mut sq = SymbolQuery::new(format!("{term}*"));
        sq.limit = Some(QUERY_LIMIT);
        ops.push(Op::Symbols(sq));
    }
    let stride = (files.len() / FILE_TOKEN_READS).max(1);
    for f in files.iter().step_by(stride).take(FILE_TOKEN_READS) {
        ops.push(Op::FileTokens {
            repo: f.repo.clone(),
            path: f.path.clone(),
        });
    }
    ops.push(Op::Describe);
    ops
}

/// Latencies of one pass, plus the total rows (to keep results observable).
fn run_pass(store: &dyn StoreRead, ops: &[Op], lat: &mut Vec<Duration>) -> usize {
    ops.iter()
        .map(|op| {
            let t = Instant::now();
            let n = op.run(store);
            lat.push(t.elapsed());
            n
        })
        .sum()
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn report(label: &str, mut lat: Vec<Duration>, wall: Duration, stats: ReadStats) {
    lat.sort();
    let busy: f64 = lat.iter().map(Duration::as_secs_f64).sum();
    let share = stats.decode_nanos() as f64 / 1e9 / busy * 100.0;
    let n = lat.len() as f64;
    println!(
        "| {label} | {} | {:.3} | {:.3} | {:.0} | {share:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.1} | {:.1} | {:.1} | {:.2} |",
        lat.len(),
        percentile(&lat, 0.5),
        percentile(&lat, 0.95),
        n / wall.as_secs_f64(),
        stats.dict_decode_nanos as f64 / 1e9 / busy * 100.0,
        stats.lazy_decode_nanos as f64 / 1e9 / busy * 100.0,
        stats.full_decode_nanos as f64 / 1e9 / busy * 100.0,
        stats.dict_block_decodes as f64 / n,
        stats.lazy_stream_decodes as f64 / n,
        stats.symbol_section_decodes as f64 / n,
        stats.read_txns as f64 / n,
    );
}

fn header() {
    println!("\n| phase | queries | p50 ms | p95 ms | qps | decode share | dict | lazy+sym | full | dict blocks/q | lazy/q | sym/q | txns/q |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
}

fn cold(db: &Path, ops: &[Op], cache_bytes: usize) {
    let store = open(db, Some(cache_bytes));
    let before = read_stats();
    let mut lat = vec![];
    let t = Instant::now();
    run_pass(&store, ops, &mut lat);
    report(
        &format!("cold, cache {} KiB", cache_bytes >> 10),
        lat,
        t.elapsed(),
        read_stats().since(&before),
    );
}

fn warm(store: &V2Store, ops: &[Op], reps: usize, timing: bool) -> Duration {
    set_read_timing(timing);
    run_pass(store, ops, &mut vec![]);
    let before = read_stats();
    let mut lat = vec![];
    let t = Instant::now();
    for _ in 0..reps {
        run_pass(store, ops, &mut lat);
    }
    let wall = t.elapsed();
    let label = format!("warm x{reps}, timing {}", if timing { "on" } else { "off" });
    report(&label, lat, wall, read_stats().since(&before));
    wall
}

fn concurrent(store: &V2Store, ops: &[Op], reps: usize, threads: usize) {
    let before = read_stats();
    let t = Instant::now();
    let lat: Vec<Duration> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut lat = vec![];
                    for _ in 0..reps {
                        run_pass(store, ops, &mut lat);
                    }
                    lat
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("reader thread"))
            .collect()
    });
    report(
        &format!("{threads} threads x{reps}"),
        lat,
        t.elapsed(),
        read_stats().since(&before),
    );
}

/// Cost of one relaxed counter bump, to bound the always-on overhead.
fn bump_cost_ns() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    const N: u64 = 50_000_000;
    let c = AtomicU64::new(0);
    let t = Instant::now();
    for _ in 0..N {
        c.fetch_add(1, Ordering::Relaxed);
    }
    std::hint::black_box(c.load(Ordering::Relaxed));
    t.elapsed().as_secs_f64() * 1e9 / N as f64
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let work = PathBuf::from(
        args.get(1)
            .expect("usage: readbench <workdir> <corpus-dir>"),
    );
    let corpus = PathBuf::from(
        args.get(2)
            .expect("usage: readbench <workdir> <corpus-dir>"),
    );
    let reps: usize = args.get(3).map_or(20, |s| s.parse().expect("warm-reps"));
    let cold_cache: usize = args
        .get(4)
        .map_or(1 << 20, |s| s.parse().expect("cache bytes"));

    let files = load_corpus(&corpus);
    let db = work.join("readbench.redb");
    let _ = std::fs::remove_file(&db);
    let t = Instant::now();
    let tokens = index(&open(&db, None), &files);
    println!(
        "indexed {} files, {tokens} tokens in {:.1}s; db {:.1} MiB",
        files.len(),
        t.elapsed().as_secs_f64(),
        std::fs::metadata(&db).expect("db").len() as f64 / 1048576.0
    );
    let ops = workload(&files);
    println!("workload: {} queries per pass", ops.len());

    header();
    set_read_timing(true);
    cold(&db, &ops, cold_cache);
    let store = open(&db, None);
    let off = warm(&store, &ops, reps, false);
    let on = warm(&store, &ops, reps, true);
    for threads in THREAD_COUNTS {
        concurrent(&store, &ops, (reps / 4).max(2), threads);
    }
    let bump = bump_cost_ns();
    println!(
        "\ntiming overhead (warm, on vs off): {:+.1}%; one relaxed counter bump: {bump:.2} ns",
        (on.as_secs_f64() / off.as_secs_f64() - 1.0) * 100.0
    );
}
