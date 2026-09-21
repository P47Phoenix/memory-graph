# Daemon and locking spike

Throwaway code and raw logs behind [docs/spikes/daemon-and-locking.md](../../docs/spikes/daemon-and-locking.md) (S1 socket round trip, S2 two-process lock behaviour, S4 reflink/copy cost). It is **not** part of the cargo workspace and must not be added to it: `Cargo.toml` has its own empty `[workspace]` table, so cargo treats it as a separate root. The pure-Rust gate (`scripts/check-no-c-deps.py`) only inspects the root workspace via `cargo metadata`, so it does not see this crate; it depends only on the workspace crates (by path) plus `serde` and `serde_json`, no C dependencies.

## Contents
- `src/main.rs`: one binary. Commands: `hold`, `try`, `retry`, `storm-worker`, `search-once` (S2); `serve`, `bench` (S1). Contains a JSON codec and a hand-written compact binary codec.
- `run_s1.sh`: in-process versus socket benchmark for one database (uses three reflink copies because redb locks the file).
- `run_s2.sh`: lock-hold, second-process, retry-policy and contention scenarios (set `ONLY=36` to run only some numbered sections).
- `logs/`: `s1_{1x,1m,10m}.txt`, `s2_lock.txt` (full run; its sections 3 and 6 hit an extractor error and are superseded), `s2_lock_index_rerun.txt` (sections 3 and 6 rerun on a working input), `s2_index_1x.txt`, `s4_copy.txt`.

## How to run
```sh
cd spikes/daemon && cargo build --release          # needs ../../target/release/memory-graph for S2: cargo build --release -p graph-cli at the repo root
W=/some/scratch/dir
memory-graph --db $W/d1x.redb index --org corpus --repo <name> testdata/corpus/<name>   # for each corpus repo
./run_s1.sh $W 1x $W/d1x.redb
./run_s2.sh $W "$(git rev-parse --show-toplevel)"
```
S2 also expects `$W/d1m.redb`, `$W/d10m.redb` (the model-A files from the data-model spike, see its README) and `$W/bigsrc` (a directory of Rust crates to index, about 2,000 files; the Cargo registry works apart from crates that trip the extractor bug noted in the spike doc). Measured on an Intel Core Ultra 9 275HX, 32 GB RAM, NVMe, btrfs, rustc 1.94.1, redb 2.6.3.
