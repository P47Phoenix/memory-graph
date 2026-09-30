# ADR 0007: Indexing files in any source encoding

**Status:** Proposed (2026-09-30). The owner's decisions of 2026-09-30 are recorded below; the owner accepts this ADR before any code lands. Builds on [ADR 0003](0003-data-model.md) (the v2 store and its on-disk versioning), [ADR 0004](0004-client-server-and-replication.md) (the wire contract and Raft apply) and [ADR 0005](0005-mcp.md) (MCP). Epic amendment: stories 40-44 in the [epic](../epic-code-memory-graph.md).

## In plain words

1. Today memory-graph indexes only UTF-8. Anything else is refused as "not valid UTF-8", and every file with a NUL byte, which includes every UTF-16 file, is skipped as "binary".
2. This ADR makes the indexer decode each file to UTF-8 first: UTF-16, Windows-1252/Latin-1, the Windows "ANSI" code page, Shift_JIS, GBK/GB18030, EUC-KR, Big5 and the rest of the WHATWG encodings. When nothing fits, it decodes lossily as a last resort, so no text file is skipped for its encoding.
3. Spans (byte, line, column) point into the **decoded UTF-8 text**, not the raw file. Each file records the encoding it was decoded from. For a UTF-8 file the decoded text is the file, so nothing changes for it.
4. Detection is automatic. `--encoding <name>` (and a per-glob table in the config) overrides it.
5. Because every file becomes UTF-8 before tokenizing, `CustomerId` in a UTF-16 C# file, a Windows-1252 Pascal file and a UTF-8 Rust file is one term, and one search finds all three.
6. The on-disk schema goes from 10 to 11 through a new flag bit, so every existing (UTF-8) file stays byte-identical and an old database upgrades by a restamp, with no re-index.

## Owner decisions (2026-09-30)

- **Encodings:** UTF-16 LE/BE (with a BOM or sniffed); legacy 8-bit (Windows-1252/Latin-1); Windows "ANSI" (the system code page); CJK and other code pages (Shift_JIS, GBK/GB18030, EUC-KR, Big5, ...); lossy decoding as the last resort, so nothing is skipped for its encoding.
- **Span meaning:** spans point into the decoded UTF-8 text. Each file records its source encoding. Raw-byte offsets are not stored.
- **Detection:** automatic, with an override: `--encoding <name>`, plus a per-glob config table.
- **Dependencies:** `encoding_rs` and `chardetng`. The BSD-3-Clause part of `encoding_rs` is accepted, as it was for `subtle`.

## Context (checked against `origin/main`, 2026-09-30)

