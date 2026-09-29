//! `From` / `TryFrom` between the `memory_graph.v1` messages and the
//! `graph-core` / `graph-store` types they mirror.
//!
//! Rust -> proto is infallible (`From`). Proto -> Rust is `TryFrom` with
//! [`ConvertError`]: proto3 has no "absent" for scalars and enums, so an
//! UNSPECIFIED enum value, a missing required sub-message (`SymbolDecl.span`,
//! `SearchRequest.query`) or a `u64` count that does not fit `usize` is a
//! protocol error the caller sees, never a silently substituted default.
use crate::pb;
use graph_core::{Extraction, Node, NodeKind, Span, SymbolDecl, SymbolKind, TokenClass, TokenDecl};
use graph_store::{
    BatchFile, CompactStats, Grain, Hit, IndexOptions, IngestStats, LanguageInfo, Page, Query,
    RepoInfo, SnapshotStats, StoreError, SymbolHit, SymbolQuery, VacuumStats,
};
use std::collections::BTreeMap;
use std::time::Duration;

/// A message that does not describe a valid value: what it was and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError(pub String);

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed message: {}", self.0)
    }
}

impl std::error::Error for ConvertError {}

impl From<ConvertError> for StoreError {
    fn from(e: ConvertError) -> Self {
        crate::error::protocol_error(e.to_string())
    }
}

impl From<ConvertError> for tonic::Status {
    fn from(e: ConvertError) -> Self {
        crate::error::WireError::Protocol(e.to_string()).into()
    }
}

fn unspecified(what: &str, value: i32) -> ConvertError {
    ConvertError(format!("{what} value {value} is unspecified or unknown"))
}

fn missing(what: &str) -> ConvertError {
    ConvertError(format!("required field `{what}` is missing"))
}

fn to_usize(what: &str, v: u64) -> Result<usize, ConvertError> {
    usize::try_from(v).map_err(|_| ConvertError(format!("`{what}` = {v} does not fit usize")))
}

fn opt_usize(what: &str, v: Option<u64>) -> Result<Option<usize>, ConvertError> {
    v.map(|v| to_usize(what, v)).transpose()
}

fn to_u64(v: usize) -> u64 {
    // usize is at most 64 bits on every supported target.
    v as u64
}

// ---------------------------------------------------------------- enums --

macro_rules! enum_map {
    ($rust:ty, $pb:ty, $what:literal, { $($r:ident => $p:ident),* $(,)? }) => {
        impl From<$rust> for $pb {
            fn from(v: $rust) -> Self {
                match v { $(<$rust>::$r => <$pb>::$p,)* }
            }
        }
        impl TryFrom<$pb> for $rust {
            type Error = ConvertError;
            fn try_from(v: $pb) -> Result<Self, ConvertError> {
                match v {
                    $(<$pb>::$p => Ok(<$rust>::$r),)*
                    <$pb>::Unspecified => Err(unspecified($what, 0)),
                }
            }
        }
        impl TryFrom<i32> for Wire<$rust> {
            type Error = ConvertError;
            /// The raw `i32` an enum field holds -> the Rust enum.
            fn try_from(v: i32) -> Result<Self, ConvertError> {
                let p = <$pb>::try_from(v).map_err(|_| unspecified($what, v))?;
                <$rust>::try_from(p).map(Wire)
            }
        }
    };
}

/// Newtype so the raw `i32` an enum field carries can be converted to the
/// Rust enum with `TryFrom` (a blanket `TryFrom<i32>` on foreign enums is
/// not ours to write). `Wire::<NodeKind>::try_from(n.kind)?.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire<T>(pub T);

enum_map!(NodeKind, pb::NodeKind, "NodeKind", {
    Org => Org, Repo => Repo, File => File, Symbol => Symbol, Token => Token,
});
enum_map!(SymbolKind, pb::SymbolKind, "SymbolKind", {
    Module => Module, Type => Type, Function => Function, Method => Method,
    Variable => Variable, Constant => Constant, Other => Other,
});
enum_map!(TokenClass, pb::TokenClass, "TokenClass", {
    Identifier => Identifier, Keyword => Keyword, Literal => Literal, Operator => Operator,
    Punctuation => Punctuation, Comment => Comment, Other => Other,
});
enum_map!(Grain, pb::Grain, "Grain", {
    Token => Token, Symbol => Symbol, Method => Method, Class => Class,
    File => File, Repo => Repo, Org => Org,
});

