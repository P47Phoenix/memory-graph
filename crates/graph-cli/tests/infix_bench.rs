//! ADR 0011 D7: infix symbol search p95 on a large index, run by hand:
//!
//! ```sh
//! MG_INFIX_DB=D:/tmp/a5-readbench2/readbench.redb \
//! MG_INFIX_NEEDLES=get,parse,widget,qqxzj \
//! cargo test --release -p graph-cli --test infix_bench measure_infix -- --ignored --nocapture
//! ```
//!
//! For each needle and case mode: the hit count (no limit), one cold query
//! (reported apart), 10 discarded warm-ups, then 200 timed queries with
//! limit 50 and offset 0; prints p50, p95 and max. Embedded only.
use graph_store::{StoreRead, SymbolQuery, V2Store};
use std::time::{Duration, Instant};

#[test]
#[ignore = "manual benchmark: needs MG_INFIX_DB"]
fn measure_infix() {
    let db = std::env::var("MG_INFIX_DB").expect("MG_INFIX_DB");
    let needles = std::env::var("MG_INFIX_NEEDLES").unwrap_or_else(|_| "get".into());
    let opened = Instant::now();
    let s = V2Store::open(&db).expect("open");
    eprintln!("open {:?}", opened.elapsed());
    for needle in needles.split(',') {
        for exact in [false, true] {
            // A needle with its own `*` is used as written (a prefix baseline).
            let pattern = if needle.contains('*') {
                needle.to_string()
            } else {
                format!("*{needle}*")
            };
            let mut q = SymbolQuery::new(pattern.as_str());
            q.exact_case = exact;
            let t = Instant::now();
            let first = s.search_symbols(&{
                let mut q = q.clone();
                q.limit = Some(50);
                q
            });
            let cold = t.elapsed();
            let first = first.expect("query").len();
            let hits = s.search_symbols(&q).expect("query").len();
            q.limit = Some(50);
            for _ in 0..10 {
                std::hint::black_box(s.search_symbols(&q).unwrap());
            }
            let mut t: Vec<Duration> = (0..200)
                .map(|_| {
                    let start = Instant::now();
                    std::hint::black_box(s.search_symbols(&q).unwrap());
                    start.elapsed()
                })
                .collect();
            t.sort();
            println!(
                "{pattern:<16} exact_case={exact:<5} hits={hits:<8} page={first:<3} cold={:>8.2}ms p50={:>8.2}ms p95={:>8.2}ms max={:>8.2}ms",
                cold.as_secs_f64() * 1e3,
                t[100].as_secs_f64() * 1e3,
                t[189].as_secs_f64() * 1e3,
                t[199].as_secs_f64() * 1e3,
            );
        }
    }
}
