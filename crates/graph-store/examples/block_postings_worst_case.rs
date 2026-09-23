//! Worst-case block-encoding overhead measurement for block-encoded count
//! postings (ADR 0003 story 6, decision D1; issue #51 follow-up to
//! `block_postings_100m.rs`).
//!
//! `block_postings_100m.rs` measures average-case growth on a realistic,
//! Zipf-ish corpus and finds ~4.7% growth. This example instead
//! characterizes the *upper bound* on block-encoding overhead by combining
//! two ingredients that each make the fixed per-block overhead
//! (`varint(block_len) + varint(block_bytes)`, ADR 0003 story 6) as large a
//! share of the total as possible:
//!
//! 1. **Minimal delta-compressibility inside the content itself**: gaps are
//!    kept tiny (every gap fits in a single varint byte) rather than a real
//!    tokenizer's more varied step sizes, so the block-encoded form's per-item
//!    content cost is at its theoretical floor and the fixed per-block header
//!    bytes dominate the difference from the flat encoding -- growth is
//!    driven purely by how many block boundaries a posting crosses, not by
//!    how expensive its content is to encode either way (a genuinely huge,
//!    maximally-scattered-ordinal posting was tried too and, as expected,
//!    performed *better* than average case: large gaps make the content
//!    itself dominate total size, diluting the fixed per-block overhead
//!    rather than amplifying it).
//! 2. **Maximally-fragmented block boundaries**: every posting has length
//!    `POSTING_BLOCK + 1` (one full block plus a single-entry straggler
//!    block) -- the worst possible ratio of fixed per-block overhead to
//!    content for any posting length, since a longer straggler-having length
//!    (e.g. `2 * POSTING_BLOCK + 1`) amortizes the same one wasted header
//!    over more full-block content and so grows less.
//!
//! This runs purely against the codec (no redb, no file I/O), matching
//! `block_postings_100m.rs`'s harness style.
//!
//! `cargo run --release -p graph-store --example block_postings_worst_case [-- <target_tokens>]`
//! Default target is 10_000_000 (worst-case postings are far more expensive
//! per token to generate/encode than the average-case example's, so the
//! default target is smaller to keep run time reasonable).

use graph_store::{encode_posting, POSTING_BLOCK};

/// Frozen copy of the pre-story-6 flat delta-varint posting encoding (one
/// running delta chain across the whole posting, no blocks), for comparison.
/// Identical to the copy in `block_postings_100m.rs`.
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
        .unwrap_or(10_000_000);

    let mut rng = Rng(0xD1B54A32D192ED03);
    let start = std::time::Instant::now();

    let mut total_tokens: u64 = 0;
    let mut old_bytes: u64 = 0;
    let mut new_bytes: u64 = 0;
    let mut n_postings: u64 = 0;

    // Every posting uses length POSTING_BLOCK + 1: one full block plus a
    // single-entry straggler block, so each posting's last block is a
    // single-entry straggler -- the worst ratio of per-block overhead to
    // block content, since it maximizes the fixed per-block overhead
    // relative to content for any posting length (see module doc).
    let b = POSTING_BLOCK;
    let lengths: &[usize] = &[b + 1];

    while total_tokens < target {
        let len = lengths[(rng.next() as usize) % lengths.len()];
        let mut ords = Vec::with_capacity(len);
        let mut at = 0usize;
        for _ in 0..len {
            // Tiny, single-varint-byte gaps: content cost is at its floor so
            // the fixed per-block header bytes are the largest possible
            // share of the total (see module doc).
            at += 1 + (rng.next() % 3) as usize;
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
    println!("worst-case growth vs flat: {growth:.3}%");
    println!("elapsed: {:.2}s", elapsed.as_secs_f64());
}