/// `enum -> i32` for a message field.
pub fn enum_i32<R, P: From<R> + Into<i32>>(v: R) -> i32 {
    P::from(v).into()
}

fn enum_field<R>(what: &str, v: i32) -> Result<R, ConvertError>
where
    Wire<R>: TryFrom<i32, Error = ConvertError>,
{
    Wire::<R>::try_from(v)
        .map(|w| w.0)
        .map_err(|e| ConvertError(format!("{what}: {}", e.0)))
}

fn opt_enum_field<R>(what: &str, v: Option<i32>) -> Result<Option<R>, ConvertError>
where
    Wire<R>: TryFrom<i32, Error = ConvertError>,
{
    v.map(|v| enum_field(what, v)).transpose()
}

// ----------------------------------------------------------------- Span --

impl From<Span> for pb::Span {
    fn from(s: Span) -> Self {
        pb::Span {
            start: s.start,
            end: s.end,
            start_line: s.start_line,
            start_col: s.start_col,
            end_line: s.end_line,
            end_col: s.end_col,
        }
    }
}

impl TryFrom<pb::Span> for Span {
    type Error = ConvertError;
    fn try_from(s: pb::Span) -> Result<Self, ConvertError> {
        // Every value is representable; validation of the range (`start <=
        // end`, lines >= 1) is the store's job (`validate_spans`), the same
        // as for an embedded caller.
        Ok(Span {
            start: s.start,
            end: s.end,
            start_line: s.start_line,
            start_col: s.start_col,
            end_line: s.end_line,
            end_col: s.end_col,
        })
    }
}

// ----------------------------------------------------------------- Node --

impl From<Node> for pb::Node {
    fn from(n: Node) -> Self {
        pb::Node {
            id: n.id,
            parent: n.parent,
            kind: enum_i32::<_, pb::NodeKind>(n.kind),
            name: n.name,
            language: n.language,
            symbol_kind: n.symbol_kind.map(enum_i32::<_, pb::SymbolKind>),
            lang_kind: n.lang_kind,
            token_class: n.token_class.map(enum_i32::<_, pb::TokenClass>),
            has_errors: n.has_errors,
            origin: n.origin,
            fingerprint: n.fingerprint,
            span: n.span.map(Into::into),
        }
    }
}

impl TryFrom<pb::Node> for Node {
    type Error = ConvertError;
    fn try_from(n: pb::Node) -> Result<Self, ConvertError> {
        Ok(Node {
            id: n.id,
            parent: n.parent,
            kind: enum_field("Node.kind", n.kind)?,
            name: n.name,
            language: n.language,
            symbol_kind: opt_enum_field("Node.symbol_kind", n.symbol_kind)?,
            lang_kind: n.lang_kind,
            token_class: opt_enum_field("Node.token_class", n.token_class)?,
            has_errors: n.has_errors,
            origin: n.origin,
            fingerprint: n.fingerprint,
            span: n.span.map(Span::try_from).transpose()?,
        })
    }
}

fn nodes_from(v: Vec<pb::Node>) -> Result<Vec<Node>, ConvertError> {
    v.into_iter().map(Node::try_from).collect()
}

fn nodes_into(v: Vec<Node>) -> Vec<pb::Node> {
    v.into_iter().map(Into::into).collect()
}

impl From<Page<Node>> for pb::NodePage {
    fn from(p: Page<Node>) -> Self {
        pb::NodePage {
            items: nodes_into(p.items),
            has_more: p.has_more,
        }
    }
}

impl TryFrom<pb::NodePage> for Page<Node> {
    type Error = ConvertError;
    fn try_from(p: pb::NodePage) -> Result<Self, ConvertError> {
        Ok(Page {
            items: nodes_from(p.items)?,
            has_more: p.has_more,
        })
    }
}

impl From<Vec<Node>> for pb::NodeBatch {
    fn from(nodes: Vec<Node>) -> Self {
        pb::NodeBatch {
            nodes: nodes_into(nodes),
            not_found: false,
        }
    }
}

