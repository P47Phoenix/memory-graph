# Spike: why a 100-file batch took seconds to apply (#162)

**Question.** In `payload_too_large_backlog_and_a_20_mib_file_replicate`,
applying one Raft entry that holds a 100-small-file batch took 3.5 s on a
debug build (11-14 s under load), on the leader and the follower alike. Is
it per-file commit or fsync cost, catalog upkeep, or a debug-only hot spot?
Target: at most 1 s per 100-file batch in a release build.

**Method.** `crates/graph-store/examples/batch_apply.rs` times one
100-file batch (`src/f{i}.rs`, one small Rust function each, new content
every round), split into `prepare_with` and the commit through
`index_prepared_marked` (the Raft apply path), on an empty store and on one
that first indexed the test's large files: six 1.5 MiB and one 20 MiB
comment-only `.js` files, in another repo. `BA_SIX` and `BA_HUGE_KIB` vary
the large files; `BATCH_APPLY_SPLIT=1` also times an empty marked batch and
a one-file index.

```sh
cargo run --release -p graph-store --example batch_apply -- 5
```

## Measured (release, Windows 11, before the fix)

| store before the batch | prepare | commit |
|---|---|---|
| empty | 1.4 ms | 7.3 ms |
| six 1.5 MiB + one 20 MiB file, first batch | 1.5 ms | 2,703 ms |
| same, later batches | 1.5 ms | 830 ms |
| one 20 MiB file only, later batches | 1.7 ms | 510 ms |
| one 5 MiB file only, later batches | 1.5 ms | 122 ms |
| six 1.5 MiB files only, later batches | 1.5 ms | 270 ms |

An empty marked batch took 0.9 ms and a one-file index 1.3 ms in every
case, so neither fsync nor the marker nor catalog upkeep was the cost. The
commit time grew linearly with the size of the *largest tokens already
stored*, in any repo.

## Cause

Each large comment is one token, and its text is stored whole in the packed
reverse dictionary (`dict_rev_blocks`, ADR 0003 story 5), in whatever block
was last when it was interned. `dict_rev_append` extended the last block
whenever it held fewer than `DICT_BLOCK` entries: decode the whole block,
push one entry, encode it again and insert it. After a long term, every
new term (each new number or name in the small files) therefore decoded,
re-encoded and rewrote the 20 MiB block, until the block reached
`DICT_BLOCK` (128) entries.
The first batch after the large files paid that for the most terms, which is
the 2.7 s; later batches still added a few new terms each. A debug build
does the same work, only slower, hence the 3.5-14 s seen in the test.

## Fix

`dict_rev_append` also treats a last block of `DICT_BLOCK_MAX_BYTES`
(64 KiB) or more as full, checking the stored length before decoding, so a
long term closes its block and later terms start a new one. Readers never
assumed `DICT_BLOCK` entries per block (`vacuum` already leaves uneven
boundaries and the lookup binary-searches first ids), so this is a write
policy, not a format change: old and new binaries read each other's files.
Test: `a_long_term_closes_its_dict_rev_block`.

## Measured (release, after the fix)

| store before the batch | prepare | commit |
|---|---|---|
| empty | 1.4 ms | 7.3 ms |
| six 1.5 MiB + one 20 MiB file, first batch | 1.4 ms | 8.2 ms |
| same, later batches | 1.4 ms | 7.6-9.4 ms |

About 9 ms per 100-file batch either way, far under the 1 s target.

## Not changed

A hashed long term (over `MAX_INLINE_TERM` bytes) is verified against its
`dict_rev` text on every lookup, which decodes that term's whole block. With
the fix the block holds little besides the long term, so this costs about
one copy of the term, only when a file containing that same term is
indexed again.
