# Spike: pure-Rust embedded storage (story 2)

**TL;DR:** `redb` works (0.57 s to index 99 k tokens) at ~690 B/token; `fjall` was not benchmarked. The data-model follow-up is in [data-model.md](data-model.md).

**Scope actually done:** `redb` 2.x only, via the real store (`crates/graph-store`). `fjall` and other stores were **not** benchmarked; that comparison remains open (see ADR 0001).

## Measurements (release build, one file, 99,000 token nodes, 9,000 lines)
| Operation | Result |
|---|---|
| Initial index (write) | 0.57 s |
| Re-index same file (delete subtree + rewrite) | 0.85 s |
| Search by token text (`foo`, token/file grain) | ~6 ms wall, incl. process start |
| On-disk size | 67.9 MB (~690 B/token) |

Caveats: nodes are JSON-encoded, which dominates size. A binary encoding (postcard) would likely cut it several-fold. Single-writer, one transaction per file.

## Assessment
- redb: pure Rust, in-process, ACID (MVCC, single writer), Apache-2.0/MIT, actively maintained; file lock gives the "already open" error used by story 5.
- Estimates: story 4 and 5 done inside the MVP; story 18 (benchmarks) should add a size/encoding pass and a fjall comparison.