impl TryFrom<pb::NodeBatch> for Vec<Node> {
    type Error = ConvertError;
    fn try_from(b: pb::NodeBatch) -> Result<Self, ConvertError> {
        nodes_from(b.nodes)
    }
}

// ---------------------------------------------------------------- Query --

impl From<Query> for pb::Query {
    fn from(q: Query) -> Self {
        pb::Query {
            text: q.text,
            language: q.language,
            org: q.org,
            repo: q.repo,
            class: q.class.map(enum_i32::<_, pb::TokenClass>),
            grain: enum_i32::<_, pb::Grain>(q.grain),
            symbol_kind: q.symbol_kind,
            limit: q.limit.map(to_u64),
            offset: q.offset.map(to_u64),
        }
    }
}

impl TryFrom<pb::Query> for Query {
    type Error = ConvertError;
    fn try_from(q: pb::Query) -> Result<Self, ConvertError> {
        Ok(Query {
            text: q.text,
            language: q.language,
            org: q.org,
            repo: q.repo,
            class: opt_enum_field("Query.class", q.class)?,
            grain: enum_field("Query.grain", q.grain)?,
            symbol_kind: q.symbol_kind,
            limit: opt_usize("Query.limit", q.limit)?,
            offset: opt_usize("Query.offset", q.offset)?,
        })
    }
}

impl From<SymbolQuery> for pb::SymbolQuery {
    fn from(q: SymbolQuery) -> Self {
        pb::SymbolQuery {
            pattern: q.pattern,
            kind: q.kind,
            language: q.language,
            org: q.org,
            repo: q.repo,
            file: q.file,
            limit: q.limit.map(to_u64),
            offset: q.offset.map(to_u64),
        }
    }
}

impl TryFrom<pb::SymbolQuery> for SymbolQuery {
    type Error = ConvertError;
    fn try_from(q: pb::SymbolQuery) -> Result<Self, ConvertError> {
        Ok(SymbolQuery {
            pattern: q.pattern,
            kind: q.kind,
            language: q.language,
            org: q.org,
            repo: q.repo,
            file: q.file,
            limit: opt_usize("SymbolQuery.limit", q.limit)?,
            offset: opt_usize("SymbolQuery.offset", q.offset)?,
        })
    }
}

// ----------------------------------------------------------------- Hits --

impl From<Hit> for pb::Hit {
    fn from(h: Hit) -> Self {
        pb::Hit {
            grain: enum_i32::<_, pb::Grain>(h.grain),
            org: h.org,
            repo: h.repo,
            file: h.file,
            language: h.language,
            symbol: h.symbol,
            symbol_kind: h.symbol_kind.map(enum_i32::<_, pb::SymbolKind>),
            lang_kind: h.lang_kind,
            token_class: h.token_class.map(enum_i32::<_, pb::TokenClass>),
            span: h.span.map(Into::into),
            count: to_u64(h.count),
            no_symbols: h.no_symbols,
            no_matching_symbol: h.no_matching_symbol,
        }
    }
}

impl TryFrom<pb::Hit> for Hit {
    type Error = ConvertError;
    fn try_from(h: pb::Hit) -> Result<Self, ConvertError> {
        Ok(Hit {
            grain: enum_field("Hit.grain", h.grain)?,
            org: h.org,
            repo: h.repo,
            file: h.file,
            language: h.language,
            symbol: h.symbol,
            symbol_kind: opt_enum_field("Hit.symbol_kind", h.symbol_kind)?,
            lang_kind: h.lang_kind,
            token_class: opt_enum_field("Hit.token_class", h.token_class)?,
            span: h.span.map(Span::try_from).transpose()?,
            count: to_usize("Hit.count", h.count)?,
            no_symbols: h.no_symbols,
            no_matching_symbol: h.no_matching_symbol,
        })
    }
}

impl From<SymbolHit> for pb::SymbolHit {
    fn from(h: SymbolHit) -> Self {
        pb::SymbolHit {
            org: h.org,
            repo: h.repo,
            file: h.file,
            language: h.language,
            name: h.name,
            qualified: h.qualified,
            kind: enum_i32::<_, pb::SymbolKind>(h.kind),
            lang_kind: h.lang_kind,
            span: h.span.map(Into::into),
            owner: h.owner,
        }
    }
}

