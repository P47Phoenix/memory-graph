# memory graph
 

## Quick start (MVP)
```sh
cargo build --release
memory-graph --db ./g index-file --org acme --repo api src/lib.rs
memory-graph --db ./g index --org acme --repo api ./api   # whole directory, honors .gitignore
memory-graph --db ./g index --org acme --repo api ./api --prune   # also drop files a previous directory run indexed that are gone now
memory-graph --db ./g search foo --language rust --json
memory-graph --db ./g describe                          # languages and symbol kinds actually present, per repo
memory-graph --db ./g symbols 'pars*' --kind method --language rust --json   # definitions by name/kind
memory-graph --db ./g search foo --grain file      # token|symbol|file|repo|org
python3 scripts/check-no-c-deps.py                 # pure-Rust gate, same as CI
```
Languages are detected per file (extension, filename such as `Makefile`, or a `#!` line), so a polyglot repo needs no flags; `describe` shows what was found, and `--language`/`--kind` values are checked against it. Language names are lowercased; a UTF-8 BOM is ignored; file paths are normalized (`./a.rs` = `a.rs`). Every language is tokenized by a generic fallback; symbols arrive with per-language extractors (later stories).
