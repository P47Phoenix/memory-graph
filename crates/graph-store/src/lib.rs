//! Embedded graph store on `redb` (pure Rust). Knows nothing about any
//! particular language.
//!
//! Layout: `api.rs` holds the object-safe [`Store`]/[`StoreRead`] traits and
//! [`open_store`]; `common.rs` the shared write helpers and entity tables;
//! `v2.rs` the one storage format (ADR 0003: interned dictionary, one compact
//! stream per file, count postings). This file is the public types and the
//! module wiring.
//!
//! The original per-node format ("v1", schema versions 1 and 2) was retired
//! outright (ADR 0003, D5): a file in that format is refused with
//! [`StoreError::LegacyFormat`] and left untouched (redb may first repair a
//! file that was not cleanly closed, before the version is read). The last
//! release that could read it is tagged `v1-last` and has `memory-graph
//! migrate`.
use graph_core::{NodeId, Span, SymbolKind, TokenClass};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod api;
/// Block/varint stream and posting codec. Crate-internal implementation
/// detail of the store (ADR 0003), not part of the query API (issue #50:
/// this was `pub mod codec` until pre-1.0 cleanup narrowed it). The few
/// items benchmarks genuinely need (`encode_posting`, `POSTING_BLOCK`, ADR
/// 0003 story 6) are re-exported individually below instead.
pub(crate) mod codec;
mod common;
pub mod conformance;
pub mod export;
pub mod read_stats;
pub use read_stats::ReadStats;
mod cache_size;
pub use cache_size::{
    derive_cache_bytes, FALLBACK_CACHE_BYTES, MAX_DERIVED_CACHE_BYTES, MIN_DERIVED_CACHE_BYTES,
};
mod v2;
pub use api::{
    detect_format, open_store, FingerprintSnapshot, Page, PreparedFile, RemoteParts, SnapshotStats,
    Store, StoreRead,
};
pub use codec::{encode_posting, POSTING_BLOCK};
pub(crate) use common::{
    check_unchanged, commit_prepared, dec, describe_in, enc, grain_accepts, kind_label,
    kind_matches, name_key, open_failed, prepare_file, stored_fingerprint_matches,
    stored_fingerprints, validate_spans, Scope, Tally, CATALOG, CHILDREN, META, NAMES, NODES,
    SYMBOLS,
};
pub use common::{FINGERPRINT_FORMAT_VERSION, MAX_SOURCE_BYTES};
/// Older schema versions an open upgrades in place (see `SCHEMA_VERSION`).
#[doc(hidden)]
pub use v2::precheck_reads;
pub use v2::UPGRADABLE_SCHEMA_VERSIONS;
pub use v2::V2_SCHEMA_VERSION as SCHEMA_VERSION;
pub use v2::{CompactStats, MarkedCommitHook, RaftMarker, V2Snapshot, V2Store, VacuumStats};

/// Schema versions stamped by the retired per-node format. A file carrying
/// one of these is refused with [`StoreError::LegacyFormat`]; the range is
/// kept only so the refusal can name the format and its migration path.
pub const LEGACY_SCHEMA_VERSIONS: std::ops::RangeInclusive<u64> = 1..=2;
/// Version of the derived describe catalog (per-repo/language counts kept in
/// step with every write). Databases with another value rebuild it on open.
/// 2 (ADR 0007, epic story 43): per-repo counts of non-UTF-8 files per
/// encoding (`e`) and of lossy files (`l`); a lower or missing value has
/// them recounted from the File nodes on open.
pub const CATALOG_VERSION: u64 = 2;
/// `Node::origin` of files written by a directory run; only these are pruned.
pub const ORIGIN_DIRECTORY: &str = "directory";

/// Whether `e` is a per-file refusal by `strict_encoding` (ADR 0007 C3, C8):
/// `NotUtf8` for a file decoded as UTF-8, or `StrictEncoding` naming the
/// encoding. Both are typed variants with their own wire detail (#180), so
/// this holds for an error that came back over the wire too, whatever a
/// proxy did to its message.
pub fn is_strict_encoding_refusal(e: &StoreError) -> bool {
    match e {
        StoreError::NotUtf8(_) | StoreError::StrictEncoding { .. } => true,
        // Version skew (#180): a server from before the typed variant sends
        // a `Rejected` ending in this text. Kept for one release; removed
        // in the next (ADR 0007).
        StoreError::Rejected(m) => m.ends_with(LEGACY_STRICT_ENCODING_SUFFIX),
        _ => false,
    }
}