impl TryFrom<pb::SymbolHit> for SymbolHit {
    type Error = ConvertError;
    fn try_from(h: pb::SymbolHit) -> Result<Self, ConvertError> {
        Ok(SymbolHit {
            org: h.org,
            repo: h.repo,
            file: h.file,
            language: h.language,
            name: h.name,
            qualified: h.qualified,
            kind: enum_field("SymbolHit.kind", h.kind)?,
            lang_kind: h.lang_kind,
            span: h.span.map(Span::try_from).transpose()?,
            owner: h.owner,
        })
    }
}

// ------------------------------------------------------------- describe --

fn counts_into(m: BTreeMap<String, usize>) -> BTreeMap<String, u64> {
    m.into_iter().map(|(k, v)| (k, to_u64(v))).collect()
}

fn counts_from(
    what: &str,
    m: BTreeMap<String, u64>,
) -> Result<BTreeMap<String, usize>, ConvertError> {
    m.into_iter()
        .map(|(k, v)| to_usize(what, v).map(|v| (k, v)))
        .collect()
}

impl From<LanguageInfo> for pb::LanguageInfo {
    fn from(l: LanguageInfo) -> Self {
        pb::LanguageInfo {
            files: to_u64(l.files),
            symbols: to_u64(l.symbols),
            tokens: to_u64(l.tokens),
            symbol_kinds: counts_into(l.symbol_kinds),
        }
    }
}

impl TryFrom<pb::LanguageInfo> for LanguageInfo {
    type Error = ConvertError;
    fn try_from(l: pb::LanguageInfo) -> Result<Self, ConvertError> {
        Ok(LanguageInfo {
            files: to_usize("LanguageInfo.files", l.files)?,
            symbols: to_usize("LanguageInfo.symbols", l.symbols)?,
            tokens: to_usize("LanguageInfo.tokens", l.tokens)?,
            symbol_kinds: counts_from("LanguageInfo.symbol_kinds", l.symbol_kinds)?,
        })
    }
}

impl From<RepoInfo> for pb::RepoInfo {
    fn from(r: RepoInfo) -> Self {
        pb::RepoInfo {
            org: r.org,
            repo: r.repo,
            files: to_u64(r.files),
            languages: r
                .languages
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            token_classes: counts_into(r.token_classes),
            open_batch: r.open_batch,
        }
    }
}

impl TryFrom<pb::RepoInfo> for RepoInfo {
    type Error = ConvertError;
    fn try_from(r: pb::RepoInfo) -> Result<Self, ConvertError> {
        Ok(RepoInfo {
            org: r.org,
            repo: r.repo,
            files: to_usize("RepoInfo.files", r.files)?,
            languages: r
                .languages
                .into_iter()
                .map(|(k, v)| LanguageInfo::try_from(v).map(|v| (k, v)))
                .collect::<Result<_, _>>()?,
            token_classes: counts_from("RepoInfo.token_classes", r.token_classes)?,
            open_batch: r.open_batch,
        })
    }
}

// ---------------------------------------------------------------- stats --

impl From<IngestStats> for pb::IngestStats {
    fn from(s: IngestStats) -> Self {
        pb::IngestStats {
            file_id: s.file_id,
            symbols: to_u64(s.symbols),
            tokens: to_u64(s.tokens),
            replaced: s.replaced,
            unchanged: s.unchanged,
            has_errors: s.has_errors,
            path: s.path,
            language: s.language,
        }
    }
}

impl TryFrom<pb::IngestStats> for IngestStats {
    type Error = ConvertError;
    fn try_from(s: pb::IngestStats) -> Result<Self, ConvertError> {
        Ok(IngestStats {
            file_id: s.file_id,
            symbols: to_usize("IngestStats.symbols", s.symbols)?,
            tokens: to_usize("IngestStats.tokens", s.tokens)?,
            replaced: s.replaced,
            unchanged: s.unchanged,
            has_errors: s.has_errors,
            path: s.path,
            language: s.language,
        })
    }
}

