//! Issue #162: how long one 100-small-file batch takes to apply, split into
//! its prepare (decode, fingerprint, extract) and commit halves, on an empty
//! store and on one that already holds the large files of
//! `payload_too_large_backlog_and_a_20_mib_file_replicate` (six 1.5 MiB
//! comment-only files and a 20 MiB one). The commit goes through
//! `index_prepared_marked`, as a Raft apply does. See
//! `docs/spikes/batch-apply.md`.
//!
//! `cargo run --release -p graph-store --example batch_apply -- [rounds]`
use graph_lang_rust::RustExtractor;
use graph_store::{BatchFile, IndexOptions, RaftMarker, Store, V2Store};
use std::time::{Duration, Instant};

fn comment_js(bytes: usize, fill: u8) -> Vec<u8> {
    let mut v = b"/*".to_vec();
    v.extend(std::iter::repeat_n(b'a' + fill % 26, bytes));
    v.extend_from_slice(b"*/\nfunction f() { return 1; }\n");
    v
}

fn small(i: usize, round: usize) -> (String, Vec<u8>) {
    (
        format!("src/f{i}.rs"),
        format!("fn f{i}() -> u32 {{ {} }}\n", i + round * 1000).into_bytes(),
    )
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .map_or(5, |r| r.parse().expect("rounds"));
    for big in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = V2Store::open(dir.path().join("g.redb")).unwrap();
        s.register(Box::new(RustExtractor));
        let mut index = 1u64;
        let mut marker = || {
            index += 1;
            RaftMarker {
                term: 1,
                index,
                node_id: 1,
            }
        };
        if big {
            let t = Instant::now();
            for i in 0..std::env::var("BA_SIX").map_or(6u8, |v| v.parse().unwrap()) {
                s.index_bytes(
                    "o",
                    "big",
                    &format!("m{i}.js"),
                    &comment_js(1536 << 10, i),
                    None,
                )
                .unwrap();
            }
            s.index_bytes(
                "o",
                "big",
                "huge.js",
                &comment_js(
                    std::env::var("BA_HUGE_KIB")
                        .map_or(20 << 20, |v| v.parse::<usize>().unwrap() << 10),
                    7,
                ),
                None,
            )
            .unwrap();
            println!("large files (29 MiB) indexed in {:.0} ms", ms(t.elapsed()));
        }
        for round in 0..rounds {
            let srcs: Vec<_> = (0..100).map(|i| small(i, round)).collect();
            let files: Vec<BatchFile<'_>> = srcs
                .iter()
                .map(|(p, b)| BatchFile {
                    path: p,
                    bytes: b,
                    origin: Some("directory"),
                    ..Default::default()
                })
                .collect();
            let opts = IndexOptions::default();
            let t = Instant::now();
            let snap = s.fingerprint_snapshot("o", "r").unwrap();
            let prepared: Vec<_> = files
                .iter()
                .map(|f| s.prepare_with("o", "r", f, opts, &snap).unwrap())
                .collect();
            let prep = t.elapsed();
            let t = Instant::now();
            let out = s
                .index_prepared_marked("o", "r", prepared, opts, marker(), None)
                .unwrap();
            let commit = t.elapsed();
            assert!(out.iter().all(Result::is_ok));
            if std::env::var_os("BATCH_APPLY_SPLIT").is_some() {
                let t = Instant::now();
                s.index_prepared_marked("o", "r", Vec::new(), opts, marker(), None)
                    .unwrap();
                let empty = t.elapsed();
                let one = small(500 + round, round);
                let t = Instant::now();
                s.index_bytes("o", "r", &one.0, &one.1, None).unwrap();
                println!(
                    "  empty marked batch {:.1} ms, one-file unmarked index {:.1} ms",
                    ms(empty),
                    ms(t.elapsed())
                );
            }
            println!(
                "{} round {round}: prepare {:.1} ms, commit {:.1} ms, total {:.1} ms",
                if big {
                    "with large files"
                } else {
                    "empty store     "
                },
                ms(prep),
                ms(commit),
                ms(prep + commit)
            );
        }
    }
}
