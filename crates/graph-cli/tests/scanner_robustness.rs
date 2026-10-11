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

/// Rust nested too deep for `syn` (#245) is stored tokens only with a
/// warning, on a 2 MiB thread, instead of overflowing the stack. The shapes
/// are what crashed a release build on a 2 MiB thread: the Peano numerals
/// of rustc's `tests/ui/parser/survive-peano-lesson-queue.rs` (2005 nested
/// calls, synthesized here), and long prefix-operator, binary-operator and
/// method chains.
#[test]
fn rust_nested_too_deep_is_stored_tokens_only_with_a_warning() {
    let n = 100_000;
    let peano = format!(
        "fn two() -> u64 {{ {}0{} }}\n",
        "S(".repeat(2005),
        ")".repeat(2005)
    );
    let shapes = [
        ("peano.rs", peano),
        (
            "not.rs",
            format!("fn f() -> bool {{ {}true }}\n", "!".repeat(n)),
        ),
        (
            "plus.rs",
            format!("fn f() -> u32 {{ {} }}\n", vec!["1"; n].join(" + ")),
        ),
        ("calls.rs", format!("fn f() {{ x{}; }}\n", ".a()".repeat(n))),
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        graph_store::open_store(&dir.path().join("g.redb"), graph_cli::shipped_extractors())
            .expect("open store");
    let rust = graph_cli::shipped_extractors()
        .into_iter()
        .find(|e| e.language() == "rust")
        .expect("the rust extractor")
        .version();
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn_scoped(s, || {
                for (path, src) in &shapes {
                    let st = store
                        .index_bytes("o", "r", path, src.as_bytes(), None)
                        .expect("index");
                    assert_eq!((st.language.as_str(), st.symbols), ("rust", 0), "{path}");
                    assert!(st.tokens > 0, "{path}");
                    let w = st.span_warning.as_deref().expect("a warning");
                    assert!(
                        w.contains(&rust) && w.contains("skipped parsing"),
                        "{path}: {w}"
                    );
                }
                // Ordinary Rust next to them still gets its symbols.
                let st = store
                    .index_bytes("o", "r", "ok.rs", b"fn ok() {}", None)
                    .expect("index");
                assert_eq!((st.symbols, st.span_warning), (1, None));
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow")
    });
}

/// The shapes QA found past the pre-scan (#245): keyword-led, postfix,
/// type and closure nesting that overflowed a 16 MiB thread (the first six)
/// or a 2 MiB one (the rest). The sizes are fixed literals so they do not
/// move with the extractor's limits. On a 2 MiB thread, each must index
/// without a crash and either keep its symbols or carry a warning.
#[test]
fn rust_nesting_past_the_pre_scan_never_overflows() {
    let e = |body: String| format!("fn f() {{ loop {{ {body}; }} }}\n");
    let shapes = [
        ("break.rs", e(format!("{}1", "break ".repeat(5_000)))),
        ("return.rs", e(format!("{}1", "return ".repeat(20_000)))),
        (
            "fntype.rs",
            format!("type T = {}u8;\n", "fn() -> ".repeat(5_000)),
        ),
        ("as.rs", e(format!("x{}", " as u8".repeat(100_000)))),
        ("index.rs", e(format!("x{}", "[0]".repeat(100_000)))),
        (
            "elseif.rs",
            e(format!("if a {{}} {}", "else if a {} ".repeat(1_000_000))),
        ),
        ("closure.rs", e(format!("{}1", "|a| ".repeat(511)))),
        (
            "refs.rs",
            format!("type T<'a> = {}u8;\n", "&'a ".repeat(1_000)),
        ),
        (
            "elseif2.rs",
            e(format!("if a {{}} {}", "else if a {} ".repeat(50_000))),
        ),
        ("return2.rs", e(format!("{}1", "return ".repeat(5_000)))),
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        graph_store::open_store(&dir.path().join("g.redb"), graph_cli::shipped_extractors())
            .expect("open store");
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn_scoped(s, || {
                for (path, src) in &shapes {
                    let st = store
                        .index_bytes("o", "r", path, src.as_bytes(), None)
                        .expect("index");
                    assert!(st.tokens > 0, "{path}");
                    assert!(
                        st.symbols > 0 || st.span_warning.is_some(),
                        "{path}: no symbols and no warning: {st:?}"
                    );
                }
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow")
    });
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

/// A Rust file whose one top-level item is just under the parse stack cap
/// (#254): `syn` gets a dedicated thread of nearly `MAX_PARSE_STACK`, and the
/// `break` chain (the deepest shape per token measured, `stack.rs`) makes the
/// parse touch as much of it as any input can. `salt` varies the names so
/// two files differ. Sized for the build the test runs in (the debug cap is
/// smaller than release's).
#[cfg(feature = "lang-rust")]
fn near_cap_rust_source(salt: usize) -> String {
    use graph_lang_rust::{MAX_PARSE_STACK, STACK_BASE, STACK_PER_TOKEN};
    let cap = (MAX_PARSE_STACK - STACK_BASE) / STACK_PER_TOKEN;
    // `fn fN ( ) { loop { <breaks> 1 ; } }` is 12 tokens besides the breaks.
    let breaks = cap - 12 - 16;
    format!(
        "fn f{salt}() {{ loop {{ {}1; }} }}\n",
        "break ".repeat(breaks)
    )
}

/// Run the CLI's `index --json --stats` and return its summary.
#[cfg(feature = "lang-rust")]
fn index_stats(db: &std::path::Path, dir: &std::path::Path, extra: &[&str]) -> serde_json::Value {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .arg("--db")
        .arg(db)
        .args(["index", "--org", "o", "--repo", "r", "--json", "--stats"])
        .args(extra)
        .arg(dir)
        .output()
        .expect("run memory-graph");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

/// Two near-cap files indexed in parallel under `--memory 1GiB` (#254):
/// each parse reserves the stack it may touch from the ingest budget before
/// it starts its thread, so the reserved parse stacks plus every other
/// budgeted byte never exceed the 1 GiB (the second file waits for the
/// first), and both files still get their symbols.
#[cfg(feature = "lang-rust")]
#[test]
fn parse_stack_reservations_stay_within_memory_budget() {
    let d = tempfile::tempdir().expect("tempdir");
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for i in 0..2 {
        std::fs::write(src.join(format!("near{i}.rs")), near_cap_rust_source(i)).unwrap();
    }
    let sum = index_stats(
        &d.path().join("g.redb"),
        &src,
        &["--memory", "1GiB", "--jobs", "2"],
    );
    let stats = &sum["stats"];
    let cap = stats["memory_budget"].as_u64().unwrap();
    let peak = stats["peak_in_flight"].as_u64().unwrap();
    let scratch = stats["memory"]["scratch_peak"].as_u64().unwrap();
    assert_eq!(cap, 1 << 30, "{sum}");
    assert!(peak <= cap, "peak {peak} > budget {cap}: {sum}");
    // One stack of nearly 2 GiB / 3 was reserved, never both at once.
    let one = (graph_lang_rust::MAX_PARSE_STACK / 3) as u64;
    assert!(
        scratch > one / 2 && scratch <= one,
        "scratch peak {scratch}: {sum}"
    );
    assert_eq!(sum["files"], 2, "{sum}");
    assert_eq!(sum["symbols"], 2, "both parsed: {sum}");
}

/// The store does not depend on the budget or the jobs (#254): a run
/// that reserves near-cap parse stacks under a small `--memory` with one
/// job answers every query like one under a large `--memory` with four.
#[cfg(feature = "lang-rust")]
#[test]
fn parse_stack_budget_does_not_change_the_store() {
    let d = tempfile::tempdir().expect("tempdir");
    let src = d.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for i in 0..2 {
        std::fs::write(src.join(format!("near{i}.rs")), near_cap_rust_source(i)).unwrap();
    }
    std::fs::write(src.join("ok.rs"), "fn ok() {}\nstruct S;\n").unwrap();
    let small = d.path().join("small.redb");
    let large = d.path().join("large.redb");
    index_stats(&small, &src, &["--memory", "1MiB", "--jobs", "1"]);
    index_stats(&large, &src, &["--memory", "4GiB", "--jobs", "4"]);
    let a = graph_store::open_store(&small, graph_cli::shipped_extractors()).expect("open");
    let b = graph_store::open_store(&large, graph_cli::shipped_extractors()).expect("open");
    graph_store::conformance::run_differential(&*a, &*b);
}
