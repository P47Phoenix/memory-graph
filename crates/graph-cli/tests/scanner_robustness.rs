//! Scanner robustness across every shipped extractor (#133): long
//! unbalanced delimiter runs stay linear (closers are found with
//! `graph_core::scan::close_table`, not one `matching_close` scan per
//! opener), and nothing overflows the indexer's 2 MB stack.
use std::time::{Duration, Instant};

#[test]
fn unbalanced_delimiter_runs_are_fast() {
    for ex in graph_cli::shipped_extractors() {
        for open in ["(", "[", "{"] {
            let src = open.repeat(100_000);
            let lang = ex.language().to_string();
            let start = Instant::now();
            let ex_ref = &ex;
            std::thread::scope(|s| {
                std::thread::Builder::new()
                    .stack_size(2 << 20)
                    .spawn_scoped(s, move || ex_ref.extract(&src))
                    .unwrap()
                    .join()
                    .unwrap()
            });
            let took = start.elapsed();
            // Generous: debug builds do this in well under a second; the
            // quadratic path took ~22 s.
            assert!(took < Duration::from_secs(2), "{lang} {open}: {took:?}");
        }
    }
}