/// The end of the `Rejected` message older servers sent for a non-UTF-8
/// strict refusal, before `StoreError::StrictEncoding` (#180). Recognised
/// for one release only.
const LEGACY_STRICT_ENCODING_SUFFIX: &str = "(strict encoding)";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database is locked by another process: {0}")]
    Locked(String),
    #[error("incompatible schema version {found} (this build supports {SCHEMA_VERSION}); database left unmodified")]
    SchemaMismatch { found: u64 },
    /// The file is in the retired per-node format (ADR 0003, D5). It is not
    /// modified (beyond redb's own crash repair of a file that was not
    /// cleanly closed): re-index from source, or convert it with the last
    /// release that reads it.
    #[error("database `{path}` is in the retired v1 format (schema version {version}); this release reads only the v2 format. Re-index from source into a new file (recommended), or convert it with the last v1-capable release (git tag `v1-last`): `memory-graph migrate <new.redb>`")]
    LegacyFormat { path: String, version: u64 },
    #[error("cannot open database {path}: {reason}")]
    OpenFailed { path: String, reason: String },
    #[error("rejected: {0}")]
    Rejected(String),
    /// Produced only with `strict_encoding` when a file decoded as UTF-8
    /// (a hint of `utf-8`, or a UTF-8 BOM) has invalid sequences (ADR 0007
    /// C8); without it the invalid sequences become U+FFFD and the file is
    /// stored `lossy`. Kept for API and wire compatibility.
    #[error("rejected: {0} is not valid UTF-8")]
    NotUtf8(String),
    /// Produced only with `strict_encoding` when a file decoded in an
    /// encoding other than UTF-8 has byte sequences invalid in it (ADR 0007
    /// C3, C8, #180). `path` is the file as given, `encoding` the WHATWG
    /// name it was decoded as (`Shift_JIS`, `windows-1252`, ...).
    #[error(
        "rejected: `{path}` has byte sequences that are invalid in {encoding} (strict encoding)"
    )]
    StrictEncoding { path: String, encoding: String },
    /// The file is binary (ADR 0007 C5): it has a NUL byte, no BOM, no
    /// UTF-16 hint, and does not sniff as UTF-16.
    #[error("rejected: {0} is binary")]
    Binary(String),
    #[error("rejected: {0} is larger than 4 GiB")]
    TooLarge(String),
    #[error("invalid span: {0}")]
    InvalidSpan(String),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Schema(graph_core::SchemaError),
    #[error("storage error: {0}")]
    Storage(String),
    /// A snapshot handle (ADR 0003 story 10, [`Store::snapshot`]) was used
    /// past its configured max age. Returned by every read made through an
    /// expired handle (checked on each call, not only when the snapshot was
    /// issued -- see the doc comment on [`Store::snapshot`]), never by a
    /// fresh call to `snapshot()` itself.
    #[error(
        "snapshot expired after {age_secs}s (max age {max_age_secs}s); open a new snapshot with Store::snapshot"
    )]
    SnapshotExpired { age_secs: u64, max_age_secs: u64 },
    /// A write reached a replica that is not the leader (ADR 0004 D8): the
    /// leader, when known, is named so a client can retry there. Never
    /// produced by the embedded store; surfaced through the `Store` trait
    /// by a remote backend.
    #[error("not the leader{}", not_leader_hint(.leader_id, .leader_addr))]
    NotLeader {
        leader_id: Option<u64>,
        leader_addr: Option<String>,
    },
    /// No leader is currently known (election in progress or quorum lost);
    /// retry after the suggested delay.
    #[error("no leader is currently known; retry after {retry_after_ms} ms")]
    NoLeader { retry_after_ms: u64 },
    /// The client and server do not speak the same protocol (version
    /// mismatch, malformed response, ...).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A node's data directory belongs to another cluster than the one it
    /// was asked to take part in (ADR 0004 D6: `serve --join` on a
    /// directory of another cluster, or a membership change naming a node
    /// of another cluster). Nothing was changed.
    #[error("wrong cluster: expected cluster {expected}, found cluster {found}; nothing was changed (a data directory belongs to one cluster for life: clear it to join another)")]
    WrongCluster { expected: String, found: String },
    /// A raft-marked write (ADR 0004 D5, `V2Store::*_marked`) whose marker
    /// index is at or below the stored one: already applied, nothing was
    /// written. The server's state machine consumes it (a replay is a
    /// skip); it never reaches a client.
    #[error("raft marker {index} already applied")]
    AlreadyApplied { index: u64 },
}

