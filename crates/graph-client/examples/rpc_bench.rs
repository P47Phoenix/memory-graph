//! RPC overhead micro-benchmark (ADR 0003 Q5 / ADR 0004 D1: the 5 ms
//! trigger). The same queries on the same database file, embedded (a
//! `V2Store` in this process) and remote (a `RemoteStore` over loopback
//! gRPC to a server on that file), p50 / p95 over N iterations after a
//! warm-up. Results go in `docs/spikes/rpc-overhead.md`.
//!
//! ```sh
//! # a database of the vendored corpus, indexed by the release CLI
//! for r in testdata/corpus/*/; do
//!   target/release/memory-graph --db /tmp/corpus.redb index --org corpus --repo "$(basename "$r")" "$r"
//! done
//! cargo run --release -p graph-client --example rpc_bench -- /tmp/corpus.redb \
//!   [--iters 500] [--rare TEXT] [--common TEXT]
//! ```
//!
//! The server runs in this process (its own runtime, `graph_server::testing`)
//! on 127.0.0.1: every remote call is a real TCP round trip through tonic,
//! prost and the server's services, without network latency.
use graph_client::{ClientConfig, RemoteStore};
use graph_server::testing::TestServer;
use graph_store::{open_store, Query, Store};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Args {
    db: PathBuf,
    iters: usize,
    rare: String,
    common: String,
}

fn args() -> Args {
    let mut a = std::env::args().skip(1);
    let mut out = Args {
        db: PathBuf::new(),
        iters: 500,
        rare: "Ensure".into(),
        common: "return".into(),
    };
    while let Some(x) = a.next() {
        match x.as_str() {
            "--iters" => out.iters = a.next().and_then(|v| v.parse().ok()).expect("--iters N"),
            "--rare" => out.rare = a.next().expect("--rare TEXT"),
            "--common" => out.common = a.next().expect("--common TEXT"),
            p => out.db = PathBuf::from(p),
        }
    }
    assert!(
        out.db.is_file(),
        "usage: rpc_bench <db.redb> [--iters N] [--rare TEXT] [--common TEXT]"
    );
    out
}

/// p50 and p95 of `iters` timed runs of `f` after `iters / 10` warm-ups.
fn measure(iters: usize, mut f: impl FnMut()) -> (Duration, Duration) {
    for _ in 0..(iters / 10).max(3) {
        f();
    }
    let mut t: Vec<Duration> = (0..iters)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed()
        })
        .collect();
    t.sort();
    let at = |q: f64| t[((t.len() as f64 * q) as usize).min(t.len() - 1)];
    (at(0.50), at(0.95))
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

type Op<'a> = (&'a str, Box<dyn Fn(&dyn Store) + 'a>);

fn main() {
    let a = args();
    let rare = Query::new(&a.rare);
    let mut rare_1000 = Query::new(&a.rare);
    rare_1000.limit = Some(1000);
    let mut common = Query::new(&a.common);
    common.limit = Some(100);
    let ops: Vec<Op> = vec![
        (
            "describe",
            Box::new(|s: &dyn Store| {
                s.describe(None, None).unwrap();
            }),
        ),
        (
            "search rare",
            Box::new(|s: &dyn Store| {
                s.search(&rare).unwrap();
            }),
        ),
        (
            // What the server runs for an unlimited request (its default
            // limit, D1): separates the limit's cost from the RPC's.
            "search rare, limit 1000",
            Box::new(|s: &dyn Store| {
                s.search(&rare_1000).unwrap();
            }),
        ),
        (
            "search common, limit 100",
            Box::new(|s: &dyn Store| {
                s.search(&common).unwrap();
            }),
        ),
    ];

    let embedded = open_store(&a.db, vec![]).expect("open the database");
    let rare_hits = embedded.search(&rare).unwrap().len();
    let common_hits = embedded.search(&common).unwrap().len();
    let mut rows = Vec::new();
    for (name, op) in &ops {
        rows.push((name.to_string(), measure(a.iters, || op(&*embedded))));
    }
    drop(embedded);

    let server = TestServer::start(&a.db, vec![]);
    let remote = RemoteStore::connect(ClientConfig::new(server.endpoint())).expect("connect");
    let ping = measure(a.iters, || {
        remote.health("").unwrap();
    });
    let mut remote_rows = Vec::new();
    for (_, op) in &ops {
        remote_rows.push(measure(a.iters, || op(&remote)));
    }
    drop(remote);
    drop(server);

    println!(
        "db {} ({} MB), {} iterations each; rare `{}` = {} hits, common `{}` = {} hits (limit 100)",
        a.db.display(),
        std::fs::metadata(&a.db).map_or(0, |m| m.len()) >> 20,
        a.iters,
        a.rare,
        rare_hits,
        a.common,
        common_hits
    );
    println!(
        "| operation | embedded p50 | embedded p95 | remote p50 | remote p95 | overhead p50 |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    println!(
        "| ping (Health.Check) | - | - | {} | {} | {} |",
        ms(ping.0),
        ms(ping.1),
        ms(ping.0)
    );
    for ((name, (e50, e95)), (r50, r95)) in rows.iter().zip(&remote_rows) {
        println!(
            "| {name} | {} | {} | {} | {} | {} |",
            ms(*e50),
            ms(*e95),
            ms(*r50),
            ms(*r95),
            ms(r50.saturating_sub(*e50))
        );
    }
    println!("(milliseconds)");
}