impl From<IndexOptions> for pb::IndexOptions {
    fn from(o: IndexOptions) -> Self {
        pb::IndexOptions { reindex: o.reindex }
    }
}

impl From<pb::IndexOptions> for IndexOptions {
    fn from(o: pb::IndexOptions) -> Self {
        IndexOptions { reindex: o.reindex }
    }
}

impl From<SnapshotStats> for pb::SnapshotStats {
    fn from(s: SnapshotStats) -> Self {
        pb::SnapshotStats {
            open_count: to_u64(s.open_count),
            // Whole milliseconds; a duration past u64 ms (584 million years)
            // saturates rather than wraps.
            oldest_age_ms: s
                .oldest_age
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            store_size_bytes: s.store_size_bytes,
        }
    }
}

impl TryFrom<pb::SnapshotStats> for SnapshotStats {
    type Error = ConvertError;
    fn try_from(s: pb::SnapshotStats) -> Result<Self, ConvertError> {
        Ok(SnapshotStats {
            open_count: to_usize("SnapshotStats.open_count", s.open_count)?,
            oldest_age: s.oldest_age_ms.map(Duration::from_millis),
            store_size_bytes: s.store_size_bytes,
        })
    }
}

impl From<VacuumStats> for pb::VacuumStats {
    fn from(s: VacuumStats) -> Self {
        pb::VacuumStats {
            terms_removed: to_u64(s.terms_removed),
            terms_kept: to_u64(s.terms_kept),
        }
    }
}

impl TryFrom<pb::VacuumStats> for VacuumStats {
    type Error = ConvertError;
    fn try_from(s: pb::VacuumStats) -> Result<Self, ConvertError> {
        Ok(VacuumStats {
            terms_removed: to_usize("VacuumStats.terms_removed", s.terms_removed)?,
            terms_kept: to_usize("VacuumStats.terms_kept", s.terms_kept)?,
        })
    }
}

impl From<CompactStats> for pb::CompactStats {
    fn from(s: CompactStats) -> Self {
        pb::CompactStats {
            before_bytes: s.before_bytes,
            after_bytes: s.after_bytes,
        }
    }
}

impl From<pb::CompactStats> for CompactStats {
    fn from(s: pb::CompactStats) -> Self {
        CompactStats {
            before_bytes: s.before_bytes,
            after_bytes: s.after_bytes,
        }
    }
}

// ----------------------------------------------------------- extraction --

impl From<SymbolDecl> for pb::SymbolDecl {
    fn from(s: SymbolDecl) -> Self {
        pb::SymbolDecl {
            name: s.name,
            kind: enum_i32::<_, pb::SymbolKind>(s.kind),
            lang_kind: s.lang_kind,
            span: Some(s.span.into()),
            owner: s.owner,
        }
    }
}

impl TryFrom<pb::SymbolDecl> for SymbolDecl {
    type Error = ConvertError;
    fn try_from(s: pb::SymbolDecl) -> Result<Self, ConvertError> {
        Ok(SymbolDecl {
            name: s.name,
            kind: enum_field("SymbolDecl.kind", s.kind)?,
            lang_kind: s.lang_kind,
            span: s
                .span
                .ok_or_else(|| missing("SymbolDecl.span"))?
                .try_into()?,
            owner: s.owner,
        })
    }
}

impl From<TokenDecl> for pb::TokenDecl {
    fn from(t: TokenDecl) -> Self {
        pb::TokenDecl {
            text: t.text,
            class: enum_i32::<_, pb::TokenClass>(t.class),
            span: Some(t.span.into()),
        }
    }
}

impl TryFrom<pb::TokenDecl> for TokenDecl {
    type Error = ConvertError;
    fn try_from(t: pb::TokenDecl) -> Result<Self, ConvertError> {
        Ok(TokenDecl {
            text: t.text,
            class: enum_field("TokenDecl.class", t.class)?,
            span: t
                .span
                .ok_or_else(|| missing("TokenDecl.span"))?
                .try_into()?,
        })
    }
}

