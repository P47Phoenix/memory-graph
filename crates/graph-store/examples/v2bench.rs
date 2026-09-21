//! v1-vs-v2 benchmark for the ADR 0003 story 4 checkpoint (not built by
//! `cargo test`; run with `--release`). Indexes a set of source directories
//! (each direct subdirectory is one repo), optionally replicated N times with
//! rare identifiers renamed (same scaler as `spikes/data-model`), into a v1 and
//! a v2 file, then compares size, ingest time and query latency, and checks
//! that both backends return the same rows.
//!
//! ```sh
//! cargo run --release -p graph-store --example v2bench -- \
//!     <workdir> <copies> <reps> <dir> [<dir>...]
//! ```
use graph_core::TokenClass;
use graph_store::{open_store, Backend, BatchFile, Grain, IndexOptions, Query, Store, SymbolQuery};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

struct Src {
    repo: String,
    path: String,
    src: String,
}

fn walk(d: &Path, root: &Path, repo: &str, out: &mut Vec<Src>) {
    let mut es: Vec<_> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            if p.file_name().unwrap() != ".git" && p.file_name().unwrap() != "target" {
                walk(&p, root, repo, out);
            }
        } else if let Ok(s) = std::fs::read_to_string(&p) {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push(Src {
                repo: repo.into(),
                path: rel,
                src: s,
            });
        }
    }
}

/// Each direct subdirectory of `dir` is a repo.
fn load(dir: &str, out: &mut Vec<Src>) {
    let mut rs: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    rs.sort();
    for r in rs {
        walk(&r, &r, r.file_name().unwrap().to_str().unwrap(), out);
    }
}

fn words(s: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = s.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() && !(b[i].is_ascii_alphabetic() || b[i] == b'_') {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        let st = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        Some((st, i))
    })
}

