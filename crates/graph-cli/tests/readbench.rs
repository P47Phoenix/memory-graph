//! Read-path benchmark for the read cache (ADR 0008, epic stories 45-46; see
//! `docs/spikes/read-cache.md`). Not a gate (timings depend on the machine):
//!
//! ```sh
//! cargo test --release -p graph-cli --test readbench measure_reads -- --ignored --nocapture
//! ```
//!
//! Inputs, all optional, come from the environment:
//!
//! - `MG_READBENCH_CORPUS`: the corpus; every direct subdirectory is one
//!   repo (default `testdata/corpus`).
//! - `MG_READBENCH_WORKDIR`: where the database and the workload file go
//!   (default a temp dir, removed afterwards).
//! - `MG_READBENCH_REPS`: passes per warm and concurrent phase (default 20).
//! - `MG_READBENCH_COLD_CACHE`: the cold phase's redb cache in bytes
//!   (default 1 MiB).
//! - `MG_READBENCH_REUSE=1`: reuse the database and workload already in the
//!   workdir instead of indexing again (index once, measure many times).
//!   It needs a fixed `MG_READBENCH_WORKDIR` and ignores the caps; when the
//!   files are missing it warns and indexes afresh.
//! - `MG_READBENCH_MAX_FILES` / `MG_READBENCH_MAX_BYTES`: stop indexing
//!   after this many files / source bytes.
//!
//! The walk is `index_dir`'s (`graph_cli::dir_walker`: the repo's
//! `.gitignore` rules, `.git` skipped, the same binary check and size cap),
//! plus two extra skipped directories, `target` and `node_modules`.
//!
//! The workload is agent-like: searches at token, symbol, method, class and
//! file grain over hot and cold terms, `search_symbols`, `file_tokens` and
//! `describe`. It runs cold (a **fresh process** with a small redb cache;
//! the OS page cache stays warm unless it is dropped by hand), warm, and on
//! 1/8/16/32 reader threads, and reports p50/p95 latency, queries per
//! second and the share of query time spent decoding: each reader's own
//! share (from `read_stats::thread_snapshot`), median across readers, as
//! ADR 0008 defines the gate.
use graph_store::read_stats::{self, ReadStats};
use graph_store::{BatchFile, Grain, IndexOptions, Query, Store, StoreRead, SymbolQuery, V2Store};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ORG: &str = "corpus";
const DB_NAME: &str = "readbench.redb";
const WORKLOAD_NAME: &str = "readbench-workload.json";
/// Names the database for the fresh-process cold phase.
const COLD_DB_ENV: &str = "MG_READBENCH_COLD_DB";
const COLD_FN: &str = "readbench_cold_phase";
const HOT_TERMS: usize = 6;
const COLD_TERMS: usize = 18;
/// A cold term occurs exactly this often in the sample.
const COLD_TERM_FREQUENCY: usize = 3;
const FILE_TOKEN_READS: usize = 8;
const QUERY_LIMIT: usize = 50;
const THREAD_COUNTS: [usize; 4] = [1, 8, 16, 32];
/// Alternating timing-off/on warm runs for the overhead range.
const OVERHEAD_TRIALS: usize = 5;
/// Files and source bytes per `index_batch` call while streaming.
const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 32 << 20;
/// Source bytes per repo whose words feed the workload, so a big tree
/// does not hold every distinct identifier in memory.
const SAMPLE_BYTES_PER_REPO: u64 = 8 << 20;
/// Directories never walked. `.git` is `index_dir`'s own skip; `target`
/// and `node_modules` are extra, for clones whose `.gitignore` does not
/// cover their build output or dependencies.
const SKIPPED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

fn is_skipped_dir(name: &std::ffi::OsStr) -> bool {
    SKIPPED_DIRS.iter().any(|d| name == *d)
}

/// Benchmark inputs, from the `MG_READBENCH_*` environment.
struct Config {
    corpus: PathBuf,
    workdir: PathBuf,
    reps: usize,
    cold_cache: usize,
    reuse: bool,
    max_files: Option<u64>,
    max_bytes: Option<u64>,
}