- `graph-store/src/common.rs` `prepare_file` does `std::str::from_utf8(f.bytes)` and, on failure, rejects the file with `StoreError::NotUtf8`. `graph-cli/src/lib.rs` tallies that as "not valid UTF-8".
- `graph-cli/src/lib.rs` `read_and_prepare` skips any file that contains a NUL byte as `binary` (`bytes.contains(&0)`), before the store sees it. That throws away every UTF-16 file.
- The tokenizer (`graph-core/src/tokenizer.rs`) keeps a UTF-8 BOM in the text: U+FEFF counts as whitespace, has no column, and is stripped only for first-line checks (the RPG `**FREE` probe and similar).
- `common.rs` `fingerprint(registry, bytes, lang)` is `sha256:<hex of the raw bytes>|<lang>|<extractor version>|<FINGERPRINT_FORMAT_VERSION>`. `fingerprint_extractor_version` parses that shape back.
- The wire: `FileBytes { path, bytes, optional language, optional origin }` in `proto/memory_graph/v1/common.proto`. The server, and the Raft state machine, call the same `prepare_file`.
- The codec: `V2_SCHEMA_VERSION = 10`, `UPGRADABLE_SCHEMA_VERSION = 9` (a restamp, issue #137). The per-file stream header has a flag byte with bits 0 (`ranges_dense`) and 1 (`has_owners`); unknown bits are refused, not ignored.
- The TOML config today is `serve --config` (`graph-cli/src/serve_config.rs`, `MEMORY_GRAPH_CONFIG`). There is no config file for `index`.
- `encoding_rs` 0.8.42 is already in `Cargo.lock`, but only as a dev-dependency (through `yaml-rust2`, used by graph-cli's tests). It becomes a normal dependency here.

## Decisions

| # | Question | What was decided |
|---|---|---|
| C1 | What "exact spans" means | Exact against the decoded source; for a UTF-8 file the decoded source is the file. |
| C2 | Where decoding happens | Once, in `graph_core::encoding::decode`, called by `prepare_file`; embedded, server and Raft apply share it. |
| C3 | Detection order | BOM, explicit hint, valid UTF-8, BOM-less UTF-16 sniff, `chardetng` over legacy encodings, lossy. |
| C4 | `ansi` | The Windows system code page (`GetACP`) on Windows, windows-1252 elsewhere; resolved on the client, never on a server or replica. |
| C5 | Binary files | Skipped only when they contain NULs and do not sniff as UTF-16; the same check in `index`, `index-file` and the store. |
| C6 | Storage | Encoding and `lossy` on the file, behind a new flag bit; schema 10 → 11 by restamp; UTF-8 files byte-identical. |
| C7 | Fingerprint | Unchanged for UTF-8 files; the encoding name is added for every other file. |
| C8 | Exposure | `--encoding`, `--strict-encoding`, a per-glob `[encoding]` table; additive proto fields; `describe`, `--json`, `--stats` and MCP show encodings. |
| C9 | Matching across encodings | One dictionary of UTF-8 terms, so a token matches whatever its source encoding; limits listed below. |
| C10 | Dependencies | `encoding_rs` 0.8 and `chardetng` 1.0, pinned; both pure Rust. |

### C1. The exact-spans invariant, redefined

Today: every token's and symbol's text, byte range, line and column match the source exactly. From this ADR:

> Every token's and symbol's text, byte range, line and column match the **decoded source** exactly. For a UTF-8 file the decoded source is the file's bytes, unchanged.

- Byte offsets are offsets into the decoded UTF-8 string. Lines and columns are unchanged in meaning (columns count characters).
- Raw-file byte offsets are not stored. A caller that needs them re-decodes the file with its recorded encoding (C6) and maps offsets itself; `encoding_rs` makes that a single pass.
- The CLAUDE.md invariant line is amended on acceptance, not before.

### C2. One decoder, in the store

A new `graph_core::encoding` module (language-agnostic, no extractor involvement):

```rust
pub struct Decoded<'a> {
    pub text: Cow<'a, str>,          // borrowed for valid UTF-8 (no copy)
    pub encoding: &'static str,      // WHATWG name, e.g. "UTF-8", "UTF-16LE", "Shift_JIS"
    pub lossy: bool,                 // a U+FFFD was inserted for an invalid sequence
}
pub fn decode(bytes: &[u8], hint: Option<&'static Encoding>) -> Decoded<'_>;
```

`prepare_file` calls `decode` in place of `from_utf8`. Extractors and the tokenizer are unchanged: they still receive `&str`. Because the server and the Raft state machine apply writes through `prepare_file` too, every node decodes the same bytes the same way (the hint travels with the bytes, C8; `ansi` is resolved before that, C4).

### C3. Detection order

1. **BOM.** UTF-8 (`EF BB BF`), UTF-16LE (`FF FE`), UTF-16BE (`FE FF`). The BOM is decoded to U+FEFF and **kept** at the start of the text, as a UTF-8 BOM is today, so the tokenizer's BOM rules (whitespace, no column, stripped for first-line probes) are unchanged for every encoding. A BOM wins over a hint: a file that says what it is is believed.
2. **Explicit hint** (`--encoding`, the per-glob table, or `FileBytes.encoding_hint`). `utf-8` with `--strict-encoding` restores today's refusal.
3. **Valid UTF-8**, used as-is (borrowed, no copy). **Zero behaviour change for existing files**: same text, same spans, same fingerprint.
4. **BOM-less UTF-16**, sniffed from alternating NULs: in a sample of the first 4 KiB (even length), at least 3/4 of the even (BE) or odd (LE) bytes are NUL and fewer than 1/16 of the others are, and the whole file then decodes without error. The thresholds are fixed in code and tested; they target ASCII-heavy source, which is what BOM-less UTF-16 code looks like.
5. **`chardetng`**, fed the whole file (it is linear and allocation-free), with UTF-8 excluded (step 3 already failed) and its answer restricted to the legacy single- and multi-byte encodings; top-level domain hint none. If its answer does not decode cleanly, **windows-1252** is used (it maps every byte, so it cannot fail).
6. **Lossy.** Only reachable through a hint (an explicit encoding that does not fit the bytes) or a BOM followed by invalid sequences: the file is decoded with U+FFFD replacements and flagged `lossy`. `--strict-encoding` turns this into a per-file refusal, reported as today.

The detected encoding is deterministic for given bytes and binary (the crates are pinned, C10).

### C4. `ansi`

- `ansi` means the Windows "ANSI" code page: `GetACP()` through `windows-sys` (already in the tree, pure Rust) on Windows, mapped to its `encoding_rs` equivalent (1252 → windows-1252, 932 → Shift_JIS, 936 → GBK, 949 → EUC-KR, 950 → Big5, 125x → windows-125x, 874 → windows-874; an unmapped page, or 65001, is an error naming it).
- On any other OS it is windows-1252.
- It is **resolved on the client**, in the CLI, into a concrete label before indexing or sending. A server or Raft replica never consults its own code page, so two nodes on different systems cannot decode one log entry differently.

### C5. Binary detection

- A file is binary when it contains a NUL byte **and** has no UTF-16 BOM **and** does not sniff as UTF-16 (C3 step 4). It is skipped as `binary`, as today: a lossy decode of an image is useless.
- A file with a UTF-16/UTF-8 BOM, or with an explicit UTF-16 hint, is never treated as binary.
- The check moves into `graph_core::encoding::is_binary` and is applied by the directory walk, `index-file` and `prepare_file`, so every path (embedded, `--server`, a raw `Index` RPC) gives the same answer. The store reports it as a new per-file rejection `StoreError::Binary`, which the walk tallies under the existing `binary` skip.

### C6. Storage (schema 10 → 11)

- The per-file stream header's flag byte gains **bit 2, `has_encoding`**. When set, an encoding record follows the flag byte: one byte of flags (bit 0 `lossy`) and the encoding's WHATWG name (varint length + ASCII). When clear, the file is UTF-8 and not lossy.
- A UTF-8 file never sets it, so every existing file's bytes are **unchanged**, and the golden-byte tests for today's streams stay as they are. New golden-byte tests pin a UTF-16LE and a lossy windows-1252 stream; an unknown encoding name or a set reserved bit is refused.
- The catalog (behind `describe`) gains a per-repo, per-encoding file count for non-UTF-8 files (UTF-8 is the remainder), and a lossy count. An absent entry means zero, so a v10 catalog is already correct for its all-UTF-8 content.
- `V2_SCHEMA_VERSION` becomes 11 and opening a v10 database restamps it (one small commit), as #137 did for 9 → 10; `UPGRADABLE_SCHEMA_VERSION` becomes a set {9, 10}, both restamped straight to 11 (neither has the new bit or catalog entries). Anything older or newer is refused as today, without writing.
- `size_gate` must not move: the UTF-8 corpus is byte-identical.

### C7. The fingerprint

- UTF-8 files (decoded at step 1 or 3 as UTF-8, not lossy): **unchanged**, `sha256:<raw>|<lang>|<ver>|<fmt>`. No existing file is re-indexed by the upgrade.
- Every other file: `sha256:<raw>|<lang>|<ver>|<fmt>|enc=<WHATWG name>[+lossy]`. The hash is still of the raw bytes.
- So re-running with a different `--encoding` that changes the result re-indexes exactly that file; an unchanged file with an unchanged decode is still a no-op.
- `fingerprint_extractor_version` is extended to tolerate the optional suffix (with a unit test), because the extractor-gap check parses fingerprints.

### C8. CLI, config, wire and MCP

- **CLI.** `index` and `index-file` take `--encoding <auto|ansi|LABEL>`, where LABEL is any `encoding_rs` label (`utf-8`, `utf-16le`, `utf-16be`, `windows-1252`, `latin1`, `shift_jis`, `gbk`, `gb18030`, `euc-kr`, `big5`, ...); default `auto`. An unknown label is a usage error listing examples. `--strict-encoding` refuses lossy decodes (per file, reported like today's `NotUtf8`).
- **Config.** An `[encoding]` table of glob → label, e.g. `"legacy/**/*.pas" = "windows-1252"`, in an index config file (see open question 1). The first matching glob wins; `--encoding` other than `auto` overrides the table.
- **Wire** (additive; regenerated with the xtask; `PROTOCOL_VERSION` unchanged):
  - `FileBytes.encoding_hint` (`optional string`, a concrete label; the client has already resolved `ansi`) and `FileBytes.strict_encoding` (`bool`).
  - The file details returned by the file-listing and search results gain `optional string encoding` (absent = UTF-8) and `bool lossy`.
  - `RepoInfo` gains the per-encoding counts and the lossy count from the catalog.
  - An old server receiving an `encoding_hint` ignores it; the client warns when `Hello` reports a server older than this change and a non-`auto` encoding was asked for.
- **Output.** `describe` (text and `--json`) lists encodings per repo; `symbols` and `search --json` show `encoding` per file when it is not UTF-8; `--stats` adds `transcoded` and `lossy` counts.
- **MCP.** `describe` and `list_files` include the encoding (and `lossy`) where it is not UTF-8.
- **`StoreError::NotUtf8`** stays for API and wire compatibility. It is produced only by `--encoding utf-8 --strict-encoding` on invalid bytes.

### C9. Matching across encodings

Every file is decoded to UTF-8 before tokenizing, so token text is interned in the one shared dictionary whatever its source encoding. A token matches across encodings: `CustomerId` in UTF-16 C#, Windows-1252 Pascal and UTF-8 Rust is one term, and one search hits all three, with `café` and `日本` behaving the same way when decoded correctly. The limits:

- A **mis-detected** legacy code page garbles non-ASCII tokens (ASCII still matches, because every supported legacy encoding is ASCII-compatible). `--encoding` fixes it, and the recorded encoding makes the mistake visible in `describe`/`symbols`.
- A **lossy** U+FFFD token does not match its original spelling.
- **NFC vs NFD** differences are unchanged: that is Unicode normalization, not encoding, and would be a separate decision.

### C10. Dependencies

| Crate | Version | Licence | Build | Notes |
|---|---|---|---|---|
| `encoding_rs` | 0.8.42 | (Apache-2.0 OR MIT) AND BSD-3-Clause | a pure-Rust `build.rs` (no `cc`, no `links`) | Already in `Cargo.lock` as a dev-dependency (`yaml-rust2`); becomes a normal dependency of graph-core. The BSD-3-Clause part is data derived from the WHATWG/Chromium tables; accepted by the owner, as for `subtle`. Pulls `cfg-if`, `core_detect`, `multiversion_no_op`, `scopeguard`, `simdutf8` (all MIT/Apache). |
| `chardetng` | 1.0.0 | Apache-2.0 OR MIT | none | Pulls `encoding_rs`, `cfg-if`, `memchr` (Unlicense OR MIT). |

Checked on 2026-09-30 by adding both to graph-core on a scratch copy of `origin/main`: `cargo tree` showed only the crates above as new, and `python scripts/check-no-c-deps.py` passed ("checked 257 dependencies, pure-Rust gate passed"). The scratch change was not committed. Both are pinned (`=`) in the workspace, because a detector change would change detected encodings (and so fingerprints, C7); a bump is a deliberate change with its own tests.

## Alternatives rejected

- **Raw-byte offsets** (store spans into the undecoded file, or both). It would keep "exact against the file" literally, but every consumer would need the encoding to read a span's text, the stream would grow a second offset per token (against `size_gate`), and cross-encoding matching would still need decoded text. Rejected by the owner.
- **Lossy-only** (decode everything as UTF-8 with U+FFFD). Trivial, but it garbles every non-ASCII identifier in legacy code and throws UTF-16 files into mostly-NUL noise; it would index files without making them searchable.
- **Explicit-only detection** (index non-UTF-8 only when `--encoding` says so). Safe, but most users do not know which of their files are UTF-16 or Windows-1252, and a mixed tree would need a glob table before it worked at all. Kept as the override instead.
- **Transcoding in the CLI only** (send UTF-8 over the wire). The server's raw `Index` RPC, MCP and library users would still reject the same files, and a `ansi`-style decision could differ between writers. One decoder in `prepare_file` avoids both.
- **`charset-normalizer-rs`, `chardet`** (other detectors). Heavier, less maintained, or not restricted to the WHATWG set that `encoding_rs` decodes; `chardetng` is the detector Firefox uses, by the `encoding_rs` author.
- **Normalizing to NFC** while decoding. Out of scope (C9); it would change UTF-8 files, which this ADR promises not to touch.

## Consequences

- Files that were skipped (UTF-16) or refused (legacy encodings) are now indexed and searchable alongside UTF-8 ones.
- The exact-spans invariant is weaker for non-UTF-8 files: exact against the decoded text, not the file. Tools that seek into the raw file by a span's byte offset are wrong for those files and must decode first; `encoding` is shown wherever it matters.
- A one-off restamp on open (10 → 11); no re-index for UTF-8 content; `size_gate` unchanged.
- Two new pinned dependencies in graph-core; the gate stays clean.
- Decoding costs a copy only for non-UTF-8 files (valid UTF-8 is borrowed).
- A `chardetng` guess can be wrong on short legacy files; `--encoding` and the per-glob table are the remedy, and the recorded encoding makes it diagnosable.

## Test plan

- **Unit and golden bytes.** The detection order (each step, including BOM beating a hint, and a hint that does not fit → lossy); the UTF-16 sniff thresholds at their edges; `ansi` mapping (Windows only, plus the non-Windows default); golden bytes for a UTF-16LE and a lossy stream; reserved-bit and unknown-name refusal; `fingerprint_extractor_version` with the suffix.
- **Proptest.** Random bytes never panic; spans are always valid char boundaries of the decoded text and their text matches it; `lossy` is set iff a U+FFFD was inserted that was not in the input; valid UTF-8 input decodes to itself with `encoding = "UTF-8"`.
- **Fixtures.** One per encoding: UTF-16LE/BE with and without a BOM, windows-1252, Shift_JIS, GBK, EUC-KR, Big5, and one invalid-bytes file. Each indexes with the expected encoding recorded; token text and line/col are exact against the decoded text; symbols are found (e.g. a C# class in a UTF-16 file).
- **Cross-encoding match.** The same identifiers (ASCII and non-ASCII, e.g. `café`, `日本`) in UTF-8, UTF-16LE, UTF-16BE, windows-1252 (Latin only) and Shift_JIS (CJK): one `search` returns hits from every file, and the dictionary holds one term per identifier.
- **No regression.** The UTF-8 corpus output is byte-identical to `main` and the corpus hash is unchanged; `size_gate` passes unchanged.
- **Binary.** A PNG is still skipped as `binary`; a UTF-16 file is not; the same through `index-file`, `--server` and a raw `Index` RPC.
- **Conformance and differential.** A `run_all` case with an encoded file (encoding and spans read back), and `run_differential` / `run_crash_rerun_differential` inputs that include encoded files, so embedded, `--server` and Raft all agree.
- **Upgrade.** A v10 database opens, is restamped to v11, reads identically, and re-indexing it is a no-op; `--encoding` forces a re-index of just the affected file; a v12 file is refused without writing.
- **Gates.** fmt, clippy, `cargo test --workspace`, `test_gate.py`, `check-no-c-deps.py`, the proto-regen job.

## Stories (epic 40-44)

| # | Story | Points | Delivery |
|---|---|---|---|
| 40 | E1: `graph_core::encoding` (decode, detection order, UTF-16 sniff, `is_binary`, `ansi` resolution) | 5 | PR B |
| 41 | E2: store integration: `prepare_file`, binary rejection, flag bit and schema 10 → 11 restamp, fingerprint rule, conformance/differential | 8 | PR B |
| 42 | E3: CLI `--encoding`/`--strict-encoding`, the per-glob `[encoding]` table, walk tallies | 3 | PR B |
| 43 | E4: exposure: proto fields (xtask), server honours the hint, catalog counts, `describe`/`--json`/`symbols`/`--stats`, MCP | 5 | PR C |
| 44 | E5: encoding fixtures and cross-encoding tests, the guide's Encodings section, glossary, ADR 0003 note, CLAUDE.md invariant | 3 | PR C |

Acceptance criteria are in the [epic](../epic-code-memory-graph.md).

## Open questions (for the owner)

1. **Where the per-glob `[encoding]` table lives.** Today the only TOML config is `serve --config`. Proposed: `index --config <file>` (the same `MEMORY_GRAPH_CONFIG` variable, and the `[encoding]` table also accepted in the serve config for server-side defaults). The alternative is a `.memory-graph.toml` discovered at the indexed root, which travels with the repo but makes the result depend on a file inside the tree.
2. **Lossy is only reachable through a hint or a BOM** (C3): auto-detection always lands on a legacy encoding that maps every byte (windows-1252 at worst), so an auto-detected file is never flagged `lossy`, only possibly mis-detected. Is that the intended meaning of "lossy as the last resort", or should auto-detection prefer lossy UTF-8 when a file is "almost UTF-8" (say, a few invalid bytes in an otherwise valid UTF-8 file)? Proposed: keep it as written; a nearly-UTF-8 file is common enough (one stray Latin-1 byte) that a follow-up could add that heuristic behind its own test.