fn not_leader_hint(id: &Option<u64>, addr: &Option<String>) -> String {
    match (id, addr) {
        (Some(id), Some(addr)) => format!(" (leader is node {id} at {addr})"),
        (Some(id), None) => format!(" (leader is node {id})"),
        (None, Some(addr)) => format!(" (leader is at {addr})"),
        (None, None) => String::new(),
    }
}

impl<E: Into<redb::Error>> From<E> for StoreError {
    fn from(e: E) -> Self {
        match e.into() {
            redb::Error::DatabaseAlreadyOpen => StoreError::Locked("already open".into()),
            other => StoreError::Storage(other.to_string()),
        }
    }
}

/// Options for `index_bytes_opts` / `index_batch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexOptions {
    /// Re-index files even when their fingerprint is unchanged.
    pub reindex: bool,
    /// The encoding hint for `index_bytes_opts` (ADR 0007 C8), and the
    /// default for a [`BatchFile`] without one of its own. `None` means
    /// auto-detect; a BOM always wins.
    pub encoding: Option<&'static graph_core::encoding::Encoding>,
    /// Refuse a file whose decode is lossy (`--strict-encoding`); for a
    /// batch, a file is strict when this or its own flag is set.
    pub strict_encoding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grain {
    Token,
    /// The nearest enclosing symbol of any kind.
    Symbol,
    /// The nearest enclosing callable: generic kind `method` or `function`.
    Method,
    /// The nearest enclosing generic `type`, or a language's method container
    /// that is not a type (a Rust `impl` block).
    Class,
    File,
    Repo,
    Org,
}

impl Grain {
    /// Grains whose rows are an enclosing symbol (symbol, method, class):
    /// they read the symbol chain and take `Query::symbol_kind`.
    pub fn is_symbolic(self) -> bool {
        matches!(self, Self::Symbol | Self::Method | Self::Class)
    }
}

impl std::str::FromStr for Grain {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Ok(match s {
            "token" => Self::Token,
            "symbol" => Self::Symbol,
            "method" => Self::Method,
            "class" => Self::Class,
            "file" => Self::File,
            "repo" => Self::Repo,
            "org" => Self::Org,
            _ => {
                return Err(format!(
                    "unknown grain `{s}` (token|symbol|method|class|file|repo|org)"
                ))
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    pub class: Option<TokenClass>,
    pub grain: Grain,
    /// Restrict the symbol, method or class grain to this kind: generic
    /// (`method`) or language-specific (`struct`). Combined with the grain's
    /// own rule (`--grain class --symbol-kind struct` is the nearest
    /// enclosing struct).
    pub symbol_kind: Option<String>,
    /// Keep at most this many rows (after deterministic ordering).
    pub limit: Option<usize>,
    /// Skip this many rows (after the same deterministic ordering `limit`
    /// truncates) before collecting `limit` rows. ADR 0003 story 11: paired
    /// with `limit`, this is the offset half of offset/limit paging over a
    /// [`Store::snapshot`](crate::Store::snapshot) handle -- call `search`
    /// again on the same snapshot with `offset` advanced by the previous
    /// page's `limit` to fetch the next page of a frozen, consistent view.
    pub offset: Option<usize>,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: None,
            org: None,
            repo: None,
            class: None,
            grain: Grain::Token,
            symbol_kind: None,
            limit: None,
            offset: None,
        }
    }
}

/// Symbol lookup by name: exact, or a prefix with a trailing `*` (`*` alone
/// lists everything). A trailing `\*` is a literal star (exact match on a name
/// ending in `*`). Empty patterns and `**` are rejected as ambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolQuery {
    pub pattern: String,
    /// Generic kind (`method`) or language-specific kind (`struct`, `trait`).
    pub kind: Option<String>,
    pub language: Option<String>,
    pub org: Option<String>,
    pub repo: Option<String>,
    /// Restrict to one file path (normalized, relative as indexed).
    pub file: Option<String>,
    /// Keep at most this many rows (after deterministic ordering).
    pub limit: Option<usize>,
    /// Skip this many rows before collecting `limit` rows; see
    /// [`Query::offset`] for the paging convention this mirrors.
    pub offset: Option<usize>,
}