fn env_num<T: std::str::FromStr>(name: &str) -> Option<T> {
    let v = std::env::var(name).ok()?;
    Some(
        v.parse()
            .unwrap_or_else(|_| panic!("{name}={v} is not a number")),
    )
}

fn reps_from_env() -> usize {
    let reps = env_num("MG_READBENCH_REPS").unwrap_or(20);
    assert!(
        reps >= 1,
        "MG_READBENCH_REPS must be at least 1, got {reps}"
    );
    reps
}

fn reuse_from_env() -> bool {
    match std::env::var("MG_READBENCH_REUSE").as_deref() {
        Ok("1") => true,
        Ok("0") | Ok("") | Err(_) => false,
        Ok(other) => {
            eprintln!("warning: MG_READBENCH_REUSE={other} is not 1; indexing afresh");
            false
        }
    }
}

impl Config {
    fn from_env(default_workdir: &Path) -> Self {
        let corpus = std::env::var_os("MG_READBENCH_CORPUS").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus"),
            PathBuf::from,
        );
        Self {
            corpus,
            workdir: std::env::var_os("MG_READBENCH_WORKDIR")
                .map_or_else(|| default_workdir.to_path_buf(), PathBuf::from),
            reps: reps_from_env(),
            cold_cache: env_num("MG_READBENCH_COLD_CACHE").unwrap_or(1 << 20),
            reuse: reuse_from_env(),
            max_files: env_num("MG_READBENCH_MAX_FILES"),
            max_bytes: env_num("MG_READBENCH_MAX_BYTES"),
        }
    }
}

/// What the queries are built from: kept in a file so the cold child
/// process and a `REUSE` run measure the same workload.
struct Workload {
    terms: Vec<String>,
    file_reads: Vec<(String, String)>,
}

impl Workload {
    fn save(&self, path: &Path) {
        let v = serde_json::json!({ "terms": self.terms, "file_reads": self.file_reads });
        std::fs::write(path, v.to_string()).expect("write workload");
    }

    fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).expect("read workload");
        let v: serde_json::Value = serde_json::from_str(&text).expect("workload json");
        let strings = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .expect("array")
                .iter()
                .map(|s| s.as_str().expect("string").to_string())
                .collect()
        };
        Self {
            terms: strings(&v["terms"]),
            file_reads: v["file_reads"]
                .as_array()
                .expect("file_reads")
                .iter()
                .map(|p| {
                    let pair = strings(p);
                    (pair[0].clone(), pair[1].clone())
                })
                .collect(),
        }
    }

    fn ops(&self) -> Vec<Op> {
        let mut ops = vec![];
        for term in &self.terms {
            for grain in [
                Grain::Token,
                Grain::Symbol,
                Grain::Method,
                Grain::Class,
                Grain::File,
            ] {
                let mut q = Query::new(term);
                q.grain = grain;
                q.limit = Some(QUERY_LIMIT);
                ops.push(Op::Search(q));
            }
            let mut sq = SymbolQuery::new(format!("{term}*"));
            sq.limit = Some(QUERY_LIMIT);
            ops.push(Op::Symbols(sq));
        }
        for (repo, path) in &self.file_reads {
            ops.push(Op::FileTokens {
                repo: repo.clone(),
                path: path.clone(),
            });
        }
        ops.push(Op::Describe);
        ops
    }
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

fn open(path: &Path, cache_bytes: Option<usize>) -> V2Store {
    let mut s = V2Store::open_with_cache_bytes(path, cache_bytes).expect("open store");
    for e in graph_cli::shipped_extractors() {
        s.register(e);
    }
    s
}

/// Source files under `repo_dir` in path order, walked as `index_dir`
/// walks (its `.gitignore` rules), plus the extra `SKIPPED_DIRS`.
fn repo_files(repo_dir: &Path) -> impl Iterator<Item = PathBuf> {
    graph_cli::dir_walker(repo_dir)
        .filter_entry(|e| !is_skipped_dir(e.file_name()))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(ignore::DirEntry::into_path)
}

