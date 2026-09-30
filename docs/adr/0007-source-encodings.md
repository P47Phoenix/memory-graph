# ADR 0007: Indexing files in any source encoding

**Status:** Accepted (2026-09-30, by the owner), keeping File-record storage (C6). The owner's decisions of 2026-09-30 are recorded below, including the answers to the two questions this ADR first left open. Builds on [ADR 0003](0003-data-model.md) (the v2 store and its on-disk versioning), [ADR 0004](0004-client-server-and-replication.md) (the wire contract, Raft apply and the extractor-set hash) and [ADR 0005](0005-mcp.md) (MCP). Epic amendment: stories 40-44 in the [epic](../epic-code-memory-graph.md).

## In plain words

1. Today memory-graph indexes only UTF-8. Anything else is refused as "not valid UTF-8", and every file with a NUL byte, which includes every UTF-16 file, is skipped as "binary".
2. This ADR makes the indexer decode each file to UTF-8 first: UTF-16, Windows-1252/Latin-1, the Windows "ANSI" code page, Shift_JIS, GBK/GB18030, EUC-KR, Big5, ISO-2022-JP and the rest of the WHATWG encodings. No text file is skipped for its encoding.
3. Spans (byte, line, column) point into the **decoded UTF-8 text**, not the raw file. Each file records the encoding it was decoded from. For a UTF-8 file the decoded text is the file, so nothing changes for it.
4. Detection is automatic. `--encoding <name>` on the command line, and a `.memory-graph.toml` committed at the root of the indexed tree, override it.
5. Because every file becomes UTF-8 before tokenizing, `CustomerId` in a UTF-16 C# file, a Windows-1252 Pascal file and a UTF-8 Rust file is one term, and one search finds all three.
6. The on-disk schema goes from 10 to 11. Every existing (UTF-8) file stays byte-identical and an old database upgrades by a restamp, with no re-index. Old and new nodes cannot share a Raft log across that change.

## Owner decisions (2026-09-30)

- **Encodings:** UTF-16 LE/BE (with a BOM or sniffed); legacy 8-bit (Windows-1252/Latin-1); Windows "ANSI" (the system code page); CJK and other code pages (Shift_JIS, GBK/GB18030, EUC-KR, Big5, ...); lossy decoding as the last resort, so nothing is skipped for its encoding.
- **Span meaning:** spans point into the decoded UTF-8 text. Each file records its source encoding. Raw-byte offsets are not stored.
- **Detection:** automatic, with overrides: `--encoding <name>`, plus per-glob overrides.
- **Where the per-glob overrides live (was open question 1):** in a `.memory-graph.toml` at the indexed root, committed with the code. Precedence: BOM > CLI `--encoding` > `.memory-graph.toml` glob > auto. It is resolved on the client, which sends each file's resolved hint with that file; the server never reads repo files and never decides per node.
- **Lossy semantics (was open question 2):** as proposed. Lossy is reachable only through an explicit hint, or a BOM followed by invalid bytes. An "almost UTF-8" heuristic is a follow-up, not part of this ADR.
- **Dependencies:** `encoding_rs` and `chardetng`. The BSD-3-Clause part of `encoding_rs` is accepted, as it was for `subtle`.

## Context (checked against `origin/main`, 2026-09-30)

