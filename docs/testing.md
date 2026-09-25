# Testing notes

## Disk space

`memory-graph index` keeps a reserve of free space on the database's volume
(`--min-free-disk`, default 5% of the volume, between 2 GB and 32 GB) and
projects the final database size from what it has stored so far. It refuses
up front when the volume is already below the reserve, and stops cleanly mid-run
when free space drops below it or when the projection (once the directory
walk is done) would not fit. Stopping commits what is pending, leaves the
database consistent, and exits non-zero; rerunning resumes, since stored
files are skipped as unchanged.

Three layers test this:

1. **Unit tests** (`crates/graph-cli/src/diskinfo.rs`): the policy over
   scripted samples (reserve rule, projection, calibration of the
   database-bytes-per-source-byte ratio, headroom vs projection stop, unknown
   platform and `--no-disk-check` never stop) and the recognition of
   "No space left on device" / Windows disk-full error text.
2. **Injected probe** (`crates/graph-cli/tests/diskguard.rs`): `index_dir`
   with a probe that reports plenty of space until the database passes a
   size, then almost none; asserts the stop message, a consistent database
   (`describe` shows no open batch), a partial store, and a full resume.
   Runs in `cargo test --workspace` on every platform.
3. **Real ENOSPC** (`scripts/disk-full-tmpfs.sh`, Linux with sudo): indexes a
   20x copy of `testdata/corpus` into a 48 MB tmpfs three ways: with the
   check off (a real `No space left on device`, reported as "disk full",
   database still consistent), with a 4 MB reserve (clean stop), then after
   enlarging the volume (resume). CI runs it on ubuntu. On Windows a
   size-capped volume needs a VHD and administrator rights, so that layer is
   manual there; the injected probe covers the logic.

## Database size

`crates/graph-cli/tests/size_gate.rs` indexes `testdata/corpus` and fails if
the database grows past a fixed multiple of the source bytes (and bytes per
token), before and after `vacuum --compact`, so on-disk cost regressions are
caught in CI. `scripts/measure-size.py` prints the full matrix (commit modes,
re-index, vacuum, compact) as a Markdown table; its numbers are the
provenance for the gate thresholds and for `diskinfo::DISK_RATIO`.
