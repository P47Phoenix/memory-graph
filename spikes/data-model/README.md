# Data-model spike (ADR 0003)

Throwaway prototype and raw data behind [docs/spikes/data-model.md](../../docs/spikes/data-model.md) and [ADR 0003](../../docs/adr/0003-data-model.md). It is **not** part of the cargo workspace and must not be added to it.

TL;DR: `spike.rs` builds a real `graph-store` index of `testdata/corpus` (and scaled copies), and compares the current JSON-node model (A), binary nodes (E) and a dictionary + stream + postings prototype (P).

## Contents
- `spike.rs`: the single-file example (~500 lines). Commands: `freq | verify | ingest | stats | bench | mutate | search1`.
- `logs/`: raw output of the 1 M and 10 M ingest, stats and bench runs (`ingest_*`, `stats_*`, `bench_*`, `out_bench_*`). The corpus-level (1x) search table and the process-level CLI/cold-cache timings in the spike doc were **not retained as logs**; only the 1x ingest and stats logs exist.

## How to run
It needs the `graph-store` crate as of commit **`293bb2a`** (main before PR #8; the spike calls `Store::index_batch` and `BatchFile`, which exist there; it may not compile against later main without edits) and two dev-dependencies (`sha2` is already a workspace dependency of `graph-store`; only `miniz_oxide` is new and deliberately not in the workspace).

```sh
git worktree add /tmp/dm-spike 293bb2a
cd /tmp/dm-spike
mkdir -p crates/graph-store/examples
cp /path/to/repo/spikes/data-model/spike.rs crates/graph-store/examples/spike.rs
cat >> crates/graph-store/Cargo.toml <<'TOML'

[dev-dependencies.sha2]
version = "0.10"
[dev-dependencies.miniz_oxide]
version = "0.8"
TOML
cargo run --release -p graph-store --example spike -- freq
```

Measurements in the spike doc were taken on an Intel Core Ultra 9 275HX, 30 GB RAM, NVMe, btrfs, rustc 1.94.1, redb 2.6.3. `python3 scripts/check-no-c-deps.py` passed with both dev-dependencies. The scaled sets (4x, 41x) are produced by the spike's own scaler.
