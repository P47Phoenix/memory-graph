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

/// Long runs of other openers that never close, each repeated 25k times
/// (#142, #146): keyword blocks (`keyword_close_table`), generic lists and
/// type aliases (`NestedEnds`, `TypeEnds`), and declarations with no
/// terminator that made the C-family scanners resynchronize one token at a
/// time, each resync scanning to the end of the file. Before the fix these
/// took minutes in release (Elixir `def f do` x 40k: 99.5 s; TypeScript
/// `type a = (` x 40k: 65.7 s; C# `<div>` x 40k: 23 s); linear scanners
/// take milliseconds per shape even in debug. Every extractor gets every
/// shape, so a quadratic path in any of them shows up here too.
///
/// Why an absolute bound at 25k rather than a doubling ratio: the ratio
/// check was tried for #133 and was too noisy on shared runners. At 25k a
/// linear shape takes ~0.1 s in debug, while the quadratic ones above take
/// 4-40 s at 25k even in release, so the 5 s bound separates them widely
/// and the whole test stays short in CI's debug run.
#[test]
fn unclosed_keyword_and_declaration_runs_are_linear() {
    let n = 25_000;
    let shapes = [
        "def f do\n",
        "class A ",
        "class A extends B<",
        "abstract class A ",
        "if x; then\n",
        "function f() {\n",
        "type a = (",
        "type a = <",
        "type a<",
        "template <",
        "a<",
        "x = ",
        "struct a {} ",
        "<div>",
        "\"",
        "`",
        "/*",
        "if ",
        "case ",
        "<%",
    ];
    for ex in graph_cli::shipped_extractors() {
        let lang = ex.language().to_string();
        for shape in shapes {
            let took = time_extract(ex.as_ref(), shape.repeat(n));
            assert!(took < Duration::from_secs(5), "{lang} {shape:?}: {took:?}");
        }
    }
}