fn scaled(base: &[Src], copies: usize) -> Vec<Src> {
    let mut freq: HashMap<&str, u32> = HashMap::new();
    for f in base {
        for (a, b) in words(&f.src) {
            *freq.entry(&f.src[a..b]).or_default() += 1;
        }
    }
    let mut out = vec![];
    for k in 0..copies {
        for f in base {
            let src = if k == 0 {
                f.src.clone()
            } else {
                let mut s = String::with_capacity(f.src.len() + 64);
                let mut last = 0;
                for (a, b) in words(&f.src) {
                    let w = &f.src[a..b];
                    if w.len() >= 4 && freq[w] <= 50 {
                        s.push_str(&f.src[last..b]);
                        s.push_str(&k.to_string());
                        last = b;
                    }
                }
                s.push_str(&f.src[last..]);
                s
            };
            out.push(Src {
                repo: format!("{}-{k}", f.repo),
                path: f.path.clone(),
                src,
            });
        }
    }
    out
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn open(b: Backend, p: &Path) -> Box<dyn Store> {
    open_store(b, p, vec![Box::new(graph_lang_rust::RustExtractor)]).unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let work = Path::new(&a[1]);
    let copies: usize = a[2].parse().unwrap();
    let reps: usize = a[3].parse().unwrap();
    let mut base = vec![];
    for d in &a[4..] {
        load(d, &mut base);
    }
    let set = scaled(&base, copies);
    let mut by: BTreeMap<&str, Vec<&Src>> = BTreeMap::new();
    for f in &set {
        by.entry(&f.repo).or_default().push(f);
    }
    let org_of = |repo: &str| {
        format!(
            "org{}",
            repo.rsplit('-')
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap_or(0)
                % 10
        )
    };
    let mut stores: Vec<(&str, Box<dyn Store>, std::path::PathBuf)> = vec![];
    for (name, b) in [("v1", Backend::Redb), ("v2", Backend::RedbV2)] {
        let p = work.join(format!("{name}.redb"));
        let _ = std::fs::remove_file(&p);
        let s = open(b, &p);
        let t = Instant::now();
        let (mut nt, mut nf, mut ns) = (0, 0, 0);
        for (repo, fs) in &by {
            let bf: Vec<BatchFile> = fs
                .iter()
                .map(|f| BatchFile {
                    path: &f.path,
                    bytes: f.src.as_bytes(),
                    language: None,
                    origin: Some("directory"),
                })
                .collect();
            for r in s
                .index_batch(&org_of(repo), repo, &bf, IndexOptions::default())
                .unwrap()
            {
                let st = r.unwrap();
                nt += st.tokens;
                ns += st.symbols;
                nf += 1;
            }
        }
        let el = t.elapsed().as_secs_f64();
        let sz = std::fs::metadata(&p).unwrap().len();
        println!(
            "{name}: files={nf} tokens={nt} symbols={ns} ingest_s={el:.2} db_MiB={:.1} B/token={:.1}",
            sz as f64 / 1048576.0,
            sz as f64 / nt as f64
        );
        stores.push((name, s, p));
    }
    // Query set. Terms are picked from the base data so they exist at any scale.
    let q = |t: &str, g: Grain| {
        let mut q = Query::new(t);
        q.grain = g;
        q
    };
    let mut queries: Vec<(String, Query)> = vec![];
    for (label, t) in [
        ("common punct `(`", "("),
        ("keyword `self`", "self"),
        ("ident `new`", "new"),
        ("ident `Result`", "Result"),
    ] {
        for (g, gn) in [
            (Grain::Token, "token"),
            (Grain::Symbol, "symbol"),
            (Grain::File, "file"),
            (Grain::Repo, "repo"),
            (Grain::Org, "org"),
        ] {
            queries.push((format!("{label} {gn}"), q(t, g)));
        }
    }
    let mut c = q("new", Grain::Token);
    c.class = Some(TokenClass::Identifier);
    queries.push(("`new` token +class=identifier".into(), c));
    let mut c = q("new", Grain::File);
    c.class = Some(TokenClass::Identifier);
    queries.push((
        "`new` file +class=identifier (stream walk, no rows)".into(),
        c,
    ));
    let mut c = q("(", Grain::Token);
    c.limit = Some(100);
    queries.push(("`(` token --limit 100".into(), c));
    let mut sq: Vec<(String, SymbolQuery)> = vec![];
    for (l, p) in [
        ("symbols `new`", "new"),
        ("symbols `new*`", "new*"),
        ("symbols `*`", "*"),
        ("symbols `fmt`", "fmt"),
    ] {
        sq.push((l.into(), SymbolQuery::new(p)));
    }
    let mut s = SymbolQuery::new("*");
    s.limit = Some(100);
    sq.push(("symbols `*` --limit 100".into(), s));
    println!("\n| query | v1 p50 ms | v2 p50 ms | v2/v1 | rows equal |");
    println!("|---|---|---|---|---|");
    let time = |s: &dyn Store, f: &dyn Fn(&dyn Store) -> usize| {
        let mut v = vec![];
        let mut n = 0;
        for _ in 0..reps {
            let t = Instant::now();
            n = f(s);
            v.push(ms(t));
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], n)
    };
    for (label, qq) in &queries {
        let f = |s: &dyn Store| s.search(qq).unwrap().len();
        let (t1, n1) = time(&*stores[0].1, &f);
        let (t2, n2) = time(&*stores[1].1, &f);
        let same = stores[0].1.search(qq).unwrap() == stores[1].1.search(qq).unwrap();
        println!(
            "| {label} ({n1} rows) | {t1:.1} | {t2:.1} | {:.2}x | {} |",
            t2 / t1,
            same && n1 == n2
        );
    }
    for (label, qq) in &sq {
        let f = |s: &dyn Store| s.search_symbols(qq).unwrap().len();
        let (t1, n1) = time(&*stores[0].1, &f);
        let (t2, n2) = time(&*stores[1].1, &f);
        let same =
            stores[0].1.search_symbols(qq).unwrap() == stores[1].1.search_symbols(qq).unwrap();
        println!(
            "| {label} ({n1} rows) | {t1:.1} | {t2:.1} | {:.2}x | {} |",
            t2 / t1,
            same && n1 == n2
        );
    }
    let paths: Vec<_> = stores.iter().map(|s| (s.0, s.2.clone())).collect();
    drop(stores);
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .map(str::to_string)
        })
        .unwrap_or_default();
    println!("\npeak RSS of this process (both stores, queries): {rss}");
    for (name, p) in paths {
        let before = std::fs::metadata(&p).unwrap().len();
        let mut db = redb::Database::open(&p).unwrap();
        let mut rounds = 0;
        while db.compact().unwrap() && rounds < 20 {
            rounds += 1;
        }
        drop(db);
        let after = std::fs::metadata(&p).unwrap().len();
        println!(
            "{name}: file {:.1} MiB, after redb compaction {:.1} MiB",
            before as f64 / 1048576.0,
            after as f64 / 1048576.0
        );
    }
}