impl From<Extraction> for pb::Extraction {
    fn from(e: Extraction) -> Self {
        pb::Extraction {
            symbols: e.symbols.into_iter().map(Into::into).collect(),
            tokens: e.tokens.into_iter().map(Into::into).collect(),
            has_errors: e.has_errors,
        }
    }
}

impl TryFrom<pb::Extraction> for Extraction {
    type Error = ConvertError;
    fn try_from(e: pb::Extraction) -> Result<Self, ConvertError> {
        Ok(Extraction {
            symbols: e
                .symbols
                .into_iter()
                .map(SymbolDecl::try_from)
                .collect::<Result<_, _>>()?,
            tokens: e
                .tokens
                .into_iter()
                .map(TokenDecl::try_from)
                .collect::<Result<_, _>>()?,
            has_errors: e.has_errors,
        })
    }
}

// ------------------------------------------------------------ FileBytes --

impl From<BatchFile<'_>> for pb::FileBytes {
    fn from(f: BatchFile<'_>) -> Self {
        pb::FileBytes {
            path: f.path.to_string(),
            bytes: f.bytes.to_vec(),
            language: f.language.map(str::to_string),
            origin: f.origin.map(str::to_string),
        }
    }
}

impl pb::FileBytes {
    /// Borrow this message as the store's batch input (`BatchFile` borrows
    /// its strings and bytes, so this is the reverse of `From<BatchFile>`).
    pub fn as_batch_file(&self) -> BatchFile<'_> {
        BatchFile {
            path: &self.path,
            bytes: &self.bytes,
            language: self.language.as_deref(),
            origin: self.origin.as_deref(),
        }
    }
}

// --------------------------------------------------------------- results --

impl From<Result<IngestStats, StoreError>> for pb::FileResult {
    fn from(r: Result<IngestStats, StoreError>) -> Self {
        pb::FileResult {
            result: Some(match r {
                Ok(s) => pb::file_result::Result::Stats(s.into()),
                Err(e) => pb::file_result::Result::Error(crate::error::WireError::from(e).into()),
            }),
        }
    }
}

impl TryFrom<pb::FileResult> for Result<IngestStats, StoreError> {
    type Error = ConvertError;
    fn try_from(r: pb::FileResult) -> Result<Self, ConvertError> {
        match r.result.ok_or_else(|| missing("FileResult.result"))? {
            pb::file_result::Result::Stats(s) => Ok(Ok(s.try_into()?)),
            pb::file_result::Result::Error(d) => {
                let e = crate::error::WireError::try_from(d)?;
                Ok(Err(e.into()))
            }
        }
    }
}

// ----------------------------------------------------------------- View --

/// How a read request is served: the Rust form of `pb::View`. An absent or
/// empty message is [`View::Local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Read this node's store as it is (may lag the leader).
    #[default]
    Local,
    /// Confirm leadership before reading.
    Linearizable,
    /// A server-side snapshot handle from `Store.OpenSnapshot`.
    Snapshot(u64),
}

impl From<View> for pb::View {
    fn from(v: View) -> Self {
        pb::View {
            v: Some(match v {
                View::Local => pb::view::V::Mode(pb::ReadMode::Local as i32),
                View::Linearizable => pb::view::V::Mode(pb::ReadMode::Linearizable as i32),
                View::Snapshot(id) => pb::view::V::SnapshotId(id),
            }),
        }
    }
}

impl TryFrom<pb::View> for View {
    type Error = ConvertError;
    fn try_from(v: pb::View) -> Result<Self, ConvertError> {
        Ok(match v.v {
            None => View::Local,
            Some(pb::view::V::SnapshotId(id)) => View::Snapshot(id),
            Some(pb::view::V::Mode(m)) => match pb::ReadMode::try_from(m) {
                Ok(pb::ReadMode::Local) => View::Local,
                Ok(pb::ReadMode::Linearizable) => View::Linearizable,
                Err(_) => return Err(unspecified("ReadMode", m)),
            },
        })
    }
}

impl TryFrom<Option<pb::View>> for View {
    type Error = ConvertError;
    /// The `view` field of a request; absent means LOCAL.
    fn try_from(v: Option<pb::View>) -> Result<Self, ConvertError> {
        v.map_or(Ok(View::Local), View::try_from)
    }
}