- `graph-store/src/common.rs` `prepare_file` does `std::str::from_utf8(f.bytes)` and, on failure, rejects the file with `StoreError::NotUtf8`. `graph-cli/src/lib.rs` tallies that as "not valid UTF-8".
- `graph-store/src/v2.rs` `V2Store::index_bytes_opts` (around line 2971) has its **own** `from_utf8` and fingerprint, separate from `prepare_file`; `index_batch` goes through `prepare_file`.
- `graph-cli/src/lib.rs` `read_and_prepare` skips any file that contains a NUL byte as `binary` (`bytes.contains(&0)`), before the store sees it. That throws away every UTF-16 file.
- The tokenizer (`graph-core/src/tokenizer.rs`) keeps a UTF-8 BOM in the text: U+FEFF counts as whitespace, has no column, and is stripped only for first-line checks (the RPG `**FREE` probe and similar).
- `common.rs` `fingerprint(registry, bytes, lang)` is `sha256:<hex of the raw bytes>|<lang>|<extractor version>|<FINGERPRINT_FORMAT_VERSION>`. `fingerprint_extractor_version` parses that shape back.
- Nodes, including File nodes, are stored as `serde_json` in the `nodes` table (`common.rs`); the per-file token/symbol stream is a separate table (`stream`).
- The public write inputs are `BatchFile { path, bytes, language, origin }` (`graph-store/src/lib.rs`) and `Store::index_bytes*` (`api.rs`). The wire: `FileBytes { path, bytes, optional language, optional origin }` in `proto/memory_graph/v1/common.proto`.
- `graph-server/src/extractors.rs` `extractors_hash` hashes the extractor set; nodes with different hashes refuse to share a cluster (ADR 0004 D5).
- The codec: `V2_SCHEMA_VERSION = 10`, `UPGRADABLE_SCHEMA_VERSION = 9` (a restamp, issue #137).
- The TOML config today is `serve --config` (`graph-cli/src/serve_config.rs`). There is no config for `index`.
- `encoding_rs` 0.8.42 is already in `Cargo.lock`, but only as a dev-dependency (through `yaml-rust2`, used by graph-cli's tests). It becomes a normal dependency here.

## Decisions

| # | Question | What was decided |
|---|---|---|
| C1 | What "exact spans" means | Exact against the decoded source; for a UTF-8 file the decoded source is the file. |
| C2 | Where decoding happens | Once, in `graph_core::encoding::decode`, called only from `prepare_file`; every write path goes through it. |
| C3 | Detection order | BOM, explicit hint, BOM-less UTF-16 sniff, 7-bit ISO-2022-JP by its escapes (amended 2026-09-30), valid UTF-8, `chardetng`, windows-1252. Lossy only through a hint or a BOM. |
| C4 | `ansi` | The Windows system code page (`GetACP`) on Windows, windows-1252 elsewhere; resolved on the client. |
| C5 | Binary files | Skipped only when they contain NULs and are not UTF-16; one check for every path. |
| C6 | Storage | `encoding` and `lossy` on the File node record, omitted for UTF-8; schema 10 → 11 by restamp from {9, 10}. |
| C7 | Fingerprint | Unchanged for UTF-8 files; `enc=<name>[+lossy]@<DECODER_VERSION>` suffix for every other file. |
| C8 | Exposure | `--encoding`, `--strict-encoding`, `.memory-graph.toml`; a `Store` trait change; additive proto fields; `describe`, `--json`, `--stats`, MCP. |
| C9 | Matching across encodings | One dictionary of UTF-8 terms, so a token matches whatever its source encoding; limits listed below. |
| C10 | Dependencies and versioning | `encoding_rs` 0.8.42 and `chardetng` 1.0.0, `=`-pinned; `DECODER_VERSION` in the fingerprint and the cluster hash. |

### C1. The exact-spans invariant, redefined

Today: every token's and symbol's text, byte range, line and column match the source exactly. From this ADR:

> Every token's and symbol's text, byte range, line and column match the **decoded source** exactly. For a UTF-8 file the decoded source is the file's bytes, unchanged.

- Byte offsets are offsets into the decoded UTF-8 string. Lines and columns are unchanged in meaning (columns count characters).
- Raw-file byte offsets are not stored. A caller that needs them re-decodes the file with its recorded encoding (C6) and maps offsets itself; `encoding_rs` makes that a single pass.
- The CLAUDE.md invariant line is amended on acceptance, not before.

### C2. One decoder, at every decode site

A new `graph_core::encoding` module (language-agnostic, no extractor involvement):

```rust
pub const DECODER_VERSION: u32 = 1; // bumped with any encoding_rs/chardetng bump or rule change
pub struct Decoded<'a> {
    pub text: Cow<'a, str>,          // borrowed for valid UTF-8 (no copy)
    pub encoding: &'static Encoding, // e.g. UTF_8, UTF_16LE, SHIFT_JIS
    pub lossy: bool,                 // a U+FFFD was inserted for an invalid sequence
}
pub fn decode(bytes: &[u8], hint: Option<&'static Encoding>) -> Decoded<'_>;
pub fn is_binary(bytes: &[u8]) -> bool;
```

The decode sites today, and what happens to each:

| Site | Today | After |
|---|---|---|
| `common.rs` `prepare_file` (used by `index_batch`, the server's `Index`, the Raft state machine) | `from_utf8` → `NotUtf8` | `is_binary`, then `decode` |
| `v2.rs` `V2Store::index_bytes_opts` (the single-file path behind `index-file` and `index_bytes*`) | its own `from_utf8` and `fingerprint` | routed through `prepare_file`, so it cannot drift |
| `graph-cli` `read_and_prepare` (the walk) | NUL check → `binary` skip | `is_binary`, then the hint is resolved (C8) and sent; no decoding in the CLI |

- Language detection (`Registry::detect_language`, including shebang probes) runs on the **decoded** text, so a UTF-16 script's shebang is seen.
- Extractors and the tokenizer are unchanged: they still receive `&str`.
- Because the server and the Raft state machine apply writes through `prepare_file` too, every node decodes the same bytes with the same hint the same way.

### C3. Detection order

1. **BOM.** UTF-8 (`EF BB BF`), UTF-16LE (`FF FE`), UTF-16BE (`FE FF`), chosen with `Encoding::for_bom`. The file is then decoded with `decode_without_bom_handling` (not `decode`, which would strip it), so U+FEFF **stays** at the start of the text, as a UTF-8 BOM does today, and the tokenizer's BOM rules (whitespace, no column, stripped for first-line probes) apply unchanged to every encoding. A BOM wins over every hint: a file that says what it is is believed.
2. **Explicit hint**, already resolved by the client (C8). `utf-8` with `--strict-encoding` restores today's refusal. The `replacement` encoding is refused as a hint (it decodes everything to one U+FFFD), and so is any label `encoding_rs` does not know.
3. **BOM-less UTF-16**, sniffed from alternating NULs **before** the UTF-8 check. ASCII text encoded as UTF-16 is valid UTF-8 (every other byte is NUL), so checking UTF-8 first would index it as UTF-8 full of NULs. The sniff (amended by the owner, 2026-09-30, so CJK UTF-16 is not skipped as binary): both byte orders are tried; in the first 4 KiB (even length), NULs in at least 1/64 of the high-byte positions (at least one) and in fewer than 1/16 of the low-byte positions, then the whole file decodes strictly with no C0 control other than tab, LF, FF and CR; if both orders pass, the one with more high-byte NULs wins. The thresholds are fixed in code and tested.
3b. **7-bit ISO-2022-JP** (amended by the owner, 2026-09-30): a file that is all 7-bit and contains a JIS X 0208 designation (`ESC $ @` or `ESC $ B`; `ESC ( B` / `ESC ( J` alone prove nothing) is decoded as ISO-2022-JP if that decodes without error, else it falls through; it is valid UTF-8 as bytes, so step 5 alone could never see it. The one exception to "no change for existing files": a 7-bit UTF-8 file that literally contains `ESC $ B` or `ESC $ @` and decodes cleanly is now ISO-2022-JP.
4. **Valid UTF-8**, used as-is (borrowed, no copy). **Zero behaviour change for existing files**: same text, same spans, same fingerprint. (A UTF-8 file containing NULs that does not sniff as UTF-16 is binary under C5 and never gets here through the walk.)
5. **`chardetng`**, fed the whole file, with its guess restricted to what it can return besides UTF-8: the legacy single-byte encodings, Shift_JIS, EUC-JP, **ISO-2022-JP**, EUC-KR, GBK/GB18030 and Big5. ISO-2022-JP is the one that is **not ASCII-compatible** (it switches modes with escape sequences); it is kept, because real Japanese source uses it, and C9 states its limit. If the guess does not decode cleanly, step 6.
6. **windows-1252**, which maps every byte, so it cannot fail.

**Lossy** (U+FFFD replacements, `lossy = true`) is therefore reachable **only** through an explicit hint that does not fit the bytes (step 2), or a BOM followed by invalid sequences (step 1). An auto-detected file can be mis-detected, but it is never lossy. `--strict-encoding` turns a lossy decode into a per-file refusal, reported as today's `NotUtf8` is.

The result is deterministic for given bytes, hint and `DECODER_VERSION` (C10).

*Follow-up (not in this ADR):* an "almost UTF-8" heuristic, decoding a file that is valid UTF-8 apart from a few stray bytes as lossy UTF-8 rather than handing it to `chardetng`. It would change detected encodings, so it would bump `DECODER_VERSION` and come with its own tests.

### C4. `ansi`

- `ansi` means the Windows "ANSI" code page: `GetACP()` through `windows-sys` (already in the tree, pure Rust) on Windows, mapped to its `encoding_rs` equivalent (1252 → windows-1252, 932 → Shift_JIS, 936 → GBK, 949 → EUC-KR, 950 → Big5, 125x → windows-125x, 874 → windows-874; an unmapped page, or 65001, is an error naming it).
- On any other OS it is windows-1252.
- It is **resolved on the client**, in the CLI, into a concrete label before indexing or sending. A server or Raft replica never consults its own code page.

### C5. Binary detection

- A file is binary when it contains a NUL byte **and** has no BOM **and** does not sniff as UTF-16 (C3 step 3) **and** has no explicit UTF-16 hint. It is skipped as `binary`, as today: a lossy decode of an image is useless.
- `graph_core::encoding::is_binary` is the one check, applied by the directory walk and by `prepare_file`, so every path (embedded, `index-file`, `--server`, a raw `Index` RPC) gives the same answer. The store reports it as a new per-file rejection `StoreError::Binary`, which the walk tallies under the existing `binary` skip.

### C6. Storage (schema 10 → 11)

Where the encoding is stored, and why:

| Option | Verdict |
|---|---|
| The **File node record** (`nodes` table, `serde_json`) | **Chosen.** `list_files`, `symbols`, `search --json` and MCP read File nodes already; they get the encoding without opening a stream. Two fields, `encoding` (a WHATWG name) and `lossy`, both `skip_serializing_if` default, so a UTF-8 File node's bytes are unchanged. |
| A flag bit plus record in the per-file stream header | Rejected: every file listing would have to open and parse streams, and the stream is the hot, size-gated structure. The owner's flag-bit hint was about keeping UTF-8 files byte-identical; the omitted-when-default fields do the same. |
| Only the fingerprint suffix (C7) | Rejected as the source of truth: it is an opaque change-detection key, parsed only by `fingerprint_extractor_version`. It carries the encoding for change detection, not for display. |

- The catalog (behind `describe`) gains per-repo, per-encoding file counts for non-UTF-8 files (UTF-8 is the remainder) and a lossy count. An absent entry means zero, so a v10 catalog is already correct for its all-UTF-8 content.
- `V2_SCHEMA_VERSION` becomes 11. The restamp set becomes **{9, 10}**: opening either restamps it straight to 11 in one small commit (neither has the new fields or catalog entries, and absence means UTF-8), as #137 did for 9 → 10. Anything older or newer is refused as today, without writing.
- Golden-byte tests pin a UTF-8 File node (unchanged) and a UTF-16LE and a lossy windows-1252 File node. An unknown encoding name in a stored File node is refused on read as `StoreError::Corrupt` (a corrupt record, never a panic), checked where the node is deserialized (`common.rs`, the `serde_json::from_slice` node decode, through a validating deserializer for the field).
- Vacuum, snapshot export and NDJSON export need no change: they copy File nodes as they are. NDJSON shows `encoding` and `lossy` only for non-UTF-8 files, because the fields are omitted when default.
- `size_gate` must not move: the UTF-8 corpus is byte-identical.

### C7. The fingerprint

- UTF-8 files (UTF-8 by BOM or by step 4, not lossy): **unchanged**, `sha256:<raw>|<lang>|<ver>|<fmt>`. No existing file is re-indexed by the upgrade.
- Every other file: `sha256:<raw>|<lang>|<ver>|<fmt>|enc=<WHATWG name>[+lossy]@<DECODER_VERSION>`. The hash is still of the raw bytes.
- So a different resolved hint (a new `--encoding`, or an edit to `.memory-graph.toml`) that changes a file's decode re-indexes exactly that file, and a decoder upgrade re-indexes exactly the non-UTF-8 files; an unchanged file with an unchanged decode is still a no-op.
- `fingerprint_extractor_version` is extended to tolerate the optional suffix (with a unit test), because the extractor-gap check parses fingerprints.

### C8. CLI, `.memory-graph.toml`, the Store trait, the wire and MCP

- **CLI.** `index` and `index-file` take `--encoding <auto|ansi|LABEL>`, where LABEL is any `encoding_rs` label except `replacement` (`utf-8`, `utf-16le`, `utf-16be`, `windows-1252`, `latin1`, `shift_jis`, `gbk`, `gb18030`, `euc-kr`, `big5`, `iso-2022-jp`, ...); default `auto`. An unknown label is a usage error listing examples. `--strict-encoding` refuses lossy decodes per file.
- **`.memory-graph.toml`** at the indexed root, committed with the code:

  ```toml
  [encoding]
  "legacy/**/*.pas" = "windows-1252"
  "docs/jp/**" = "shift_jis"
  ```

  - Precedence per file: **BOM > CLI `--encoding` (other than `auto`) > the first matching `.memory-graph.toml` glob > auto**.
  - The client reads it and resolves each file's hint (including `ansi`); the hint is sent per file. The server never reads repo files and never decides per node.
  - Results therefore depend on the tree's contents. That is intended: the file travels with the code, so everyone indexing the repo gets the same decode. An edit to it is picked up on the next index without `--reindex`, because the resolved hint enters the fingerprint suffix (C7).
  - An invalid label in it is an error naming the file and glob, before anything is written.
- **The `Store` trait** (public API change, `graph-store`): `BatchFile` gains `encoding: Option<&'static Encoding>` and `strict_encoding: bool` (both default-able; existing callers set `None`/`false`), and the `index_bytes*` family takes them through its options struct. `RemoteStore` carries them in `FileBytes`; the conformance suite gains a case run against embedded and `RemoteStore` (`graph-client --test conformance`) that the hint and strict flag behave identically on both.
- **Wire** (additive; regenerated with the xtask; `PROTOCOL_VERSION` unchanged): `FileBytes.encoding_hint` (`optional string`, a concrete WHATWG label) and `FileBytes.strict_encoding` (`bool`); the file details returned by listings and search gain `optional string encoding` (absent = UTF-8) and `bool lossy`; `RepoInfo` gains the per-encoding and lossy counts. An old server ignores the new fields; the client warns when `Hello` reports an older server and a hint other than auto would be sent.
- **Output.** `describe` (text and `--json`) lists encodings per repo; `symbols` and `search --json` show `encoding` per file when it is not UTF-8; `--stats` adds `transcoded` and `lossy` counts.
- **MCP.** `describe` and `list_files` include the encoding (and `lossy`) where it is not UTF-8.
- **`StoreError::NotUtf8`** stays for API and wire compatibility. It is produced only by `--encoding utf-8 --strict-encoding` on invalid bytes.

### C9. Matching across encodings

Every file is decoded to UTF-8 before tokenizing, so token text is interned in the one shared dictionary whatever its source encoding. A token matches across encodings: `CustomerId` in UTF-16 C#, Windows-1252 Pascal and UTF-8 Rust is one term, and one search hits all three, with `café` and `日本` behaving the same way when decoded correctly. The limits:

- **Short legacy files** can be guessed wrong by `chardetng` (e.g. a one-line windows-1252 file as ISO-8859-4, a short Shift_JIS file as windows-1250). No length gate is applied, because it would misdecode short CJK files the other way; `--encoding` or `.memory-graph.toml` is the remedy (noted 2026-09-30).
- A **mis-detected** code page garbles non-ASCII tokens. For the ASCII-compatible encodings (all the supported ones except UTF-16 and ISO-2022-JP), ASCII tokens still match. A file mis-detected as or from UTF-16 or ISO-2022-JP can garble ASCII too. `--encoding` or `.memory-graph.toml` fixes it, and the recorded encoding makes the mistake visible in `describe`/`symbols`.
- A **lossy** U+FFFD token does not match its original spelling.
- **NFC vs NFD** differences are unchanged: that is Unicode normalization, not encoding, and would be a separate decision.

### C10. Dependencies and versioning

| Crate | Version | Licence | Build | Notes |
|---|---|---|---|---|
| `encoding_rs` | 0.8.42 | (Apache-2.0 OR MIT) AND BSD-3-Clause | a pure-Rust `build.rs` (no `cc`, no `links`) | Already in `Cargo.lock` as a dev-dependency (`yaml-rust2`); becomes a normal dependency of graph-core. The BSD-3-Clause part is data derived from the WHATWG/Chromium tables; accepted by the owner, as for `subtle`. Pulls `cfg-if`, `core_detect`, `multiversion_no_op`, `scopeguard`, `simdutf8` (all MIT/Apache). |
| `chardetng` | 1.0.0 | Apache-2.0 OR MIT | none | Pulls `encoding_rs`, `cfg-if`, `memchr` (Unlicense OR MIT). |

- Checked on 2026-09-30 by adding both to graph-core on a scratch copy of `origin/main`: `cargo tree` showed only the crates above as new, and `python scripts/check-no-c-deps.py` passed ("checked 257 dependencies, pure-Rust gate passed"). The scratch change was not committed.
- Both are pinned with `=` in the workspace.
- `graph_core::encoding::DECODER_VERSION` names the decoder's behaviour. It is bumped with any `encoding_rs` or `chardetng` bump and any change to the rules in C3/C5. It is part of the non-UTF-8 fingerprint suffix (C7) and is hashed into the cluster's `extractors_hash` (`graph-server/src/extractors.rs`), so nodes that would decode differently refuse to share a cluster.
- **Mixed-version clusters.** The 10 → 11 schema change and the new `DECODER_VERSION` input both change `extractors_hash`, so old and new nodes cannot share a Raft log. A rolling upgrade across this change is **unsupported**: upgrade every node together (or snapshot, upgrade, restore), and say so in the release notes.

## Alternatives rejected

- **Raw-byte offsets** (store spans into the undecoded file, or both). It would keep "exact against the file" literally, but every consumer would need the encoding to read a span's text, the stream would grow a second offset per token (against `size_gate`), and cross-encoding matching would still need decoded text. Rejected by the owner.
- **Lossy-only** (decode everything as UTF-8 with U+FFFD). Trivial, but it garbles every non-ASCII identifier in legacy code and turns UTF-16 files into mostly-NUL noise; it would index files without making them searchable.
- **Explicit-only detection** (index non-UTF-8 only when told to). Safe, but most users do not know which of their files are UTF-16 or Windows-1252, and a mixed tree would need a config before it worked at all. Kept as the override instead.
- **Transcoding in the CLI only** (send UTF-8 over the wire). The server's raw `Index` RPC, MCP and library users would still reject the same files. One decoder in `prepare_file` avoids that; the CLI only resolves hints.
- **The server resolving `.memory-graph.toml` or `ansi`.** It would make a replica's result depend on files or settings on its own disk. Rejected; the client resolves, the hint travels.
- **`charset-normalizer-rs`, `chardet`** (other detectors). Heavier, less maintained, or not restricted to the WHATWG set that `encoding_rs` decodes; `chardetng` is the detector Firefox uses, by the `encoding_rs` author.
- **Normalizing to NFC** while decoding. Out of scope (C9); it would change UTF-8 files, which this ADR promises not to touch.

## Consequences

- Files that were skipped (UTF-16) or refused (legacy encodings) are now indexed and searchable alongside UTF-8 ones.
- The exact-spans invariant is weaker for non-UTF-8 files: exact against the decoded text, not the file. Tools that seek into the raw file by a span's byte offset are wrong for those files and must decode first; `encoding` is shown wherever it matters.
- The unchanged-file check now **decodes before fingerprinting**, because the fingerprint of a non-UTF-8 file depends on the decode. For UTF-8 files that is the same validation as today; for legacy files without a BOM or hint it also runs `chardetng` over the file on every re-index, even when the file turns out unchanged. The cost is linear and small next to tokenizing, but a re-index of a large legacy tree is no longer only a hash.
- Results depend on a `.memory-graph.toml` in the tree, by design.
- A one-off restamp on open (from 9 or 10 to 11); no re-index for UTF-8 content; `size_gate` unchanged.
- The public `Store` inputs change (`BatchFile`, `index_bytes*` options); callers outside the workspace must set the new fields.
- No rolling upgrade across this change in a cluster (C10).
- Two new pinned dependencies in graph-core; the gate stays clean.
- Decoding costs a copy only for non-UTF-8 files (valid UTF-8 is borrowed).
- A `chardetng` guess can be wrong on short legacy files; `--encoding` and `.memory-graph.toml` are the remedy, and the recorded encoding makes it diagnosable.

## Test plan

- **Unit and golden bytes.** The detection order: each step, a BOM beating a hint, a hint that does not fit → lossy, `replacement` refused as a hint, and **BOM-less ASCII-only UTF-16LE and BE (valid UTF-8 as bytes) detected as UTF-16, not UTF-8**. The UTF-16 sniff thresholds at their edges; ISO-2022-JP detected and decoded; `ansi` mapping (Windows only, plus the non-Windows default); a BOM kept as U+FEFF after decoding UTF-16. Golden bytes for UTF-8 (unchanged), UTF-16LE and lossy File nodes; an unknown encoding name refused; `fingerprint_extractor_version` with the suffix; `extractors_hash` changes with `DECODER_VERSION`.
- **Proptest.** Random bytes never panic; spans are always valid char boundaries of the decoded text and their text matches it; `lossy` is set iff a U+FFFD was inserted that was not in the input; valid UTF-8 input without NULs decodes to itself with encoding UTF-8.
- **Fixtures.** One per encoding: UTF-16LE/BE with and without a BOM, windows-1252, Shift_JIS, ISO-2022-JP, GBK, EUC-KR, Big5, and one invalid-bytes file. Each indexes with the expected encoding recorded; token text and line/col are exact against the decoded text; symbols are found (e.g. a C# class in a UTF-16 file), and the language is detected from the decoded text (a UTF-16 script with a shebang).
- **Every decode site.** The same encoded file through `index` (the walk), `index-file` (`index_bytes_opts`), `index_batch`, `--server` and a raw `Index` RPC gives the same encoding, spans and fingerprint.
- **`.memory-graph.toml`.** Precedence (BOM > CLI > glob > auto); an edit to the file re-indexes exactly the affected files without `--reindex`; an invalid label is refused before any write; the server never reads it (a remote index uses the client's file).
- **Cross-encoding match.** The same identifiers (ASCII and non-ASCII, e.g. `café`, `日本`) in UTF-8, UTF-16LE, UTF-16BE, windows-1252 (Latin only) and Shift_JIS (CJK): one `search` returns hits from every file, and the dictionary holds one term per identifier.
- **No regression.** The UTF-8 corpus output is byte-identical to `main` and the corpus hash is unchanged; `size_gate` passes unchanged.
- **Binary.** A PNG is still skipped as `binary`; a UTF-16 file is not; the same through every path above.
- **Conformance and differential.** A `run_all` case with an encoded file and a hint (encoding and spans read back), run against embedded and `RemoteStore`; `run_differential` / `run_crash_rerun_differential` inputs that include encoded files, so embedded, `--server` and Raft all agree.
- **Upgrade.** A v10 and a v9 database open, are restamped to v11, read identically, and re-indexing them is a no-op; `--encoding` forces a re-index of just the affected file; a v12 file is refused without writing; a v10 node and a v11 node refuse to form one cluster.
- **Gates.** fmt, clippy, `cargo test --workspace`, `test_gate.py`, `check-no-c-deps.py`, the proto-regen job.

## Stories (epic 40-44)

| # | Story | Points | Delivery |
|---|---|---|---|
| 40 | E1: `graph_core::encoding` (decode, detection order, UTF-16 sniff, `is_binary`, `ansi` resolution, `DECODER_VERSION`) | 5 | PR B |
| 41 | E2: store integration: every decode site through `prepare_file`, the `Store` input change, binary rejection, File node fields and the {9,10} → 11 restamp, fingerprint rule, `extractors_hash`, conformance/differential | 8 | PR B |
| 42 | E3: CLI `--encoding`/`--strict-encoding`, `.memory-graph.toml`, walk tallies | 3 | PR B |
| 43 | E4: exposure: proto fields (xtask), server honours the hint, catalog counts, `describe`/`--json`/`symbols`/`--stats`, MCP | 5 | PR C |
| 44 | E5: encoding fixtures and cross-encoding tests, the guide's Encodings section, glossary, ADR 0003 note, CLAUDE.md invariant | 3 | PR C |

Acceptance criteria are in the [epic](../epic-code-memory-graph.md).

## Open questions

None. The owner answered both on 2026-09-30 (above) and accepted this ADR the same day.
