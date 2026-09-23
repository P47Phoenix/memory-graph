//! 100 M-token-scale measurement for block-encoded count postings (ADR 0003
//! story 6, decision D1). Generates a synthetic, Zipf-ish term-frequency
//! corpus reaching the requested total token-occurrence count and measures
//! total encoded posting bytes for the pre-story-6 flat delta-varint format
//! versus the new block-encoded format (`codec::encode_posting`,
//! `codec::POSTING_BLOCK`-sized self-contained blocks).
//!
//! This runs purely against the codec (no redb, no file I/O) so it can
//! actually reach 100 M occurrences in a reasonable time; the store-level
//! differential/round-trip tests that prove the encoding is behavior-preserving
//! live in `crates/graph-store/src/v2_policy_tests.rs` and `codec.rs`.
//!
//! `cargo run --release -p graph-store --example block_postings_100m [-- <target_tokens>]`
//! Default target is 100_000_000.

use graph_store::{encode_posting, POSTING_BLOCK};

/// Frozen copy of the pre-story-6 flat delta-varint posting encoding (one
/// running delta chain across the whole posting, no blocks), for comparison.
fn old_encode_posting(ords: &[usize]) -> Vec<u8> {
    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            } else {
                out.push(b | 0x80);
            }
        }
    }
    let mut out = Vec::with_capacity(ords.len() + 2);
    put_varint(&mut out, ords.len() as u64);
    let mut prev = 0;
    for &o in ords {
        put_varint(&mut out, (o - prev) as u64);
        prev = o;
    }
    out
}

/// xorshift64*, deterministic and dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn main() {
    let target: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000_000);

    let mut rng = Rng(0x9E3779B97F4A7C15);
    let start = std::time::Instant::now();

    let mut total_tokens: u64 = 0;
    let mut old_bytes: u64 = 0;
    let mut new_bytes: u64 = 0;
    let mut n_postings: u64 = 0;

    // Zipf-ish shapes for posting list lengths (occurrences of one term in
    // one file), from singleton hits up to very hot identifiers repeated
    // tens of thousands of times in a single large generated file. Weights
    // are chosen so most postings are short (realistic long tail) while a
    // handful of huge ones exercise many blocks.
    let shapes: &[(usize, u64)] = &[
        (1, 40),
        (2, 20),
        (5, 15),
        (20, 10),
        (100, 8),
        (1_000, 4),
        (20_000, 2),
        (200_000, 1),
    ];
    let weight_sum: u64 = shapes.iter().map(|(_, w)| *w).sum();

    while total_tokens < target {
        let pick = rng.next() % weight_sum;
        let mut acc = 0u64;
        let mut len = shapes[0].0;
        for &(l, w) in shapes {
            acc += w;
            if pick < acc {
                len = l;
                break;
            }
        }
        let mut ords = Vec::with_capacity(len);
        let mut at = 0usize;
        for _ in 0..len {
            at += 1 + (rng.next() % 7) as usize;
            ords.push(at);
        }
        old_bytes += old_encode_posting(&ords).len() as u64;
        new_bytes += encode_posting(&ords).len() as u64;
        total_tokens += len as u64;
        n_postings += 1;
    }

    let elapsed = start.elapsed();
    let growth = (new_bytes as f64 - old_bytes as f64) / old_bytes as f64 * 100.0;
    println!("POSTING_BLOCK = {POSTING_BLOCK}");
    println!("target tokens = {target}, actual = {total_tokens}, postings = {n_postings}");
    println!(
        "flat (pre-story-6):   {old_bytes} bytes total, {:.4} bytes/token",
        old_bytes as f64 / total_tokens as f64
    );
    println!(
        "block-encoded (story 6): {new_bytes} bytes total, {:.4} bytes/token",
        new_bytes as f64 / total_tokens as f64
    );
    println!("growth vs flat: {growth:.3}%");
    println!("elapsed: {:.2}s", elapsed.as_secs_f64());
}