/// The corpus's repos: its direct subdirectories, less `SKIPPED_DIRS`.
fn corpus_repos(corpus: &Path) -> Vec<PathBuf> {
    let mut repos: Vec<_> = std::fs::read_dir(corpus)
        .unwrap_or_else(|e| panic!("corpus dir {}: {e}", corpus.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir() && !p.file_name().is_some_and(is_skipped_dir))
        .collect();
    assert!(
        !repos.is_empty(),
        "corpus {} has no repo subdirectories (each subdirectory is one repo)",
        corpus.display()
    );
    repos.sort();
    repos
}

/// A file's bytes as `index_dir` reads them: `None` when it is over the
/// span limit, unreadable or binary (`is_binary_with_hint`, so UTF-16 and
/// BOM files are kept).
fn read_source(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let len = std::fs::metadata(path).ok()?.len();
    if graph_cli::size_skip_reason(len, None).is_some() {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| {
            f.take(graph_cli::read_cap(None).saturating_add(1))
                .read_to_end(&mut bytes)
        })
        .ok()?;
    let fits = graph_cli::size_skip_reason(bytes.len() as u64, None).is_none();
    (fits && !graph_core::encoding::is_binary_with_hint(&bytes, None)).then_some(bytes)
}

/// A file read from disk, waiting in the current batch.
struct Pending {
    path: String,
    bytes: Vec<u8>,
    /// Feeds the workload sample once it indexes.
    is_sampled: bool,
}

/// Streams the corpus into the store batch by batch, sampling words and
/// paths of files that indexed for the workload.
#[derive(Default)]
struct Indexer {
    files: u64,
    bytes: u64,
    tokens: usize,
    skipped: u64,
    word_freq: HashMap<String, usize>,
    sampled_paths: Vec<(String, String)>,
}

impl Indexer {
    fn under_caps(&self, cfg: &Config) -> bool {
        cfg.max_files.is_none_or(|m| self.files < m) && cfg.max_bytes.is_none_or(|m| self.bytes < m)
    }

    fn sample(&mut self, repo: &str, file: &Pending) {
        let text = String::from_utf8_lossy(&file.bytes);
        for w in text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| w.len() >= 4 && w.starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            *self.word_freq.entry(w.to_string()).or_default() += 1;
        }
        self.sampled_paths
            .push((repo.to_string(), file.path.clone()));
    }

    fn flush(&mut self, store: &V2Store, repo: &str, batch: &mut Vec<Pending>) {
        if batch.is_empty() {
            return;
        }
        let files: Vec<BatchFile> = batch
            .iter()
            .map(|f| BatchFile {
                path: &f.path,
                bytes: &f.bytes,
                origin: Some("directory"),
                ..Default::default()
            })
            .collect();
        let results = store
            .index_batch(ORG, repo, &files, IndexOptions::default())
            .expect("index_batch");
        for (file, r) in batch.iter().zip(results) {
            match r {
                Ok(stats) => {
                    self.tokens += stats.tokens;
                    if file.is_sampled {
                        self.sample(repo, file);
                    }
                }
                Err(_) => self.skipped += 1,
            }
        }
        batch.clear();
    }

    fn index_repo(&mut self, store: &V2Store, cfg: &Config, repo_dir: &Path) {
        let repo = repo_dir
            .file_name()
            .expect("repo name")
            .to_string_lossy()
            .into_owned();
        let mut batch: Vec<Pending> = vec![];
        let mut batch_bytes = 0u64;
        let mut sampled_bytes = 0u64;
        for p in repo_files(repo_dir) {
            if !self.under_caps(cfg) {
                break;
            }
            // Like index_dir: a path that is not UTF-8 is skipped.
            let Some(rel) = p.strip_prefix(repo_dir).expect("under repo").to_str() else {
                self.skipped += 1;
                continue;
            };
            let path = graph_core::normalize_path(rel);
            let Some(bytes) = read_source(&p) else {
                self.skipped += 1;
                continue;
            };
            let len = bytes.len() as u64;
            let is_sampled = sampled_bytes < SAMPLE_BYTES_PER_REPO;
            if is_sampled {
                sampled_bytes += len;
            }
            self.files += 1;
            self.bytes += len;
            batch_bytes += len;
            batch.push(Pending {
                path,
                bytes,
                is_sampled,
            });
            if batch.len() >= BATCH_FILES || batch_bytes >= BATCH_BYTES {
                self.flush(store, &repo, &mut batch);
                batch_bytes = 0;
            }
        }
        self.flush(store, &repo, &mut batch);
    }

    /// Hot terms are the most frequent sampled words; cold terms occur
    /// exactly `COLD_TERM_FREQUENCY` times, spread evenly over that pool.
    fn workload(&self) -> Workload {
        let mut words: Vec<(&str, usize)> = self
            .word_freq
            .iter()
            .map(|(w, n)| (w.as_str(), *n))
            .collect();
        words.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let hot = words.iter().take(HOT_TERMS);
        let cold_pool: Vec<_> = words
            .iter()
            .filter(|w| w.1 == COLD_TERM_FREQUENCY)
            .collect();
        let stride = (cold_pool.len() / COLD_TERMS).max(1);
        let cold = cold_pool.into_iter().step_by(stride).take(COLD_TERMS);
        let path_stride = (self.sampled_paths.len() / FILE_TOKEN_READS).max(1);
        Workload {
            terms: hot.chain(cold).map(|w| w.0.to_string()).collect(),
            file_reads: self
                .sampled_paths
                .iter()
                .step_by(path_stride)
                .take(FILE_TOKEN_READS)
                .cloned()
                .collect(),
        }
    }
}

