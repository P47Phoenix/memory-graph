# memory graph
 

## Quick start (MVP)
```sh
cargo build --release
memory-graph --db ./g index-file --org acme --repo api src/lib.rs
memory-graph --db ./g index --org acme --repo api ./api   # whole directory, honors .gitignore
memory-graph --db ./g index --org acme --repo api ./api --prune   # also drop files a previous directory run indexed that are gone now
memory-graph --db ./g index --org acme --repo api ./api --reindex   # re-index every file even if unchanged (index-file has --reindex too)
memory-graph --db ./g search foo --language rust --json
memory-graph --db ./g describe                          # languages and symbol kinds actually present, per repo
memory-graph --db ./g symbols 'pars*' --kind method --language rust --json   # definitions by name/kind
memory-graph --db ./g search foo --grain file      # token|symbol|file|repo|org
python3 scripts/check-no-c-deps.py                 # pure-Rust gate, same as CI
```
Languages are detected per file (extension, filename such as `Makefile`, or a `#!` line), so a polyglot repo needs no flags; `describe` shows what was found (from a small counter catalog kept in step with every write, so it and filter validation cost O(repos), not O(tokens); databases written before it are upgraded in place on first open: that one-time backfill writes to the file, so it must be writable, and afterwards older builds refuse the database with a schema-version error), and `--language`/`--kind` values are checked against it. Language names are lowercased; a UTF-8 BOM is ignored; file paths are normalized (`./a.rs` = `a.rs`). Every language is tokenized by a generic fallback; symbols arrive with per-language extractors (later stories).

**Unchanged files are skipped.** Each File node stores a fingerprint: SHA-256 of the source bytes, the (lowercased) language, the extractor version and a store-level format version. Re-running `index` / `index-file` on a file with an identical fingerprint leaves its nodes untouched (only `origin` is refreshed if it differs) and counts it as `unchanged` in the summary and `--json` (`unchanged` is a subset of `files`; skipped files add nothing to `symbols`/`tokens`). Changed content, language override or extractor version re-indexes the file fully; files indexed before fingerprints existed re-index once. `--reindex` (on `index` and `index-file`) bypasses the check. `--force` keeps its original meaning only: with `--prune`, allow removing files when a run indexed nothing; `--reindex --prune` still refuses that. The fingerprint also folds in the tokenizer version, so tokenizer changes re-index files. The `index` text summary gained an `unchanged=` field (`files=N unchanged=M ...`), and `index-file` prints `[unchanged]`. Known issue (pre-existing): `index-file` stores the path as given, so it only shares a File node with a directory `index` run when invoked with the same repo-relative path. Skipped files still count as seen, so `--prune` behaves the same.

**Per-file failures.** If a file's extraction fails span validation (`invalid span`, an extractor/tokenizer bug), only that file fails: it is not stored (any previously stored version is left as is) and the other files in the batch are stored. `index` lists `failed: <path>: <reason>` on stdout (`failed=N` in the text summary; `failed` and `failed_files` in `--json`), skips `--prune` with a warning, and exits non-zero after finishing, with a one-line count on stderr. Storage errors still abort the whole batch. `index-file` (a single file) still hard-fails on an invalid span. Library note: `Store::index_batch` now reports `InvalidSpan` in the failing file's result slot (message prefixed with the path) instead of returning an error for the whole batch; only storage errors are returned as `Err`.

**Tokenizer dialects.** The generic fallback tokenizer (every language without an extractor) is unchanged: its `r"a\"b"` is an identifier `r` and a string (Python-style escapes), `b"y"` is an identifier and a string. The Rust extractor uses the `rust_literals` dialect (`TokenizerOptions`): raw strings `r"..."`, `r#"..."#` (any number of hashes, no escapes), `br#"..."#` and byte literals `b"..."`, `b'x'` are single Literal tokens, and an unterminated raw string runs to end of input. A Rust file indexed without the Rust extractor registered gets the plain fallback tokens and no symbols. The Rust extractor version is `rust-syn-2`, so Rust files re-index; other languages do not.

## Test corpus

`testdata/corpus/` vendors real public code (MIT/Apache-2.0 only, pinned commits, see each folder's `UPSTREAM.md`) as distinct repos grouped into applications by `corpus.json`:

- **messaging**: `rebus` + `rebus-rabbitmq` + `rebus-sqlserver` (transports implementing Rebus)
- **conduit**: `conduit-ui` (Angular) → `conduit-api` (Spring) → `conduit-data-access` (MyBatis) → `conduit-sql`
- **rust-library**: `anyhow`

`cargo test -p graph-cli --test corpus` checks the manifest (public, licensed), that every cross-repo link resolves, that every token of every file is parsed with exact spans, and that the graph stores exactly those tokens. Re-vendor with `scripts/vendor-corpus.py`.
