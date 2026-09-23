//! Test-only helper for `v2_policy_tests::the_fifty_percent_age_warning_fires_exactly_once_per_snapshot`
//! (issue #58, gap 4). `cargo test`'s default output capture intercepts
//! `eprintln!` from every thread of the test binary (confirmed by direct
//! experiment: even a freshly spawned `std::thread::scope` thread's
//! `eprintln!` output is swallowed unless the whole binary is run with
//! `--nocapture`), so there is no reliable way to observe the 50%-max-age
//! warning's stderr output from *inside* a unit test. Running the scenario
//! in a genuinely separate OS process sidesteps that: this process's real
//! stderr is whatever the parent test's `std::process::Command` inherits or
//! captures via `Command::output()`, unaffected by the parent test binary's
//! own libtest capture.
//!
//! Usage: `snapshot_warn_probe <path to a v2 store file>`. Opens the store,
//! sets a 60ms max snapshot age, ingests one file, takes a snapshot, sleeps
//! past the 50% mark, and issues 5 reads through the same snapshot handle --
//! exactly the scenario whose stderr the parent test inspects.
use graph_store::{Query, Store, V2Store};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: snapshot_warn_probe <path>");
    let mut s = V2Store::open(&path).expect("open");
    s.set_max_snapshot_age(std::time::Duration::from_millis(60));
    s.ingest_file(
        "o",
        "r",
        "x.rs",
        "text",
        &graph_core::Extraction {
            has_errors: false,
            symbols: vec![],
            tokens: vec![graph_core::TokenDecl {
                text: "alpha".into(),
                class: graph_core::TokenClass::Identifier,
                span: graph_core::Span {
                    start: 0,
                    end: 5,
                    start_line: 1,
                    start_col: 1,
                    end_line: 1,
                    end_col: 6,
                },
            }],
        },
    )
    .expect("ingest");

    let snap = s.snapshot().expect("snapshot");
    // Past 50% of the 60ms max age (>=30ms) but still well within it
    // (<60ms), so these reads succeed rather than expiring.
    std::thread::sleep(std::time::Duration::from_millis(35));
    for _ in 0..5 {
        snap.search(&Query::new("alpha")).expect("search");
    }
}