/// Index the corpus into a fresh database (or reuse one; `REUSE` ignores
/// the caps) and return the database path and the workload.
fn prepare(cfg: &Config) -> (PathBuf, Workload) {
    let db = cfg.workdir.join(DB_NAME);
    let workload_path = cfg.workdir.join(WORKLOAD_NAME);
    if cfg.reuse {
        if db.is_file() && workload_path.is_file() {
            println!("reusing {} (the caps do not apply)", db.display());
            return (db, Workload::load(&workload_path));
        }
        eprintln!(
            "warning: MG_READBENCH_REUSE=1 but {} or {} is missing; indexing afresh",
            db.display(),
            workload_path.display()
        );
    }
    let repos = corpus_repos(&cfg.corpus);
    std::fs::create_dir_all(&cfg.workdir).expect("create workdir");
    let _ = std::fs::remove_file(&db);
    let t = Instant::now();
    let mut indexer = Indexer::default();
    {
        let store = open(&db, None);
        for repo_dir in repos {
            if !indexer.under_caps(cfg) {
                break;
            }
            indexer.index_repo(&store, cfg, &repo_dir);
        }
    }
    println!(
        "indexed {} files ({:.1} MiB, {} skipped), {} tokens in {:.1}s; db {:.1} MiB",
        indexer.files,
        indexer.bytes as f64 / 1048576.0,
        indexer.skipped,
        indexer.tokens,
        t.elapsed().as_secs_f64(),
        std::fs::metadata(&db).expect("db").len() as f64 / 1048576.0
    );
    let workload = indexer.workload();
    workload.save(&workload_path);
    (db, workload)
}

/// What one reader thread measured: per-query latencies, and its own
/// counters (exact, from `read_stats::thread_snapshot`).
struct Reader {
    lat: Vec<Duration>,
    stats: ReadStats,
}

impl Reader {
    fn busy_secs(&self) -> f64 {
        self.lat.iter().map(Duration::as_secs_f64).sum()
    }

    /// Decode nanoseconds over this reader's summed query latency, in %.
    fn decode_share(&self) -> f64 {
        self.stats.decode_nanos() as f64 / 1e9 / self.busy_secs() * 100.0
    }
}

