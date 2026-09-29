//! Scanner robustness across every shipped extractor (#133): long
//! unbalanced delimiter runs stay linear (closers are found with
//! `graph_core::scan::close_table`, not one `matching_close` scan per
//! opener), and nothing overflows the indexer's 2 MB stack.
use std::time::{Duration, Instant};

fn time_extract(ex: &dyn graph_core::Extractor, src: String) -> Duration {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn_scoped(s, move || {
                let start = Instant::now();
                ex.extract(&src);
                start.elapsed()
            })
            .unwrap()
            .join()
            .unwrap()
    })
}

/// 200k unbalanced openers. Linear scanners take well under a second here
/// even in debug; the quadratic path took tens of seconds at half this size
/// in release, so a generous bound separates the two without being flaky on
/// a loaded CI runner. (A doubling-ratio check was tried and was too noisy.)
#[test]
fn unbalanced_delimiter_runs_are_linear() {
    let n = 200_000;
    for ex in graph_cli::shipped_extractors() {
        for open in ["(", "[", "{"] {
            let took = time_extract(ex.as_ref(), open.repeat(n));
            let lang = ex.language();
            assert!(took < Duration::from_secs(20), "{lang} {open}: {took:?}");
        }
    }
}