impl SymbolQuery {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: None,
            language: None,
            org: None,
            repo: None,
            file: None,
            limit: None,
            offset: None,
        }
    }
}

/// A point in a source file: byte offset, line and column, numbered as a
/// [`Span`]'s start is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub byte: u32,
    pub line: u32,
    pub col: u32,
}

impl Position {
    /// A span's start.
    pub fn start_of(span: &Span) -> Self {
        Self {
            byte: span.start,
            line: span.start_line,
            col: span.start_col,
        }
    }

    /// `name_pos` when known, else the span's start (an old server leaves
    /// `name_pos` unset, ADR 0010 D3); `None` only with no span either.
    pub fn name_or_start(name_pos: Option<Position>, span: Option<&Span>) -> Option<Self> {
        name_pos.or_else(|| span.map(Self::start_of))
    }
}

/// A symbol with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolHit {
    pub org: String,
    pub repo: String,
    pub file: String,
    pub language: Option<String>,
    pub name: String,
    /// Qualified path from the outermost enclosing symbol, e.g. `S::a`.
    pub qualified: String,
    pub kind: SymbolKind,
    /// Language-specific kind string (`struct`, `impl`, `fn`, ...).
    pub lang_kind: Option<String>,
    pub span: Option<Span>,
    /// The declaration (name) position (ADR 0010 D3): the first identifier
    /// token inside `span` whose text equals `name`, else the span's start.
    /// Worked out at query time; `None` from an old server that does not
    /// send it (use [`Position::name_or_start`]). Omitted from JSON when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_pos: Option<Position>,
    /// The extractor's owner hint (`SymbolDecl::owner`), e.g. a Go method's
    /// receiver type. Omitted from JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The file's WHATWG encoding when it is not UTF-8 (ADR 0007). Omitted
    /// from JSON for UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The file was decoded lossily (U+FFFD replacements). Omitted from JSON
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossy: bool,
}

/// Per-language contents of a repo. `symbol_kinds` keys are `generic` or
/// `generic/language-specific` (e.g. `type/struct`, `method/fn`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageInfo {
    pub files: usize,
    pub symbols: usize,
    pub tokens: usize,
    pub symbol_kinds: BTreeMap<String, usize>,
}

/// A language whose stored files were indexed by an extractor the running
/// store does not have (#74): re-indexing them now would store them
/// tokens-only and drop their symbols. See [`Store::extractor_gaps`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractorGap {
    pub org: String,
    pub repo: String,
    /// Lowercased language name, as stored.
    pub language: String,
    /// The extractor version recorded in a stored file's fingerprint.
    pub stored_version: String,
    /// Symbols of this language currently stored in the repo.
    pub symbols: usize,
}

impl std::fmt::Display for ExtractorGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}: {} file(s) were indexed with extractor `{}` ({} symbols stored), which this build does not have; re-indexing them stores tokens only and drops their symbols (use a build with that language's extractor)",
            self.org, self.repo, self.language, self.stored_version, self.symbols
        )
    }
}

/// How much of the database file holds live data (#90). See
/// [`Store::space_usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUsage {
    /// The file's length on disk.
    pub file_bytes: u64,
    /// Bytes in allocated pages (what `vacuum --compact` would keep, give
    /// or take the allocator's rounding).
    pub live_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub org: String,
    pub repo: String,
    pub files: usize,
    pub languages: BTreeMap<String, LanguageInfo>,
    pub token_classes: BTreeMap<String, usize>,
    /// `true` if this repo currently has an open (in-progress or crashed)
    /// chunked ingest batch (ADR 0003 story 3, decision D3). Read from the
    /// `open_batch` marker table inside the same read transaction that
    /// builds this `RepoInfo`, so the flag is snapshot-consistent with the
    /// rest of the struct.
    #[serde(default)]
    pub open_batch: bool,
    /// Files per non-UTF-8 WHATWG encoding (ADR 0007); UTF-8 files are the
    /// remainder of `files`. Kept in the catalog, so `describe` stays O(repos).
    #[serde(default)]
    pub encodings: BTreeMap<String, usize>,
    /// Files decoded lossily (U+FFFD replacements).
    #[serde(default)]
    pub lossy: usize,
}

