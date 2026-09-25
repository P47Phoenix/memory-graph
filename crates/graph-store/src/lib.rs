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
//! [`StoreError::LegacyFormat`] and left untouched. The last release that
//! could read it is tagged `v1-last` and has `memory-graph migrate`.
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
mod v2;
pub use api::{detect_format, open_store, Page, PreparedFile, SnapshotStats, Store, StoreRead};
pub use codec::{encode_posting, POSTING_BLOCK};
pub(crate) use common::{
    check_unchanged, commit_prepared, dec, describe_in, enc, fingerprint, kind_label, kind_matches,
    name_key, open_failed, prepare_file, stored_fingerprint_matches, validate_spans, Scope, Tally,
    CATALOG, CHILDREN, META, NAMES, NODES, SYMBOLS,
};
pub use common::{FINGERPRINT_FORMAT_VERSION, MAX_SOURCE_BYTES};
pub use v2::V2_SCHEMA_VERSION as SCHEMA_VERSION;
pub use v2::{CompactStats, V2Snapshot, V2Store, VacuumStats};

/// Schema versions stamped by the retired per-node format. A file carrying
/// one of these is refused with [`StoreError::LegacyFormat`]; the range is
/// kept only so the refusal can name the format and its migration path.
pub const LEGACY_SCHEMA_VERSIONS: std::ops::RangeInclusive<u64> = 1..=2;
/// Version of the derived describe catalog (per-repo/language counts kept in
/// step with every write). Databases with another value rebuild it on open.
pub const CATALOG_VERSION: u64 = 1;
/// `Node::origin` of files written by a directory run; only these are pruned.
pub const ORIGIN_DIRECTORY: &str = "directory";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database is locked by another process: {0}")]
    Locked(String),
    #[error("incompatible schema version {found} (this build supports {SCHEMA_VERSION}); database left unmodified")]
    SchemaMismatch { found: u64 },
    /// The file is in the retired per-node format (ADR 0003, D5). It is
    /// never modified: re-index from source, or convert it with the last
    /// release that reads it.
    #[error("database `{path}` is in the retired v1 format (schema version {version}); this release reads only the v2 format. Re-index from source into a new file (recommended), or convert it with the last v1-capable release (git tag `v1-last`): `memory-graph migrate <new.redb>`")]
    LegacyFormat { path: String, version: u64 },
    #[error("cannot open database {path}: {reason}")]
    OpenFailed { path: String, reason: String },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("rejected: {0} is not valid UTF-8")]
    NotUtf8(String),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grain {
    Token,
    Symbol,
    File,
    Repo,
    Org,
}

impl std::str::FromStr for Grain {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Ok(match s {
            "token" => Self::Token,
            "symbol" => Self::Symbol,
            "file" => Self::File,
            "repo" => Self::Repo,
            "org" => Self::Org,
            _ => return Err(format!("unknown grain `{s}` (token|symbol|file|repo|org)")),
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
    /// Restrict the symbol grain to this kind: generic (`method`) or
    /// language-specific (`struct`).
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
    /// Token grain: the token; symbol grain: the symbol.
    pub span: Option<Span>,
    /// Number of matching tokens contained in this node.
    pub count: usize,
    /// Symbol grain only: the file has no symbols at all (e.g. fallback language).
    pub no_symbols: bool,
    /// Symbol grain only: the file has symbols, but none enclosing the match
    /// (of the requested kind); rolled up to the file.
    pub no_matching_symbol: bool,
}

/// One input of `Store::index_batch`.
#[derive(Debug, Clone, Copy)]
pub struct BatchFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
    pub language: Option<&'a str>,
    pub origin: Option<&'a str>,
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
mod tests;
#[cfg(test)]
mod v2_policy_tests;
#[cfg(test)]
mod v2_random;
#[cfg(test)]
mod v2_tests;