/// Run `reps` passes on this thread and measure them.
fn read_passes(store: &dyn StoreRead, ops: &[Op], reps: usize) -> Reader {
    let before = read_stats::thread_snapshot();
    let mut lat = vec![];
    for _ in 0..reps {
        for op in ops {
            let t = Instant::now();
            std::hint::black_box(op.run(store));
            lat.push(t.elapsed());
        }
    }
    Reader {
        lat,
        stats: read_stats::thread_snapshot().since(&before),
    }
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn add(a: ReadStats, b: &ReadStats) -> ReadStats {
    let mut out = a;
    out.dict_block_decodes += b.dict_block_decodes;
    out.dict_strings_decoded += b.dict_strings_decoded;
    out.dict_text_memo_hits += b.dict_text_memo_hits;
    out.dict_text_memo_misses += b.dict_text_memo_misses;
    out.full_stream_decodes += b.full_stream_decodes;
    out.lazy_stream_decodes += b.lazy_stream_decodes;
    out.symbol_section_decodes += b.symbol_section_decodes;
    out.read_txns += b.read_txns;
    out.dict_decode_nanos += b.dict_decode_nanos;
    out.lazy_decode_nanos += b.lazy_decode_nanos;
    out.full_decode_nanos += b.full_decode_nanos;
    out.queries += b.queries;
    out.query_nanos += b.query_nanos;
    out.dict_bytes += b.dict_bytes;
    out.symbol_bytes += b.symbol_bytes;
    out.lazy_bytes += b.lazy_bytes;
    out.full_bytes += b.full_bytes;
    out.symbol_decode_nanos += b.symbol_decode_nanos;
    out
}

/// Share of reads that went around the decoded-stream cache, in %. There
/// is no cache until phase 2 (stories 47-48), so every read is a bypass
/// of nothing and the column reads 0.
// TODO(#233 phase 2): report the cache's own bypass counter here.
fn bypass_rate(_stats: &ReadStats) -> f64 {
    0.0
}

/// One table row. The decode share is the median of the readers' own
/// shares; the category split and per-query counts are over all readers.
/// Timing columns read `n/a` when timing is off. `KiB/q` is the encoded
/// bytes decoded per query: dictionary blocks plus streams decoded lazily
/// or whole (symbol sections lie inside the lazy streams, so they are not
/// added again). `store ms/q` is the wall time inside `StoreRead` calls.
fn report(label: &str, readers: &[Reader], wall: Duration) {
    let mut lat: Vec<Duration> = readers.iter().flat_map(|r| r.lat.clone()).collect();
    lat.sort();
    let stats = readers
        .iter()
        .fold(ReadStats::default(), |a, r| add(a, &r.stats));
    let busy: f64 = readers.iter().map(Reader::busy_secs).sum();
    let pct = |nanos: u64| nanos as f64 / 1e9 / busy * 100.0;
    let n = lat.len() as f64;
    let timed = |text: String| {
        if read_stats::timing() {
            text
        } else {
            "n/a".into()
        }
    };
    let share = timed(format!(
        "{:.1}%",
        median(readers.iter().map(Reader::decode_share).collect())
    ));
    let category = |nanos: u64| timed(format!("{:.1}%", pct(nanos)));
    let store_ms = timed(format!(
        "{:.3}",
        stats.query_nanos as f64 / 1e6 / stats.queries.max(1) as f64
    ));
    let kib = (stats.dict_bytes + stats.lazy_bytes + stats.full_bytes) as f64 / 1024.0 / n;
    println!(
        "| {label} | {} | {:.3} | {:.3} | {:.0} | {share} | {} | {} | {} | {} | {store_ms} | {kib:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.1}% |",
        lat.len(),
        percentile(&lat, 0.5),
        percentile(&lat, 0.95),
        n / wall.as_secs_f64(),
        category(stats.dict_decode_nanos),
        category(stats.lazy_decode_nanos),
        category(stats.symbol_decode_nanos),
        category(stats.full_decode_nanos),
        stats.dict_block_decodes as f64 / n,
        stats.dict_strings_decoded as f64 / n,
        stats.lazy_stream_decodes as f64 / n,
        stats.symbol_section_decodes as f64 / n,
        stats.read_txns as f64 / n,
        bypass_rate(&stats),
    );
}

fn header() {
    println!("\n| phase | queries | p50 ms | p95 ms | qps | decode share (median of readers) | dict | lazy | sym | full | store ms/q | KiB/q | dict blocks/q | dict strings/q | lazy/q | sym/q | txns/q | bypass |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
}

/// Run the cold phase in a fresh process (this test binary, running only
/// [`readbench_cold_phase`]), so the redb cache starts empty; print its row.
fn cold_in_fresh_process(db: &Path, cfg: &Config) {
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args([
            "--exact",
            COLD_FN,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(COLD_DB_ENV, db)
        .env("MG_READBENCH_WORKDIR", &cfg.workdir)
        .env("MG_READBENCH_COLD_CACHE", cfg.cold_cache.to_string())
        .output()
        .expect("spawn cold phase");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "cold phase failed: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // libtest prints `test <name> ... ` on the same line before the row.
    let rows: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.find("| cold").map(|i| &l[i..]))
        .collect();
    assert!(!rows.is_empty(), "cold phase printed no row: {stdout}");
    for row in rows {
        println!("{row}");
    }
}

/// One warm run after a warm-up pass; returns its wall time.
fn warm(store: &V2Store, ops: &[Op], reps: usize, timing: bool, print: bool) -> Duration {
    read_stats::set_timing(timing);
    read_passes(store, ops, 1);
    let t = Instant::now();
    let reader = read_passes(store, ops, reps);
    let wall = t.elapsed();
    if print {
        let label = format!("warm x{reps}, timing {}", if timing { "on" } else { "off" });
        report(&label, &[reader], wall);
    }
    wall
}

fn concurrent(store: &V2Store, ops: &[Op], reps: usize, threads: usize) {
    let t = Instant::now();
    let readers: Vec<Reader> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| s.spawn(|| read_passes(store, ops, reps)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("reader thread"))
            .collect()
    });
    report(&format!("{threads} threads x{reps}"), &readers, t.elapsed());
}

/// Warm wall time with timing on over timing off, in %, for several
/// alternating trials: (min, median, max).
fn timing_overhead(store: &V2Store, ops: &[Op], reps: usize) -> (f64, f64, f64) {
    let trials: Vec<f64> = (0..OVERHEAD_TRIALS)
        .map(|_| {
            let off = warm(store, ops, reps, false, false);
            let on = warm(store, ops, reps, true, false);
            (on.as_secs_f64() / off.as_secs_f64() - 1.0) * 100.0
        })
        .collect();
    let min = trials.iter().copied().fold(f64::INFINITY, f64::min);
    let max = trials.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, median(trials), max)
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

/// The numbers in `docs/spikes/read-cache.md`. Not a gate: run it by hand
/// (see the module docs for the `MG_READBENCH_*` inputs).
#[test]
#[ignore = "measurement for docs/spikes/read-cache.md; run with --release --ignored"]
fn measure_reads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::from_env(tmp.path());
    // Indexing runs extractors: give it their stack (#245), not libtest's.
    let (db, workload) = std::thread::scope(|sc| {
        graph_cli::spawn_extract_scoped(sc, "readbench-index", || prepare(&cfg))
            .expect("spawn the index thread")
            .join()
            .expect("index thread")
    });
    let ops = workload.ops();
    println!("workload: {} queries per pass", ops.len());

    header();
    cold_in_fresh_process(&db, &cfg);
    read_stats::set_timing(true);
    let store = open(&db, None);
    warm(&store, &ops, cfg.reps, false, true);
    warm(&store, &ops, cfg.reps, true, true);
    for threads in THREAD_COUNTS {
        concurrent(&store, &ops, cfg.reps, threads);
    }
    let (min, mid, max) = timing_overhead(&store, &ops, cfg.reps);
    let bump = bump_cost_ns();
    println!(
        "\ntiming overhead (warm, on vs off, {OVERHEAD_TRIALS} trials): {min:+.1}% to {max:+.1}%, median {mid:+.1}%; one relaxed counter bump: {bump:.2} ns"
    );
}

/// The cold phase, run by [`measure_reads`] in a child process: a fresh
/// open with a small redb cache, one pass. The OS page cache is still warm
/// (drop it by hand for a cold disk). Does nothing when run on its own.
#[test]
#[ignore = "child process of measure_reads"]
fn readbench_cold_phase() {
    let Some(db) = std::env::var_os(COLD_DB_ENV).map(PathBuf::from) else {
        return;
    };
    let cfg = Config::from_env(db.parent().expect("db dir"));
    let ops = Workload::load(&cfg.workdir.join(WORKLOAD_NAME)).ops();
    read_stats::set_timing(true);
    let store = open(&db, Some(cfg.cold_cache));
    let t = Instant::now();
    let reader = read_passes(&store, &ops, 1);
    let label = format!(
        "cold redb cache, fresh process (OS cache warm), {} KiB",
        cfg.cold_cache >> 10
    );
    report(&label, &[reader], t.elapsed());
}