impl RepoInfo {
    /// Every kind name usable with `--kind` (generic and language-specific),
    /// optionally limited to one language.
    pub fn kind_names(&self, language: Option<&str>) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for (lang, li) in &self.languages {
            if language.is_some_and(|l| !lang.eq_ignore_ascii_case(l)) {
                continue;
            }
            for k in li.symbol_kinds.keys() {
                out.extend(k.split('/').map(str::to_string));
            }
        }
        out
    }
}

/// One result row at the requested grain, with its containment path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub grain: Grain,
    pub org: String,
    pub repo: Option<String>,
    pub file: Option<String>,
    pub language: Option<String>,
    /// Qualified enclosing symbol path, e.g. `Foo::bar`.
    pub symbol: Option<String>,
    pub symbol_kind: Option<SymbolKind>,
    pub lang_kind: Option<String>,
    /// Token grain: the token's class.
    pub token_class: Option<TokenClass>,
    /// Token grain: the token; symbol, method and class grains: the picked
    /// symbol's full span.
    pub span: Option<Span>,
    /// Symbol, method and class grains, when a symbol was picked: its
    /// declaration (name) position, as [`SymbolHit::name_pos`]. Omitted from
    /// JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_pos: Option<Position>,
    /// Number of matching tokens contained in this node.
    pub count: usize,
    /// Symbol, method and class grains only: the file has no symbols at all
    /// (e.g. fallback language).
    pub no_symbols: bool,
    /// Symbol, method and class grains only: the file has symbols, but none
    /// enclosing the match satisfies the grain (and the requested kind);
    /// rolled up to the file.
    pub no_matching_symbol: bool,
    /// The hit's file's WHATWG encoding when it is not UTF-8 (ADR 0007).
    /// Omitted from JSON for UTF-8, and always absent on repo and org rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The file was decoded lossily (U+FFFD replacements). Omitted from JSON
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossy: bool,
}

/// One input of `Store::index_batch`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BatchFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
    pub language: Option<&'a str>,
    pub origin: Option<&'a str>,
    /// The file's resolved encoding hint (ADR 0007 C8; `ansi` and
    /// `.memory-graph.toml` are resolved by the client). `None` means
    /// the batch's [`IndexOptions::encoding`], else auto-detect; a BOM
    /// always wins.
    pub encoding: Option<&'static graph_core::encoding::Encoding>,
    /// Refuse this file when its decode is lossy.
    pub strict_encoding: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestStats {
    pub file_id: NodeId,
    pub symbols: usize,
    pub tokens: usize,
    /// True when an existing file was replaced.
    pub replaced: bool,
    /// The stored file had an identical fingerprint (same content, language,
    /// extractor version and index format), so nothing was re-indexed:
    /// `symbols` and `tokens` are 0 and the existing nodes were left untouched.
    pub unchanged: bool,
    /// The file was flagged `has_errors`.
    pub has_errors: bool,
    /// Normalized path and language actually stored.
    pub path: String,
    pub language: String,
    /// The file's WHATWG encoding when it is not UTF-8 (ADR 0007). Omitted
    /// from JSON for UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The file was decoded lossily (U+FFFD replacements). Omitted from JSON
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossy: bool,
    /// Set when the extractor produced an invalid span (#203): its symbols
    /// were dropped and only the tokens stored. Names the extractor and the
    /// span. Omitted from JSON when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_warning: Option<String>,
}

/// What the test modules below reach through `use super::*`.
#[cfg(test)]
mod test_prelude {
    pub(crate) use crate::v2::V2_SCHEMA_VERSION;
    pub(crate) use graph_core::NodeKind;
    pub(crate) use redb::{ReadableMultimapTable, ReadableTable};
    pub(crate) type Result<T> = std::result::Result<T, crate::StoreError>;
}
#[cfg(test)]
pub(crate) use test_prelude::*;

#[cfg(test)]
mod detect_tests;
#[cfg(test)]
mod read_stats_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod v2_policy_tests;
#[cfg(test)]
mod v2_raft_tests;
#[cfg(test)]
mod v2_random;
#[cfg(test)]
mod v2_tests;
