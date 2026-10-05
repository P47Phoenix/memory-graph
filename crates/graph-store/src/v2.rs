//! Storage format v2 on redb (ADR 0003 stories 2-4, first slice): an interned
//! dictionary, one compact stream per file (tokens and symbols, see
//! [`crate::codec`]) and count postings `(term, file) -> count`. Tokens are not
//! rows. Orgs, repos and files are entity rows (JSON `Node`s in the shared
//! entity tables, see `common.rs`); symbols and tokens are addressed by ids
//! that encode `(file, index)`.
//!
//! This is the only storage format (the per-node "v1" layout was retired,
//! ADR 0003 D5). It sits behind [`Store`] via `open_store` and is checked by
//! the conformance suite (`run_all`) and, across its own configurations
//! (chunk size, cache size, jobs, compaction), by `run_differential`.
//!
//! Ids: `tag(2) | file_local(30) | index(32)`. Tag 0 is an entity (org, repo,
//! file; `file_local` is the allocation counter and `index` is 0), tag 1 a
//! symbol (`index` = position in the file's symbol section) and tag 2 a token
//! (`index` = ordinal). Ids of symbols and tokens are stable only until the
//! file is re-indexed (ADR Q1).
//!
//! The on-disk file is stamped with [`V2_SCHEMA_VERSION`]; any other version
//! is refused before anything is written (a retired v1 file with
//! [`StoreError::LegacyFormat`], anything else with `SchemaMismatch`).
use crate::codec::{self, Lazy, Stream, SymRec, TokRec, STREAM_FORMAT};
use crate::read_stats::{self, Counter};
use crate::{check_unchanged, dec, describe_in, enc, SnapshotStats, Store, StoreRead};
use crate::{
    commit_prepared, prepare_file, stored_fingerprint_matches, stored_fingerprints, PreparedFile,
};
use crate::{
    grain_accepts, kind_label, kind_matches, name_key, open_failed, validate_spans, BatchFile,
    Grain, Hit, IndexOptions, IngestStats, Query, RepoInfo, Scope, StoreError, SymbolHit,
    SymbolQuery, Tally, CATALOG, CATALOG_VERSION, CHILDREN, META, NAMES, NODES, ORIGIN_DIRECTORY,
    SYMBOLS,
};
use graph_core::{
    normalize_path, Extraction, Extractor, Node, NodeId, NodeKind, Registry, SymbolKind, TokenClass,
};
use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadTransaction, ReadableMultimapTable,
    ReadableTable, ReadableTableMetadata, TableDefinition,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, StoreError>;

/// Layout version of the database file (the retired v1 layout used 1 and 2,
/// see [`crate::LEGACY_SCHEMA_VERSIONS`]).
///
/// Bumped to 8 for story 6 (ADR 0003, D1): `POST` values are now
/// block-encoded (`codec::POSTING_BLOCK`-sized, self-contained blocks)
/// instead of one flat delta-varint run, so old v2 files are refused rather
/// than misread (v2 is unreleased; no migration is attempted).
///
/// Bumped to 10 for issue #137: a stream's flag byte may set bit 1
/// (`has_owners`, see `codec`), which a version-9 reader would ignore and
/// then misread the symbol section. Every version-9 stream is still a valid
/// version-10 stream (bit 1 clear), so a version-9 file is upgraded on open
/// by restamping `schema_version` only ([`UPGRADABLE_SCHEMA_VERSIONS`]); no
/// data is rewritten, and an older binary then refuses the file with
/// `SchemaMismatch` instead of misreading it.
///
/// Bumped to 11 for ADR 0007 (source encodings): a File node may carry
/// `encoding` and `lossy`, which a version-10 reader would drop on a
/// rewrite. Both are omitted for UTF-8, so every version-9/10 File node is
/// a valid version-11 one (absence means UTF-8) and the catalog needs no new
/// entries for all-UTF-8 content: a 9 or 10 file is restamped straight to 11
/// (now 12, below).
///
/// Bumped to 12 for epic story 43: the catalog gains per-repo `e`
/// (encoding) and `l` (lossy) entries, which a version-11 reader refuses as
/// a bad catalog key and a version-11 writer would leave stale. A 9 or 10
/// file is restamped to 12 with [`CATALOG_VERSION`] 2 (all UTF-8, nothing to
/// count); an 11 file is restamped with its encoding counts recounted from
/// the File nodes, in the same commit. Older binaries then refuse a 12 file
/// with `SchemaMismatch`.
pub const V2_SCHEMA_VERSION: u64 = 12;

/// The earlier layout versions [`V2Store`] upgrades in place on open (a
/// restamp, see [`V2_SCHEMA_VERSION`]); every other version is refused.
pub const UPGRADABLE_SCHEMA_VERSIONS: &[u64] = &[9, 10, 11];

/// Version of the `refs`/`content_files` derived tables (ADR 0003 story 9).
/// Unlike `V2_SCHEMA_VERSION` (a hard gate on the on-disk *layout*), this is
/// a soft, self-healing counter stored in `meta` under
/// [`DERIVED_VERSION_REFS_KEY`]: it does not change what tables exist or how
/// they are keyed, only whether their *contents* are known-fresh. A file
/// whose stored value is absent (pre-story-9 v2 file) or lower than this
/// constant is never refused -- `V2Store::open`/`open_with_cache_bytes`
/// silently calls `rebuild_refs` and stamps the current value in the same
/// write transaction. Bump it whenever `rebuild_refs`'s output would change
/// for existing data (i.e. whenever the refs/content_files derivation rule
/// itself changes).
pub const REFS_DERIVED_VERSION: u64 = 1;
/// `meta` key holding the stored [`REFS_DERIVED_VERSION`] a file was last
/// rebuilt/stamped at.
pub(crate) const DERIVED_VERSION_REFS_KEY: &str = "derived_version_refs_content_files";

/// term text -> term id.
pub(crate) const DICT: TableDefinition<&str, u64> = TableDefinition::new("dict");
/// term id -> term text, packed (ADR 0003 story 5, decision D1: "packed
/// single sorted dictionary"). One row per up-to-[`codec::DICT_BLOCK`]
/// entries (`block index -> codec::encode_dict_block` bytes) instead of one
/// row per term; blocks are always in ascending-id order (row 0 holds the
/// smallest ids), so a point lookup binary-searches block indexes by each
/// candidate block's first id ([`codec::dict_block_first_id`]) and then
/// linearly scans the found block. Term ids are assigned by a monotonic
/// counter and never reused, so `intern` always appends the newest id to the
/// last (possibly partial) block -- no reordering, no rewrite of any other
/// block. `vacuum` repacks the whole table densely when it removes a dead
/// term, which is also when block boundaries stop lining up with
/// `id / DICT_BLOCK` (dead ids leave gaps); the binary search does not
/// assume dense boundaries, so it is correct either way.
pub(crate) const DICT_REV: TableDefinition<u64, &[u8]> = TableDefinition::new("dict_rev_blocks");
/// file id -> encoded stream.
pub(crate) const STREAMS: TableDefinition<u64, &[u8]> = TableDefinition::new("stream");
/// (term id, file id) -> occurrence count and token ordinals (see `codec::encode_posting`).
const POST: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("post");
/// content id -> refcount (ADR 0003 story 3, Q2). While content sharing
/// (story 18) is off, `content_id == file id` (see [`content_id`]), so a live
/// file's refcount is always exactly 1; this table and `CONTENT_FILES` exist
/// from day one so enabling fan-out later is not a format change.
pub(crate) const REFS: TableDefinition<u64, u64> = TableDefinition::new("refs");
/// content id -> the file ids currently sharing that content.
pub(crate) const CONTENT_FILES: MultimapTableDefinition<u64, u64> =
    MultimapTableDefinition::new("content_files");

/// The in-progress chunked-ingest marker (ADR 0003 story 3, decision D3,
/// "Chunked-ingest visibility"): `"org"`/`"repo"` are present iff a chunked
/// `index_batch` currently has an open, not-yet-finalized batch. The numeric
/// `batch_id` lives in `META` (`"open_batch_id"`), since `META`'s value type
/// is `u64` and org/repo are strings, hence this second, string-valued table
/// rather than shoehorning them into `META`. `META.next_batch_id` is the
/// monotonic counter that stamps each batch, mirroring `next_id`/`next_term`.
/// Every chunk-commit transaction of `index_batch` (re)writes both this table
/// and `META.open_batch_id` in the same transaction as the chunk's data; the
/// final chunk's transaction clears both instead. This slice is write-path
/// only -- no reader (`describe`/search) surfaces this yet (slice 3o) -- and
/// deliberately does not build a `commit_epoch` counter, which only matters
/// once a manifest/sharding protocol (stories 14-17) exists; "is a batch
/// open" is fully answerable from this marker alone.
pub(crate) const OPEN_BATCH: TableDefinition<&str, &str> = TableDefinition::new("open_batch");

/// Raft state-machine bookkeeping (ADR 0004 D7): `"last_applied"` holds a
/// [`RaftMarker`] (24 bytes, see [`RaftMarker::encode`]) and `"membership"`
/// the opaque bytes the server hands in. The one recorded exception to the
/// version-bump rule: a compatible addition that is **never** created by
/// [`V2Store::open`] (so an embedded reopen stays byte-identical and never
/// gains the table) and is created lazily by the first marked write
/// (`index_prepared_marked` and friends). Readers guard
/// `TableDoesNotExist` and report "no marker".
pub(crate) const RAFT_SM: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_sm");
const RAFT_LAST_APPLIED: &str = "last_applied";
const RAFT_MEMBERSHIP: &str = "membership";

/// The last Raft log entry applied to a store (ADR 0004 D5/D7), stored in
/// [`RAFT_SM`] by the marked writes. Applying is exactly-once by this
/// marker: a marked write whose `index` is not above the stored one is
/// refused without writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RaftMarker {
    pub term: u64,
    pub index: u64,
    pub node_id: u64,
}

impl RaftMarker {
    /// Stored size of the marker: three little-endian `u64`s.
    pub const ENCODED_LEN: usize = 24;

    /// `term`, `index`, `node_id` as little-endian `u64`s, in that order
    /// (no prost, no serde: 24 fixed bytes; golden-byte tested).
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..8].copy_from_slice(&self.term.to_le_bytes());
        out[8..16].copy_from_slice(&self.index.to_le_bytes());
        out[16..].copy_from_slice(&self.node_id.to_le_bytes());
        out
    }

    /// Inverse of [`encode`](Self::encode); any other length is `Corrupt`.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let arr: &[u8; Self::ENCODED_LEN] = bytes.try_into().map_err(|_| {
            StoreError::Corrupt(format!(
                "raft marker is {} bytes, expected {}",
                bytes.len(),
                Self::ENCODED_LEN
            ))
        })?;
        let u = |i: usize| u64::from_le_bytes(arr[i..i + 8].try_into().unwrap());
        Ok(Self {
            term: u(0),
            index: u(8),
            node_id: u(16),
        })
    }
}

/// The content id backing `file`'s stream/postings/symbol-index rows. Today
/// this is the identity (content sharing is off, ADR 0003 story 3 Q2: "skipped
/// unchanged files never touch refcounts", and every ingested file owns its
/// content alone), so a file's refcount is always exactly 1. This function is
/// the seam for story 18's future content-sharing fan-out (dedup by digest):
/// when that lands, only this function's body changes -- every refs/
/// content_files caller already goes through it instead of using `file`
/// directly as the content key.
pub(crate) fn content_id(file: u64) -> u64 {
    file
}

/// Term-length policy (ADR 0003 story 3). The dictionary keeps a term inline
/// as its own key while it is at most this many bytes and does not start
/// with NUL. Any other term (very long, or NUL-leading) is keyed by
/// `"\0" + sha256 hex` (plus `.n` when two different texts ever share a
/// digest), so the B-tree keys stay small and a term's text is stored once,
/// in `dict_rev`, whole and exact. Inline keys never start with NUL and
/// hashed keys always do, so the two key spaces cannot collide. Every lookup
/// verifies the stored text, so a digest collision cannot merge two terms.
/// Spans are unaffected (they are stored in the stream). Symbol names are
/// not capped: `sym_idx` is range-scanned by prefix and needs the text.
pub const MAX_INLINE_TERM: usize = 256;

pub(crate) fn hashed_key(text: &str, n: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut k = String::with_capacity(70);
    k.push('\0');
    for b in Sha256::digest(text.as_bytes()) {
        k.push_str(&format!("{b:02x}"));
    }
    if n > 0 {
        k.push_str(&format!(".{n}"));
    }
    k
}

fn is_hashed(text: &str) -> bool {
    text.len() > MAX_INLINE_TERM || text.starts_with('\0')
}

/// Point lookup of `id`'s text in the packed reverse dictionary (ADR 0003
/// story 5): binary-search the block whose first id is the largest one
/// `<= id` (a "floor" search over `dict_block_first_id`, which is a
/// one-varint peek and does not decode a candidate block's other entries),
/// then linearly scan that one block (at most `codec::DICT_BLOCK` entries)
/// for the exact id. `Ok(None)` for an id that was never assigned, or was
/// removed by `vacuum`. Generic over `redb::Table`/`redb::ReadOnlyTable`
/// (both implement `ReadableTable`) so the read (`R`) and write (`W`) sides
/// share one implementation.
fn dict_rev_lookup<T: ReadableTable<u64, &'static [u8]> + ReadableTableMetadata>(
    t: &T,
    id: u64,
) -> Result<Option<String>> {
    let Some(block) = dict_rev_block(t, id)? else {
        return Ok(None);
    };
    let entries = codec::decode_dict_block(block.value())?;
    Ok(find_dict_entry(entries, id))
}

/// [`dict_rev_lookup`] for queries: the block decode is counted (and, with
/// timing on, timed) in [`crate::read_stats`]. Writes use the uncounted one.
fn dict_rev_lookup_counted<T: ReadableTable<u64, &'static [u8]> + ReadableTableMetadata>(
    t: &T,
    id: u64,
) -> Result<Option<String>> {
    let Some(block) = dict_rev_block(t, id)? else {
        return Ok(None);
    };
    let entries = read_stats::timed(Counter::DictBlockDecodes, Counter::DictNanos, || {
        codec::decode_dict_block(block.value())
    })?;
    Ok(find_dict_entry(entries, id))
}

fn find_dict_entry(entries: Vec<(u64, String)>, id: u64) -> Option<String> {
    entries.into_iter().find(|(i, _)| *i == id).map(|(_, t)| t)
}

/// The reverse-dictionary block that would hold `id`, if any.
fn dict_rev_block<T: ReadableTable<u64, &'static [u8]> + ReadableTableMetadata>(
    t: &T,
    id: u64,
) -> Result<Option<redb::AccessGuard<'_, &'static [u8]>>> {
    let n = t.len()?;
    if n == 0 {
        return Ok(None);
    }
    let (mut lo, mut hi) = (0u64, n); // half-open [lo, hi)
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let block = t
            .get(mid)?
            .ok_or_else(|| StoreError::Corrupt(format!("missing dict block {mid}")))?;
        let first = codec::dict_block_first_id(block.value())?
            .ok_or_else(|| StoreError::Corrupt(format!("empty dict block {mid}")))?;
        if first <= id {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        return Ok(None);
    }
    Ok(t.get(lo - 1)?)
}

/// Read transactions opened by `prepare`'s one-file pre-check, process-wide
/// (#172). Tests use it to prove that `prepare_with` with a matching
/// snapshot, and so the CLI's parse threads, never open one.
static PRECHECK_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many pre-check read transactions `prepare` has opened in this
/// process. For tests only; not a stable API.
#[doc(hidden)]
pub fn precheck_reads() -> u64 {
    PRECHECK_READS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The encoded size at which [`dict_rev_append`] stops extending the last
/// reverse-dictionary block and starts a new one (#162).
pub(crate) const DICT_BLOCK_MAX_BYTES: usize = 64 << 10;

/// Append `(id, text)` to the packed reverse dictionary (ADR 0003 story 5):
/// `id` must be larger than every id already stored (true of every call from
/// `intern`, since term ids come from a monotonic counter). Extends the last
/// block in place while it has room for another entry, otherwise starts a
/// new block -- so this touches exactly one row, not the whole table.
///
/// A last block of [`DICT_BLOCK_MAX_BYTES`] or more is also full, whatever
/// its entry count (#162): extending it rewrites it whole, so a block that
/// holds one very long term (a 20 MiB comment token) made every later new
/// term cost a 20 MiB decode, encode and insert until the block filled, and
/// a 100-small-file batch after such a file took 0.8-2.7 s instead of 8 ms.
/// Readers never assumed `DICT_BLOCK` entries per block (vacuum already
/// leaves uneven boundaries), so this is a write policy, not a format change.
pub(crate) fn dict_rev_append(
    rev: &mut redb::Table<u64, &'static [u8]>,
    id: u64,
    text: &str,
) -> Result<()> {
    let n = rev.len()?;
    if n > 0 {
        let last = n - 1;
        let block = rev.get(last)?.unwrap();
        let encoded_len = block.value().len();
        let count = codec::dict_block_entry_count(block.value())?;
        if !dict_block_is_full(encoded_len, count) {
            // Decode only a block that will be extended: a full one may hold
            // a multi-MiB term (#162).
            let mut entries = codec::decode_dict_block(block.value())?;
            drop(block);
            entries.push((id, text.to_string()));
            let refs: Vec<(u64, &str)> = entries.iter().map(|(i, t)| (*i, t.as_str())).collect();
            rev.insert(last, codec::encode_dict_block(&refs).as_slice())?;
            return Ok(());
        }
    }
    rev.insert(n, codec::encode_dict_block(&[(id, text)]).as_slice())?;
    Ok(())
}

/// The one block-boundary rule for the reverse dictionary, shared by
/// [`dict_rev_append`] and [`pack_dict_blocks`]: a block is full once it
/// holds [`codec::DICT_BLOCK`] entries or encodes to
/// [`DICT_BLOCK_MAX_BYTES`] or more.
fn dict_block_is_full(encoded_len: usize, entries: usize) -> bool {
    encoded_len >= DICT_BLOCK_MAX_BYTES || entries >= codec::DICT_BLOCK
}

/// Pack `entries` (ascending ids) into encoded reverse-dictionary blocks,
/// filling each block until [`dict_block_is_full`], exactly as a sequence of
/// [`dict_rev_append`] calls would (#188: vacuum's repack used to split by
/// entry count only, so a long term could share a block with small ones).
pub(crate) fn pack_dict_blocks(entries: &[(u64, String)]) -> Vec<Vec<u8>> {
    let mut blocks = Vec::new();
    let mut current: Vec<(u64, &str)> = Vec::new();
    // Running size of `current`'s entries; the count header (which grows a
    // byte at 128 entries) is added when checking.
    let mut body_len = 0usize;
    for (id, text) in entries {
        let encoded_len = codec::dict_block_count_len(current.len()) + body_len;
        if !current.is_empty() && dict_block_is_full(encoded_len, current.len()) {
            blocks.push(codec::encode_dict_block(&current));
            current.clear();
            body_len = 0;
        }
        body_len += codec::dict_entry_encoded_len(*id, text);
        current.push((*id, text.as_str()));
    }
    if !current.is_empty() {
        blocks.push(codec::encode_dict_block(&current));
    }
    blocks
}

const TAG_SYM: u64 = 1;
const TAG_TOK: u64 = 2;
const MAX_LOCAL: u64 = (1 << 30) - 1;

fn sub_id(tag: u64, file: u64, index: usize) -> u64 {
    tag << 62 | file << 32 | index as u64
}

/// `(tag, file, index)` of an id.
fn split_id(id: u64) -> (u64, u64, usize) {
    (
        id >> 62,
        (id >> 32) & MAX_LOCAL,
        (id & 0xffff_ffff) as usize,
    )
}

fn blank(id: NodeId, parent: Option<NodeId>, kind: NodeKind, name: String) -> Node {
    Node {
        id,
        parent,
        kind,
        name,
        language: None,
        symbol_kind: None,
        lang_kind: None,
        token_class: None,
        has_errors: false,
        origin: None,
        fingerprint: None,
        encoding: None,
        lossy: false,
        span: None,
    }
}

/// Result of [`Store::vacuum`] (dictionary terms; a backend without a dictionary reports zeros).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VacuumStats {
    pub terms_removed: usize,
    pub terms_kept: usize,
}

/// Result of [`V2Store::compact`]: the file size before and after the rebuild.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// Default cap on the source bytes one `index_batch` write transaction
/// takes in before it commits and starts the next (see [`V2Store::index_batch`]
/// docs on the `Store` impl for the semantics).
pub const DEFAULT_CHUNK_BYTES: usize = 64 << 20;

/// Default max snapshot age (ADR 0003 story 10, Q6's decision): a snapshot
/// handle older than this refuses reads with `StoreError::SnapshotExpired`.
/// Configurable per store via [`V2Store::set_max_snapshot_age`].
pub const DEFAULT_MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(15 * 60);

/// Fraction of the max age at which a snapshot logs one age warning to
/// stderr (ADR 0003 Q6: "a warning is emitted at 50% of the limit"), the
/// same `eprintln!` convention `V2Store::open`'s self-heal notice already
/// uses -- this crate has no logging dependency.
const SNAPSHOT_WARN_FRACTION: f64 = 0.5;

/// Per-store bookkeeping for currently open snapshot handles (ADR 0003
/// story 10 observability): every `V2Snapshot` registers itself here on
/// creation and deregisters on drop, so `V2Store::snapshot_stats` can report
/// the live count and the oldest handle's age without scanning anything.
#[derive(Default)]
struct SnapshotTracker {
    next_id: u64,
    open: HashMap<u64, Instant>,
}

type SharedSnapshotTracker = Arc<Mutex<SnapshotTracker>>;

/// The v2 backend: one redb file.
pub struct V2Store {
    pub(crate) db: Database,
    registry: Registry,
    pub(crate) chunk_bytes: usize,
    /// Test-only (#154): skip the open-batch marker writes, so a timing test
    /// can separate the marker's cost from the raw per-chunk commit cost.
    /// The field exists only under `cfg(test)`, and in every other
    /// build `commit_each_counted` uses a constant `false`, so the marker is
    /// always written.
    #[cfg(test)]
    pub(crate) skip_open_batch_marker: bool,
    pub(crate) cache_bytes: Option<usize>,
    path: PathBuf,
    max_snapshot_age: Duration,
    snapshot_tracker: SharedSnapshotTracker,
    marked_commit_hook: Option<MarkedCommitHook>,
}

/// A test hook the server's failpoints use (ADR 0004 stage B): called by
/// every marked write inside its write transaction, after the data and the
/// marker are staged and before the commit, with the entry's marker. An
/// `Err` aborts the transaction (nothing of the entry is written) and is
/// returned to the caller, exactly as an I/O failure at that point would.
pub type MarkedCommitHook = Arc<dyn Fn(&RaftMarker) -> Result<()> + Send + Sync>;

pub struct V2Snapshot {
    rt: ReadTransaction,
    tracker: SharedSnapshotTracker,
    tracker_id: u64,
    created_at: Instant,
    max_age: Duration,
    /// Set once the 50%-of-max-age warning has fired for this handle, so it
    /// logs at most once per snapshot rather than once per read.
    warned: Cell<bool>,
}

impl Drop for V2Snapshot {
    fn drop(&mut self) {
        // A poisoned lock still lets us recover the map and finish the
        // deregistration; a snapshot's own read transaction is unaffected
        // by a panic in some other thread's snapshot bookkeeping.
        let mut t = self
            .tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        t.open.remove(&self.tracker_id);
    }
}

impl V2Snapshot {
    /// Checked at the top of every `StoreRead` method (see the
    /// `store_read!` macro): a snapshot past `max_age` refuses the read with
    /// `SnapshotExpired` rather than only checking once, at `snapshot()`
    /// time. This matters because a snapshot is meant to be held across
    /// multiple calls (ADR 0003 story 11, paging/traversal); checking only
    /// at issuance would let a long-held handle silently keep reading a
    /// frozen view well past the configured limit, defeating the point of
    /// having one. A snapshot that has passed 50% of its max age (but not
    /// yet expired) logs one warning, per ADR 0003 Q6.
    fn check_not_expired(&self) -> Result<()> {
        let age = self.created_at.elapsed();
        if age >= self.max_age {
            return Err(StoreError::SnapshotExpired {
                age_secs: age.as_secs(),
                max_age_secs: self.max_age.as_secs(),
            });
        }
        if !self.warned.get()
            && age.as_secs_f64() >= self.max_age.as_secs_f64() * SNAPSHOT_WARN_FRACTION
        {
            self.warned.set(true);
            eprintln!(
                "memory-graph: v2 snapshot age {}s has passed {:.0}% of its {}s max age; \
                 it will start refusing reads with SnapshotExpired once it reaches the limit",
                age.as_secs(),
                SNAPSHOT_WARN_FRACTION * 100.0,
                self.max_age.as_secs(),
            );
        }
        Ok(())
    }
}

impl V2Store {
    /// No-op: only snapshot *handles* age out, not the store's own
    /// always-current read path. Same name and signature as
    /// `V2Snapshot::check_not_expired` so the `store_read!` macro can call
    /// `$s.check_not_expired()` on either type.
    fn check_not_expired(&self) -> Result<()> {
        Ok(())
    }
}

/// Read side over one read transaction.
pub(crate) struct R {
    nodes: redb::ReadOnlyTable<u64, &'static [u8]>,
    names: redb::ReadOnlyTable<&'static str, u64>,
    pub(crate) streams: redb::ReadOnlyTable<u64, &'static [u8]>,
    pub(crate) dict: redb::ReadOnlyTable<&'static str, u64>,
    pub(crate) rev: redb::ReadOnlyTable<u64, &'static [u8]>,
    pub(crate) post: redb::ReadOnlyTable<(u64, u64), &'static [u8]>,
    sym_idx: redb::ReadOnlyMultimapTable<&'static str, u64>,
    cat: redb::ReadOnlyTable<&'static str, u64>,
    kids: redb::ReadOnlyMultimapTable<u64, u64>,
    // Read only by `check_consistency` today (no read-path query needs
    // per-content refcounts yet); kept on `R` rather than opened ad hoc so
    // that seam stays in one place alongside the other derived tables.
    #[allow(dead_code)]
    pub(crate) refs: redb::ReadOnlyTable<u64, u64>,
    #[allow(dead_code)]
    pub(crate) content_files: redb::ReadOnlyMultimapTable<u64, u64>,
    /// The D3 chunked-ingest marker (ADR 0003 story 3), read by
    /// `describe_by_scan` alongside `describe_in`, from this same read
    /// transaction, so the reference scan agrees with the fast catalog path.
    open_batch: redb::ReadOnlyTable<&'static str, &'static str>,
    /// Per-query caches: dictionary texts and org/repo rows are read once.
    texts: RefCell<HashMap<u64, Rc<str>>>,
    ents: RefCell<HashMap<u64, Rc<Node>>>,
}

/// One file with its containment path.
struct FileCtx {
    org: Rc<Node>,
    repo: Rc<Node>,
    file: Node,
}

/// A symbol or token of one stream, by index.
#[derive(Clone, Copy)]
enum Item {
    Sym(usize),
    Tok(usize),
}

/// Containment inside one stream: top-level items and the children of each
/// symbol, both in creation (source) order, symbols first on equal starts.
struct Tree {
    top: Vec<Item>,
    kids: Vec<Vec<Item>>,
}

fn stream_tree(s: &Stream) -> Tree {
    let mut t = Tree {
        top: Vec::new(),
        kids: vec![Vec::new(); s.symbols.len()],
    };
    let (mut si, mut ti) = (0, 0);
    while si < s.symbols.len() || ti < s.tokens.len() {
        let take_sym = si < s.symbols.len()
            && (ti >= s.tokens.len() || s.symbols[si].span.start <= s.tokens[ti].span.start);
        let (item, parent) = if take_sym {
            si += 1;
            (Item::Sym(si - 1), s.symbols[si - 1].parent)
        } else {
            ti += 1;
            (Item::Tok(ti - 1), s.tokens[ti - 1].parent)
        };
        match parent {
            Some(p) => t.kids[p as usize].push(item),
            None => t.top.push(item),
        }
    }
    t
}

/// The ordinals in `first..=last` NOT covered by any of the given (disjoint,
/// start-sorted) `(first, last)` child ranges, in ascending order: the gaps
/// between consecutive child ranges plus the two ends. Shared by
/// [`direct_token_ordinals`] (children of a symbol, range is the symbol's
/// own `toks`) and [`top_level_ordinals`] (children of a file, range is the
/// whole `0..ntok`).
fn range_gaps(first: u32, last: u32, child_ranges: impl Iterator<Item = (u32, u32)>) -> Vec<usize> {
    let mut ords = Vec::new();
    let mut cur = first;
    for (cf, cl) in child_ranges {
        if cf > cur {
            ords.extend((cur..cf).map(|o| o as usize));
        }
        cur = cl + 1;
    }
    if cur <= last {
        ords.extend((cur..=last).map(|o| o as usize));
    }
    ords
}

/// The ordinals directly under symbol `i` (not under any of its listed
/// direct children `child_idxs`, which must be sorted ascending -- true of
/// any direct-child list taken from a start-sorted symbol section), in
/// ascending order: the gaps between `syms[i]`'s own range and the union of
/// its children's ranges. Empty when `syms[i].toks` is `None` (no tokens
/// transitively under it, so none directly under it either).
/// Symbol-to-direct-children index built once per file and reused across
/// every top-level symbol's subtree walk (see issue #40).
fn sym_kids_map(syms: &[SymRec]) -> Vec<Vec<usize>> {
    let mut sym_kids: Vec<Vec<usize>> = vec![Vec::new(); syms.len()];
    for (j, s) in syms.iter().enumerate() {
        if let Some(p) = s.parent {
            sym_kids[p as usize].push(j);
        }
    }
    sym_kids
}

fn direct_token_ordinals(syms: &[SymRec], i: usize, child_idxs: &[usize]) -> Vec<usize> {
    let Some((first, last)) = syms[i].toks else {
        return Vec::new();
    };
    range_gaps(first, last, child_idxs.iter().filter_map(|&c| syms[c].toks))
}

/// The ordinals in `0..ntok` NOT covered by any of the given (disjoint,
/// start-sorted) top-level symbol index ranges `top_idxs` -- the file-level
/// (root) analog of [`direct_token_ordinals`], used by `children`/
/// `descendants` on a file (ADR 0003 story 3, slice 3j; measured as a >30%
/// decoded-record reduction on this repo's own `crates/` tree -- see
/// `file_level_complement_decode_cost_measured_on_this_repos_own_corpus` --
/// which met the scoping plan's build gate).
fn top_level_ordinals(syms: &[SymRec], top_idxs: &[usize], ntok: usize) -> Vec<usize> {
    if ntok == 0 {
        return Vec::new();
    }
    range_gaps(
        0,
        (ntok - 1) as u32,
        top_idxs.iter().filter_map(|&j| syms[j].toks),
    )
}

/// Top-level symbol indexes of `syms` (no parent), in the start-sorted order
/// the (start-sorted) symbol section already stores them in.
fn top_level_idxs(syms: &[SymRec]) -> Vec<usize> {
    (0..syms.len())
        .filter(|&j| syms[j].parent.is_none())
        .collect()
}

impl R {
    pub(crate) fn new(rt: &ReadTransaction) -> Result<Self> {
        Ok(Self {
            kids: rt.open_multimap_table(CHILDREN)?,
            texts: RefCell::default(),
            ents: RefCell::default(),
            nodes: rt.open_table(NODES)?,
            names: rt.open_table(NAMES)?,
            streams: rt.open_table(STREAMS)?,
            dict: rt.open_table(DICT)?,
            rev: rt.open_table(DICT_REV)?,
            post: rt.open_table(POST)?,
            sym_idx: rt.open_multimap_table(SYMBOLS)?,
            cat: rt.open_table(CATALOG)?,
            refs: rt.open_table(REFS)?,
            content_files: rt.open_multimap_table(CONTENT_FILES)?,
            open_batch: rt.open_table(OPEN_BATCH)?,
        })
    }

    /// The D3 marker's org/repo, if a chunked batch is currently open
    /// (crashed or in-progress), read from this `R`'s own transaction.
    fn open_batch_marker(&self) -> Result<Option<(String, String)>> {
        let org = self.open_batch.get("org")?.map(|v| v.value().to_string());
        let repo = self.open_batch.get("repo")?.map(|v| v.value().to_string());
        Ok(org.zip(repo))
    }

    fn node(&self, id: u64) -> Result<Option<Node>> {
        match self.nodes.get(id)? {
            Some(v) => Ok(Some(dec(v.value())?)),
            None => Ok(None),
        }
    }

    fn need(&self, id: u64) -> Result<Node> {
        self.node(id)?
            .ok_or_else(|| StoreError::Corrupt(format!("dangling node {id}")))
    }

    fn find(&self, parent: Option<NodeId>, kind: NodeKind, name: &str) -> Result<Option<NodeId>> {
        Ok(self
            .names
            .get(name_key(parent, kind, name).as_str())?
            .map(|v| v.value()))
    }

    fn stream(&self, file: u64) -> Result<Option<Stream>> {
        match self.streams.get(file)? {
            Some(v) => Ok(Some(codec::decode(v.value())?)),
            None => Ok(None),
        }
    }

    /// Runs `f` against the header of one file's stream, with symbols and
    /// tokens decoded on demand (see [`Lazy`]) instead of eagerly, for
    /// callers that need only a small piece of a possibly large file.
    /// `Ok(None)` when the file has no stream row. The [`Lazy`] borrows the
    /// raw bytes for the life of this call only (redb's `AccessGuard` does
    /// not outlive it), so it cannot be returned on its own.
    fn with_lazy<T>(&self, file: u64, f: impl FnOnce(&Lazy) -> Result<T>) -> Result<Option<T>> {
        match self.streams.get(file)? {
            Some(v) => Ok(Some(f(&codec::decode_lazy(v.value())?)?)),
            None => Ok(None),
        }
    }

    /// The term id of `text`, if it is in the dictionary.
    fn lookup(&self, text: &str) -> Result<Option<u64>> {
        if !is_hashed(text) {
            return Ok(self.dict.get(text)?.map(|v| v.value()));
        }
        for n in 0.. {
            let Some(id) = self
                .dict
                .get(hashed_key(text, n).as_str())?
                .map(|v| v.value())
            else {
                return Ok(None);
            };
            if dict_rev_lookup_counted(&self.rev, id)?.is_some_and(|t| t == text) {
                return Ok(Some(id));
            }
        }
        unreachable!()
    }

    /// A dictionary text, cached for the life of this query.
    pub(crate) fn text(&self, term: u64) -> Result<Rc<str>> {
        if let Some(t) = self.texts.borrow().get(&term) {
            read_stats::bump(Counter::DictMemoHits);
            return Ok(Rc::clone(t));
        }
        read_stats::bump(Counter::DictMemoMisses);
        let t: Rc<str> = dict_rev_lookup_counted(&self.rev, term)?
            .ok_or_else(|| StoreError::Corrupt(format!("dangling term {term}")))?
            .into();
        self.texts.borrow_mut().insert(term, Rc::clone(&t));
        Ok(t)
    }

    /// An org or repo row, cached for the life of this query.
    fn entity(&self, id: u64) -> Result<Rc<Node>> {
        if let Some(n) = self.ents.borrow().get(&id) {
            return Ok(Rc::clone(n));
        }
        let n = Rc::new(self.need(id)?);
        self.ents.borrow_mut().insert(id, Rc::clone(&n));
        Ok(n)
    }

    fn sym_node(&self, file: u64, i: usize, syms: &[SymRec]) -> Result<Node> {
        let r = &syms[i];
        let parent = r.parent.map_or(file, |p| sub_id(TAG_SYM, file, p as usize));
        let mut n = blank(
            sub_id(TAG_SYM, file, i),
            Some(parent),
            NodeKind::Symbol,
            self.text(r.name)?.to_string(),
        );
        n.symbol_kind = Some(r.kind);
        n.lang_kind = r
            .lang_kind
            .map(|k| self.text(k))
            .transpose()?
            .map(|t| t.to_string());
        n.span = Some(r.span);
        Ok(n)
    }

    /// The first symbol (in source order) of `syms` named by dictionary id
    /// `owner` that the class grain accepts and `symbol_kind` (if given)
    /// matches: where an owner hint (`SymbolDecl::owner`, issue #137)
    /// resolves within one file. `None` when the file has no such symbol.
    fn resolve_owner(
        &self,
        file: u64,
        syms: &[SymRec],
        owner: u64,
        symbol_kind: Option<&str>,
    ) -> Result<Option<usize>> {
        for (j, r) in syms.iter().enumerate() {
            if r.name != owner {
                continue;
            }
            let n = self.sym_node(file, j, syms)?;
            if grain_accepts(Grain::Class, &n) && symbol_kind.is_none_or(|k| kind_matches(&n, k)) {
                return Ok(Some(j));
            }
        }
        Ok(None)
    }

    /// Where an owner hint names no type in its own file (issue #149): the
    /// first type-like symbol named by dictionary id `owner` (then required
    /// to match `symbol_kind`, if given) in another file of the same repo, the same directory
    /// and the same language as `file` -- a package, for a language whose
    /// package is its directory. Candidates come from the symbol-name index;
    /// the first by (path, source order) wins, so the answer does not depend
    /// on which batch or order the files were indexed in. Returns that file's
    /// id and the owner's chain (the owner, then its enclosing symbols).
    fn resolve_owner_sibling(
        &self,
        file: &Node,
        owner: u64,
        symbol_kind: Option<&str>,
        cache: &mut HashMap<u64, FileCtx>,
    ) -> Result<Option<(u64, Vec<Node>)>> {
        let dir = |n: &str| {
            n.rsplit_once('/')
                .map_or(String::new(), |(d, _)| d.to_string())
        };
        let want_dir = dir(&file.name);
        let name = self.text(owner)?;
        // Same repo, directory and language, judged on the file row alone
        // (no org/repo rows, no stream); then (path, source order).
        let mut cands: Vec<(String, usize, u64)> = Vec::new();
        let mut rows: HashMap<u64, Option<String>> = HashMap::new();
        for v in self.sym_idx.get(&*name)? {
            let (tag, f2, idx) = split_id(v?.value());
            if tag != TAG_SYM || f2 == file.id {
                continue;
            }
            let path = match rows.get(&f2) {
                Some(p) => p.clone(),
                None => {
                    let n = self.need(f2)?;
                    let keep = n.parent == file.parent
                        && n.language == file.language
                        && dir(&n.name) == want_dir;
                    let p = keep.then_some(n.name);
                    rows.insert(f2, p.clone());
                    p
                }
            };
            if let Some(p) = path {
                cands.push((p, idx, f2));
            }
        }
        cands.sort();
        // The first type-like candidate wins; the kind filter then applies
        // to it (a filtered-out winner is no match, not a fall-through).
        let mut decoded: Option<(u64, Vec<SymRec>)> = None;
        let mut best: Option<(usize, u64)> = None;
        for (_, idx, f2) in cands {
            if decoded.as_ref().is_none_or(|(f, _)| *f != f2) {
                let raw = self
                    .streams
                    .get(f2)?
                    .ok_or_else(|| StoreError::Corrupt(format!("file {f2} without stream")))?;
                decoded = Some((f2, codec::decode_lazy(raw.value())?.symbols()?));
            }
            let syms = &decoded.as_ref().expect("just decoded").1;
            let Some(r) = syms.get(idx) else {
                return Err(StoreError::Corrupt(format!(
                    "symbol index {idx} past file {f2}"
                )));
            };
            if r.name != owner {
                continue;
            }
            let n = self.sym_node(f2, idx, syms)?;
            if grain_accepts(Grain::Class, &n) {
                if symbol_kind.is_some_and(|k| !kind_matches(&n, k)) {
                    return Ok(None);
                }
                best = Some((idx, f2));
                break;
            }
        }
        let Some((idx, f2)) = best else {
            return Ok(None);
        };
        self.ctx(f2, cache)?;
        let syms = decoded.expect("the winner is decoded").1;
        let mut chain = Vec::new();
        let mut cur = Some(idx as u32);
        while let Some(p) = cur {
            chain.push(self.sym_node(f2, p as usize, &syms)?);
            cur = syms[p as usize].parent;
        }
        Ok(Some((f2, chain)))
    }

    fn tok_node(&self, file: u64, i: usize, r: &TokRec) -> Result<Node> {
        let parent = r.parent.map_or(file, |p| sub_id(TAG_SYM, file, p as usize));
        let mut n = blank(
            sub_id(TAG_TOK, file, i),
            Some(parent),
            NodeKind::Token,
            self.text(r.term)?.to_string(),
        );
        n.token_class = Some(r.class);
        n.span = Some(r.span);
        Ok(n)
    }

    fn item_node(&self, file: u64, s: &Stream, it: Item) -> Result<Node> {
        match it {
            Item::Sym(i) => self.sym_node(file, i, &s.symbols),
            Item::Tok(i) => self.tok_node(file, i, &s.tokens[i]),
        }
    }

    /// A single node by id. For a symbol or token this decodes only what is
    /// needed to answer for `id` (the symbol section, or one token record
    /// reached through the nearest checkpoint), not the whole stream.
    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        let (tag, file, i) = split_id(id);
        if tag == 0 {
            return self.node(id);
        }
        let found = self.with_lazy(file, |lz| {
            if tag == TAG_SYM && i < lz.nsym() {
                let syms = lz.symbols()?;
                Ok(Some(self.sym_node(file, i, &syms)?))
            } else if tag == TAG_TOK && i < lz.ntok() {
                let mut rec: Option<TokRec> = None;
                lz.tokens_at(&[i], |_, t| rec = Some(t.clone()))?;
                match rec {
                    Some(t) => Ok(Some(self.tok_node(file, i, &t)?)),
                    None => Ok(None),
                }
            } else {
                Ok(None)
            }
        })?;
        // Outer None: no stream row for `file`. Inner None: `id` is out of
        // the symbol/token range. Both mean "not found".
        Ok(found.flatten())
    }

    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        match self.get(id)?.and_then(|n| n.parent) {
            Some(p) => self.get(p),
            None => Ok(None),
        }
    }

    /// Direct children in creation order: for org and repo the entity rows,
    /// for a file its top-level symbols and tokens outside symbols, for a
    /// symbol its child symbols and direct tokens. Unknown ids and tokens
    /// have none.
    ///
    /// Both the file (top-level) and symbol cases try a range-based path
    /// first when the file's stored ranges are exact (ADR 0003 story 3:
    /// slice 3i for a symbol via [`R::children_ranged`], slice 3j for a file
    /// via [`R::children_ranged_file`]); `None` (not dense, or no range
    /// data) falls back to the same eager decode used before those slices,
    /// verbatim.
    fn children(&self, id: NodeId) -> Result<Vec<Node>> {
        let (tag, file, i) = split_id(id);
        if tag == 0 {
            let Some(n) = self.node(id)? else {
                return Ok(Vec::new());
            };
            if n.kind == NodeKind::File {
                if let Some(out) = self.children_ranged_file(id)? {
                    return Ok(out);
                }
                let Some(s) = self.stream(id)? else {
                    return Ok(Vec::new());
                };
                let tree = stream_tree(&s);
                return tree
                    .top
                    .iter()
                    .map(|&it| self.item_node(id, &s, it))
                    .collect();
            }
            let mut out = Vec::new();
            for k in self.kids.get(id)? {
                out.push(self.need(k?.value())?);
            }
            return Ok(out);
        }
        if tag != TAG_SYM {
            return Ok(Vec::new());
        }
        if let Some(out) = self.children_ranged(file, i)? {
            return Ok(out);
        }
        self.children_fallback(file, i)
    }

    /// The pre-slice-3i path: decode the whole stream and walk the full
    /// `stream_tree`. What `children` falls back to when a range is not
    /// usable, and (via [`V2Store::children_via_fallback`]) what the slice-3i
    /// differential test compares the range-based result against -- sharing
    /// this function keeps the two from silently diverging.
    fn children_fallback(&self, file: u64, i: usize) -> Result<Vec<Node>> {
        let Some(s) = self.stream(file)? else {
            return Ok(Vec::new());
        };
        if i >= s.symbols.len() {
            return Ok(Vec::new());
        }
        let tree = stream_tree(&s);
        tree.kids[i]
            .iter()
            .map(|&it| self.item_node(file, &s, it))
            .collect()
    }

    /// Direct children of symbol `i` in file `file`, computed from the
    /// stored per-symbol transitive token range (ADR 0003 story 3, slice 3i)
    /// instead of decoding the whole stream. `Ok(None)` means "cannot answer
    /// this way" (no stream row, `i` out of range, or `ranges_dense()` is
    /// false) and the caller must fall back to the eager path; that fallback
    /// is exact even when a range is inexact, because the stored range is
    /// still the true min/max in that case (see [`Lazy::ranges_dense`]), just
    /// not usable to carve out only the direct tokens.
    ///
    /// Direct child symbols come from a single already-decoded symbol
    /// section (`Lazy::symbols`, no token decoding). Direct tokens are the
    /// ordinals in the symbol's own range minus the union of its direct
    /// children's ranges -- computed as the gaps between consecutive
    /// (start-sorted) child ranges, since dense ranges are nested and
    /// non-overlapping -- fetched with one `Lazy::tokens_at` call. The result
    /// is merged with the child symbols in the same `(start, symbols-before-
    /// tokens-on-a-tie)` order `stream_tree` uses, so it is byte-for-byte the
    /// same as the fallback.
    fn children_ranged(&self, file: u64, i: usize) -> Result<Option<Vec<Node>>> {
        let res = self.with_lazy(file, |lz| {
            if !lz.ranges_dense() {
                return Ok(None);
            }
            let syms = lz.symbols()?;
            if i >= syms.len() {
                return Ok(None);
            }
            let child_idxs: Vec<usize> = (0..syms.len())
                .filter(|&j| syms[j].parent == Some(i as u32))
                .collect();
            let ords = direct_token_ordinals(&syms, i, &child_idxs);
            let mut toks: Vec<(usize, TokRec)> = Vec::with_capacity(ords.len());
            if !ords.is_empty() {
                lz.tokens_at(&ords, |ord, t| toks.push((ord, t.clone())))?;
            }
            let mut out = Vec::with_capacity(child_idxs.len() + toks.len());
            let (mut si, mut ti) = (0, 0);
            while si < child_idxs.len() || ti < toks.len() {
                let take_sym = si < child_idxs.len()
                    && (ti >= toks.len()
                        || syms[child_idxs[si]].span.start <= toks[ti].1.span.start);
                if take_sym {
                    out.push(self.sym_node(file, child_idxs[si], &syms)?);
                    si += 1;
                } else {
                    out.push(self.tok_node(file, toks[ti].0, &toks[ti].1)?);
                    ti += 1;
                }
            }
            Ok(Some(out))
        })?;
        Ok(res.flatten())
    }

    /// Top-level (not through any symbol) direct children of file `file`,
    /// computed from the stored per-symbol transitive token ranges (ADR 0003
    /// story 3, slice 3j) instead of decoding the whole stream. `Ok(None)`
    /// means "cannot answer this way" (no stream row, or `ranges_dense()` is
    /// false) and the caller must fall back to the eager `stream()` +
    /// `stream_tree` path, exactly as [`R::children_ranged`] falls back for
    /// a symbol.
    ///
    /// Top-level symbols (no parent) come from the already-decoded symbol
    /// section, no token decoding. Top-level direct tokens are the ordinals
    /// in `0..ntok` minus the union of top-level symbols' ranges (the gaps
    /// between consecutive, start-sorted top-level ranges), fetched with one
    /// `Lazy::tokens_at` call -- the file-level analog of `children_ranged`'s
    /// own-range-minus-children computation, using [`top_level_ordinals`]
    /// instead of [`direct_token_ordinals`]. Merged with the top-level
    /// symbols in `stream_tree`'s order, so this is byte-for-byte the same
    /// as the fallback.
    ///
    /// **Measured** (ADR 0003 story 3, slice 3j spike, this repo's own
    /// `crates/` tree, real `RustExtractor`): this cuts decoded token
    /// records by 97.78% versus the eager path (133,772 to 2,970 records
    /// across 27 files), well past the scoping plan's 30% build gate --
    /// `tokens_at`'s checkpoint jump pays off here because the *complement*
    /// of well-covered symbol ranges tends to sit near a stream's few
    /// existing checkpoints, not because the gaps themselves are large.
    fn children_ranged_file(&self, file: u64) -> Result<Option<Vec<Node>>> {
        let res = self.with_lazy(file, |lz| {
            if !lz.ranges_dense() {
                return Ok(None);
            }
            let syms = lz.symbols()?;
            let top_idxs = top_level_idxs(&syms);
            let ords = top_level_ordinals(&syms, &top_idxs, lz.ntok());
            let mut toks: Vec<(usize, TokRec)> = Vec::with_capacity(ords.len());
            if !ords.is_empty() {
                lz.tokens_at(&ords, |ord, t| toks.push((ord, t.clone())))?;
            }
            let mut out = Vec::with_capacity(top_idxs.len() + toks.len());
            let (mut si, mut ti) = (0, 0);
            while si < top_idxs.len() || ti < toks.len() {
                let take_sym = si < top_idxs.len()
                    && (ti >= toks.len() || syms[top_idxs[si]].span.start <= toks[ti].1.span.start);
                if take_sym {
                    out.push(self.sym_node(file, top_idxs[si], &syms)?);
                    si += 1;
                } else {
                    out.push(self.tok_node(file, toks[ti].0, &toks[ti].1)?);
                    ti += 1;
                }
            }
            Ok(Some(out))
        })?;
        Ok(res.flatten())
    }

    /// Everything below `id`, depth first in source order, parents before
    /// their children. Decodes each stream once.
    fn descendants(&self, id: NodeId) -> Result<Vec<Node>> {
        let (tag, file, i) = split_id(id);
        let mut out = Vec::new();
        if tag == 0 {
            let Some(n) = self.node(id)? else {
                return Ok(out);
            };
            if n.kind == NodeKind::File {
                if let Some(v) = self.descendants_ranged_file(id)? {
                    return Ok(v);
                }
                self.file_walk(id, None, &mut out)?;
            } else {
                for k in self.kids.get(id)? {
                    let c = self.need(k?.value())?;
                    let (cid, kind) = (c.id, c.kind);
                    out.push(c);
                    if kind == NodeKind::File {
                        if let Some(v) = self.descendants_ranged_file(cid)? {
                            out.extend(v);
                        } else {
                            self.file_walk(cid, None, &mut out)?;
                        }
                    } else {
                        out.extend(self.descendants(cid)?);
                    }
                }
            }
        } else if tag == TAG_SYM {
            if let Some(v) = self.descendants_ranged(file, i)? {
                return Ok(v);
            }
            self.file_walk(file, Some(i), &mut out)?;
        }
        Ok(out)
    }

    /// Everything below symbol `i`, depth first, matching `file_walk`'s
    /// output exactly, computed from the stored per-symbol transitive token
    /// range (ADR 0003 story 3, slice 3i) instead of decoding the whole
    /// stream. `Ok(None)` means "cannot answer this way" -- same conditions
    /// as [`R::children_ranged`] -- and the caller falls back to `file_walk`.
    ///
    /// Unlike `children_ranged`, this needs every token transitively under
    /// `i`, which is exactly the ordinals in `syms[i].toks` (dense implies
    /// that range holds precisely those tokens), so it decodes that whole
    /// range in one `Lazy::tokens_at` call -- bounded by the symbol's own
    /// subtree size, not the file's token count -- rather than one call per
    /// level. The symbol section is decoded once (as every reader in this
    /// file already does) and used to build a symbol-to-direct-children map,
    /// then a depth-first walk mirrors `stream_tree`'s per-node ordering
    /// using that map plus the already-fetched token records.
    fn descendants_ranged(&self, file: u64, i: usize) -> Result<Option<Vec<Node>>> {
        let res = self.with_lazy(file, |lz| {
            if !lz.ranges_dense() {
                return Ok(None);
            }
            let syms = lz.symbols()?;
            if i >= syms.len() {
                return Ok(None);
            }
            let sym_kids = sym_kids_map(&syms);
            Ok(Some(
                self.descendants_ranged_one(lz, &syms, &sym_kids, file, i)?,
            ))
        })?;
        Ok(res.flatten())
    }

    /// The walk shared by [`R::descendants_ranged`] and
    /// [`R::descendants_ranged_file`]: everything below symbol `i`, given a
    /// symbol table and symbol-to-direct-children map the caller has already
    /// built (once) for this file. Pulled out so `descendants_ranged_file`
    /// can build that map a single time and reuse it across every top-level
    /// symbol's subtree walk, instead of each call rebuilding it from
    /// scratch -- see issue #40 (this used to be O(N) rebuilds of an
    /// O(nsym) map for a file with N top-level symbols).
    fn descendants_ranged_one(
        &self,
        lz: &Lazy,
        syms: &[SymRec],
        sym_kids: &[Vec<usize>],
        file: u64,
        i: usize,
    ) -> Result<Vec<Node>> {
        let mut tok_map: HashMap<usize, TokRec> = HashMap::new();
        if let Some((first, last)) = syms[i].toks {
            let ords: Vec<usize> = (first as usize..=last as usize).collect();
            lz.tokens_at(&ords, |ord, t| {
                tok_map.insert(ord, t.clone());
            })?;
        }
        // Merged (symbols + direct tokens) children of symbol `j`, in
        // `stream_tree`'s (start, symbols-before-tokens-on-a-tie) order.
        let item_children = |j: usize| -> Vec<Item> {
            let kids = &sym_kids[j];
            let ords = direct_token_ordinals(syms, j, kids);
            let mut merged = Vec::with_capacity(kids.len() + ords.len());
            let (mut si, mut ti) = (0, 0);
            while si < kids.len() || ti < ords.len() {
                let take_sym = si < kids.len()
                    && (ti >= ords.len()
                        || syms[kids[si]].span.start <= tok_map[&ords[ti]].span.start);
                if take_sym {
                    merged.push(Item::Sym(kids[si]));
                    si += 1;
                } else {
                    merged.push(Item::Tok(ords[ti]));
                    ti += 1;
                }
            }
            merged
        };
        let mut out = Vec::new();
        let mut stack: Vec<Item> = item_children(i).into_iter().rev().collect();
        while let Some(it) = stack.pop() {
            match it {
                Item::Sym(j) => {
                    out.push(self.sym_node(file, j, syms)?);
                    stack.extend(item_children(j).into_iter().rev());
                }
                Item::Tok(ord) => {
                    out.push(self.tok_node(file, ord, &tok_map[&ord])?);
                }
            }
        }
        Ok(out)
    }

    /// Everything below file `file`, depth first, matching `file_walk`'s
    /// output exactly, computed from the top-level ranged children (ADR 0003
    /// story 3, slice 3j) plus, for each top-level symbol, its own
    /// [`R::descendants_ranged`] subtree -- reusing the slice-3i per-symbol
    /// walk rather than duplicating it, so a file's `descendants` cannot
    /// silently diverge from a symbol's. `Ok(None)` means "cannot answer
    /// this way", same conditions as [`R::children_ranged_file`].
    fn descendants_ranged_file(&self, file: u64) -> Result<Option<Vec<Node>>> {
        let Some(top) = self.children_ranged_file(file)? else {
            return Ok(None);
        };
        // Single `with_lazy` call for the whole file: the symbol table and
        // its symbol-to-direct-children map are decoded/built once here and
        // shared across every top-level symbol's subtree walk below, rather
        // than each `descendants_ranged_one` call rebuilding them from
        // scratch -- see issue #40 (this used to be O(N) rebuilds of an
        // O(nsym) map for a file with N top-level symbols).
        let res = self.with_lazy(file, |lz| {
            if !lz.ranges_dense() {
                return Ok(None);
            }
            let syms = lz.symbols()?;
            let sym_kids = sym_kids_map(&syms);
            let mut out = Vec::with_capacity(top.len());
            for n in top {
                let is_symbol = n.kind == NodeKind::Symbol;
                let id = n.id;
                out.push(n);
                if is_symbol {
                    let (_, _, i) = split_id(id);
                    if i >= syms.len() {
                        // `children_ranged_file` already required
                        // `ranges_dense()` true for this file, so every
                        // symbol in it has an exact range too and this
                        // branch is unreachable; fall back to the
                        // whole-file eager walk defensively rather than
                        // panic.
                        return Ok(None);
                    }
                    let sub = self.descendants_ranged_one(lz, &syms, &sym_kids, file, i)?;
                    out.extend(sub);
                }
            }
            Ok(Some(out))
        })?;
        Ok(res.flatten())
    }

    /// Depth-first walk of one stream from the file (`None`) or from a symbol.
    fn file_walk(&self, file: u64, from: Option<usize>, out: &mut Vec<Node>) -> Result<()> {
        let Some(s) = self.stream(file)? else {
            return Ok(());
        };
        if from.is_some_and(|i| i >= s.symbols.len()) {
            return Ok(());
        }
        let tree = stream_tree(&s);
        let start = match from {
            Some(i) => &tree.kids[i],
            None => &tree.top,
        };
        let mut stack: Vec<Item> = start.iter().rev().copied().collect();
        while let Some(it) = stack.pop() {
            out.push(self.item_node(file, &s, it)?);
            if let Item::Sym(i) = it {
                stack.extend(tree.kids[i].iter().rev().copied());
            }
        }
        Ok(())
    }

    /// Parent, grandparent, ... up to the org (nearest first). For a symbol
    /// or token this decodes only the symbol section (plus, for a token, one
    /// token record through the nearest checkpoint), never the token section
    /// in full.
    fn ancestors(&self, id: NodeId) -> Result<Vec<Node>> {
        let (tag, file, i) = split_id(id);
        let mut out = Vec::new();
        let mut up = if tag == 0 {
            self.node(id)?.and_then(|n| n.parent)
        } else {
            let done = self.with_lazy(file, |lz| {
                let syms = lz.symbols()?;
                let mut cur = match tag {
                    TAG_SYM if i < syms.len() => syms[i].parent,
                    TAG_TOK if i < lz.ntok() => {
                        let mut parent = None;
                        lz.tokens_at(&[i], |_, t| parent = t.parent)?;
                        parent
                    }
                    _ => return Ok(None),
                };
                while let Some(p) = cur {
                    out.push(self.sym_node(file, p as usize, &syms)?);
                    cur = syms[p as usize].parent;
                }
                Ok(Some(file))
            })?;
            // Outer None: no stream row for `file`. Inner None: `id` is out
            // of the symbol/token range. Both mean "no more ancestors".
            match done.flatten() {
                Some(f) => Some(f),
                None => return Ok(out),
            }
        };
        while let Some(p) = up {
            let n = self.need(p)?;
            up = n.parent;
            out.push(n);
        }
        Ok(out)
    }

    fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        let mut n = 0;
        match kind {
            NodeKind::Symbol | NodeKind::Token => {
                let prefix = if kind == NodeKind::Symbol {
                    "s\0"
                } else {
                    "t\0"
                };
                for r in self.cat.iter()? {
                    let (k, v) = r?;
                    if k.value().starts_with(prefix) {
                        n += v.value() as usize;
                    }
                }
            }
            _ => {
                for r in self.nodes.iter()? {
                    if dec(r?.1.value())?.kind == kind {
                        n += 1;
                    }
                }
            }
        }
        Ok(n)
    }

    /// Every top-level (parent-less) node: one per org. Used by `export`
    /// (ADR 0003 story 12) to enumerate the whole graph through the
    /// `Store`/`StoreRead` trait alone, without backend-specific access; org
    /// and repo entity rows live in the `nodes` table (unlike symbols/tokens,
    /// which are stream-encoded), so this is the same one-table scan
    /// `count_nodes`/`describe_by_scan` already do, filtered to `Org` with no
    /// parent.
    fn roots(&self) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        for r in self.nodes.iter()? {
            let n = dec(r?.1.value())?;
            if n.kind == NodeKind::Org && n.parent.is_none() {
                out.push(n);
            }
        }
        out.sort_by_key(|n| n.id);
        Ok(out)
    }

    fn ctx(&self, file_id: u64, cache: &mut HashMap<u64, FileCtx>) -> Result<()> {
        if cache.contains_key(&file_id) {
            return Ok(());
        }
        let file = self.need(file_id)?;
        let repo = self.entity(
            file.parent
                .ok_or_else(|| StoreError::Corrupt("file without repo".into()))?,
        )?;
        let org = self.entity(
            repo.parent
                .ok_or_else(|| StoreError::Corrupt("repo without org".into()))?,
        )?;
        cache.insert(file_id, FileCtx { org, repo, file });
        Ok(())
    }

    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        let Some(o) = self.find(None, NodeKind::Org, org)? else {
            return Ok(None);
        };
        let Some(r) = self.find(Some(o), NodeKind::Repo, repo)? else {
            return Ok(None);
        };
        let Some(f) = self.find(Some(r), NodeKind::File, &normalize_path(path))? else {
            return Ok(None);
        };
        let s = self
            .stream(f)?
            .ok_or_else(|| StoreError::Corrupt(format!("file {f} without stream")))?;
        let mut out = Vec::with_capacity(s.tokens.len());
        for (i, t) in s.tokens.iter().enumerate() {
            out.push(self.tok_node(f, i, t)?);
        }
        Ok(Some(out))
    }

    /// `describe` by decoding every file (the reference for the catalog).
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        let mut all: Vec<Node> = Vec::new();
        for r in self.nodes.iter()? {
            all.push(dec(r?.1.value())?);
        }
        all.sort_by_key(|n| n.id);
        let mut org_name: HashMap<NodeId, String> = HashMap::new();
        let mut repo_key: HashMap<NodeId, (String, String)> = HashMap::new();
        let mut infos: BTreeMap<(String, String), RepoInfo> = BTreeMap::new();
        for n in &all {
            match n.kind {
                NodeKind::Org => {
                    org_name.insert(n.id, n.name.clone());
                }
                NodeKind::Repo => {
                    let o = n
                        .parent
                        .and_then(|p| org_name.get(&p))
                        .cloned()
                        .unwrap_or_default();
                    let key = (o.clone(), n.name.clone());
                    infos.entry(key.clone()).or_insert_with(|| RepoInfo {
                        org: o,
                        repo: n.name.clone(),
                        files: 0,
                        languages: BTreeMap::new(),
                        token_classes: BTreeMap::new(),
                        // Default; set to the marker's value (if any) below,
                        // after every repo has been discovered by the scan.
                        open_batch: false,
                        encodings: BTreeMap::new(),
                        lossy: 0,
                    });
                    repo_key.insert(n.id, key);
                }
                NodeKind::File => {
                    let Some(key) = n.parent.and_then(|p| repo_key.get(&p)).cloned() else {
                        continue;
                    };
                    let lang = n.language.clone().unwrap_or_else(|| "unknown".into());
                    let info = infos.get_mut(&key).expect("repo registered");
                    info.files += 1;
                    if let Some(e) = &n.encoding {
                        *info.encodings.entry(e.clone()).or_default() += 1;
                    }
                    info.lossy += usize::from(n.lossy);
                    let l = info.languages.entry(lang).or_default();
                    l.files += 1;
                    let s = self
                        .stream(n.id)?
                        .ok_or_else(|| StoreError::Corrupt(format!("file {} no stream", n.id)))?;
                    for i in 0..s.symbols.len() {
                        l.symbols += 1;
                        *l.symbol_kinds
                            .entry(kind_label(&self.sym_node(n.id, i, &s.symbols)?))
                            .or_default() += 1;
                    }
                    l.tokens += s.tokens.len();
                    for t in &s.tokens {
                        *info
                            .token_classes
                            .entry(t.class.as_str().to_string())
                            .or_default() += 1;
                    }
                }
                _ => {}
            }
        }
        let marker = self.open_batch_marker()?;
        if let Some(key) = &marker {
            if let Some(info) = infos.get_mut(key) {
                info.open_batch = true;
            }
        }
        Ok(infos
            .into_values()
            .filter(|i| org.is_none_or(|o| i.org == o) && repo.is_none_or(|r| i.repo == r))
            .collect())
    }

    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        for (what, v) in [
            ("org", &q.org),
            ("repo", &q.repo),
            ("language", &q.language),
            ("file", &q.file),
            ("kind", &q.kind),
        ] {
            if v.as_deref() == Some("") {
                return Err(StoreError::Rejected(format!("empty {what} filter")));
            }
        }
        if q.pattern.is_empty() {
            return Err(StoreError::Rejected(
                "empty symbol pattern (use `*` to list all symbols)".into(),
            ));
        }
        if q.pattern.ends_with("**") {
            return Err(StoreError::Rejected(
                "pattern ending in `**` is ambiguous: use `prefix*` for a prefix or `name\\*` for a literal `*`".into(),
            ));
        }
        let mut ids: Vec<u64> = Vec::new();
        if let Some(lit) = q.pattern.strip_suffix("\\*") {
            let name = format!("{lit}*");
            for v in self.sym_idx.get(name.as_str())? {
                ids.push(v?.value());
            }
        } else {
            match q.pattern.strip_suffix('*') {
                Some(prefix) => {
                    for r in self.sym_idx.range(prefix..)? {
                        let (k, vals) = r?;
                        if !k.value().starts_with(prefix) {
                            break;
                        }
                        for v in vals {
                            ids.push(v?.value());
                        }
                    }
                }
                None => {
                    for v in self.sym_idx.get(q.pattern.as_str())? {
                        ids.push(v?.value());
                    }
                }
            }
        }
        let want_file = q.file.as_deref().map(normalize_path);
        let want_lang = q.language.as_deref().map(str::to_ascii_lowercase);
        // Group the index hits by file and apply the file-level filters
        // before any stream is read. A dangling entry (stale index) is
        // skipped, not an error.
        let mut by_file: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for id in ids {
            let (tag, file, i) = split_id(id);
            if tag == TAG_SYM {
                by_file.entry(file).or_default().push(i);
            }
        }
        let mut files: HashMap<u64, FileCtx> = HashMap::new();
        let mut order: Vec<u64> = Vec::new();
        for &fid in by_file.keys() {
            if self.node(fid)?.is_none_or(|n| n.kind != NodeKind::File) {
                continue;
            }
            self.ctx(fid, &mut files)?;
            let c = &files[&fid];
            if want_lang
                .as_ref()
                .is_some_and(|l| c.file.language.as_ref() != Some(l))
                || q.org.as_ref().is_some_and(|o| &c.org.name != o)
                || q.repo.as_ref().is_some_and(|r| &c.repo.name != r)
                || want_file.as_ref().is_some_and(|f| &c.file.name != f)
            {
                continue;
            }
            order.push(fid);
        }
        // Files in sorted-by-path order, so the limit can stop the walk.
        order.sort_by(|a, b| {
            let (a, b) = (&files[a], &files[b]);
            (&a.org.name, &a.repo.name, &a.file.name).cmp(&(
                &b.org.name,
                &b.repo.name,
                &b.file.name,
            ))
        });
        // `want` is offset+limit: the early-termination threshold below must
        // account for rows that will be skipped as `offset` at the end, or a
        // page beyond the first would come back short/empty.
        let want = q
            .offset
            .unwrap_or(0)
            .saturating_add(q.limit.unwrap_or(usize::MAX));
        let mut out: Vec<SymbolHit> = Vec::new();
        for fid in order {
            if out.len() >= want {
                break;
            }
            let Some(raw) = self.streams.get(fid)? else {
                continue;
            };
            // Only the symbol section is decoded, not the tokens.
            let syms = codec::decode_lazy(raw.value())?.symbols()?;
            let c = &files[&fid];
            let mut rows: Vec<((u32, String, u64), SymbolHit)> = Vec::new();
            for &i in &by_file[&fid] {
                if i >= syms.len() {
                    continue;
                }
                let sym = self.sym_node(fid, i, &syms)?;
                if q.kind.as_deref().is_some_and(|k| !kind_matches(&sym, k)) {
                    continue;
                }
                let mut quals = vec![sym.name.clone()];
                let mut cur = syms[i].parent;
                while let Some(p) = cur {
                    // In range: `codec::decode_lazy` checks a parent is an earlier symbol.
                    let r = &syms[p as usize];
                    quals.push(self.text(r.name)?.to_string());
                    cur = r.parent;
                }
                quals.reverse();
                let qualified = quals.join("::");
                rows.push((
                    (
                        syms[i].span.start,
                        qualified.clone(),
                        sub_id(TAG_SYM, fid, i),
                    ),
                    SymbolHit {
                        org: c.org.name.clone(),
                        repo: c.repo.name.clone(),
                        file: c.file.name.clone(),
                        language: c.file.language.clone(),
                        name: sym.name,
                        qualified,
                        kind: sym.symbol_kind.unwrap_or(SymbolKind::Other),
                        lang_kind: sym.lang_kind,
                        span: sym.span,
                        owner: syms[i]
                            .owner
                            .map(|o| self.text(o))
                            .transpose()?
                            .map(|t| t.to_string()),
                        encoding: c.file.encoding.clone(),
                        lossy: c.file.lossy,
                    },
                ));
            }
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            out.extend(rows.into_iter().map(|(_, h)| h));
        }
        out.truncate(want);
        if let Some(off) = q.offset {
            out.drain(0..off.min(out.len()));
        }
        if let Some(n) = q.limit {
            out.truncate(n);
        }
        Ok(out)
    }

    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        let Some(term) = self.lookup(q.text.as_str())? else {
            return Ok(Vec::new());
        };
        // Candidate files come from the postings; token, symbol and class
        // reads decode the stream, file/repo/org roll-ups without a class
        // filter use the posting counts alone.
        let mut cands: Vec<(u64, Vec<u8>)> = Vec::new();
        for r in self.post.range((term, 0)..=(term, u64::MAX))? {
            let (k, v) = r?;
            cands.push((k.value().1, v.value().to_vec()));
        }
        let counts_only =
            q.class.is_none() && matches!(q.grain, Grain::File | Grain::Repo | Grain::Org);
        let want_lang = q.language.as_deref().map(str::to_ascii_lowercase);
        let mut files: HashMap<u64, FileCtx> = HashMap::new();
        let mut order: Vec<(u64, Vec<u8>)> = Vec::new();
        for (fid, post) in cands {
            self.ctx(fid, &mut files)?;
            let c = &files[&fid];
            if want_lang
                .as_ref()
                .is_some_and(|l| c.file.language.as_ref() != Some(l))
                || q.org.as_deref().is_some_and(|o| c.org.name != o)
                || q.repo.as_deref().is_some_and(|r| c.repo.name != r)
            {
                continue;
            }
            order.push((fid, post));
        }
        // Sorted-by-path order: every later file sorts after every row of the
        // earlier ones, so once the limit is met at a group boundary the walk
        // can stop without reading the remaining streams.
        order.sort_by(|(a, _), (b, _)| {
            let (a, b) = (&files[a], &files[b]);
            (&a.org.name, &a.repo.name, &a.file.name).cmp(&(
                &b.org.name,
                &b.repo.name,
                &b.file.name,
            ))
        });
        // See `search_symbols`'s `want` comment: the stop threshold must
        // include rows that `offset` will later skip.
        let want = q
            .offset
            .unwrap_or(0)
            .saturating_add(q.limit.unwrap_or(usize::MAX));
        type Key = (String, String, String, u32, u64);
        let mut rows: BTreeMap<Key, Hit> = BTreeMap::new();
        let mut last_group: Option<(u64, u64, u64)> = None;
        // Files an owner hint resolved into (issue #149), apart from
        // `files` (borrowed for the whole walk), and the resolutions by
        // (owner id, repo, directory, language).
        let mut sib_files: HashMap<u64, FileCtx> = HashMap::new();
        type SibKey = (u64, Option<u64>, String, Option<String>);
        let mut sib_owners: HashMap<SibKey, Option<(u64, Vec<Node>)>> = HashMap::new();
        for (fid, post) in order {
            let c = &files[&fid];
            let group = match q.grain {
                Grain::Org => (c.org.id, 0, 0),
                Grain::Repo => (c.org.id, c.repo.id, 0),
                _ => (c.org.id, c.repo.id, fid),
            };
            // Past the limit, a later file can still matter to the class
            // grain: an owner hint resolved in a sibling file (issue #149)
            // keys its row under that earlier file. A file at or before
            // the cut-off row's path is read whole; a later one only if its
            // directory can hold a path before the cut-off (same org and
            // repo, `dir/` sorting before it) and it has owner hints; once
            // past the cut-off's repo, stop.
            let mut past_cut = false;
            if rows.len() >= want && last_group != Some(group) {
                if q.grain != Grain::Class || want == 0 {
                    break;
                }
                let (co, cr, cp, _, _) = rows.keys().nth(want - 1).expect("want rows");
                if (&c.org.name, &c.repo.name) != (co, cr) {
                    break;
                }
                let prefix = c
                    .file
                    .name
                    .rsplit_once('/')
                    .map_or(String::new(), |(d, _)| format!("{d}/"));
                // A sibling resolution may have keyed a row under this
                // file itself (or a later one): read it whole.
                if c.file.name.as_str() > cp.as_str() {
                    if prefix.as_str() >= cp.as_str() {
                        continue;
                    }
                    past_cut = true;
                }
            }
            last_group = Some(group);
            let base_hit = |count: usize| Hit {
                grain: q.grain,
                org: c.org.name.clone(),
                repo: Some(c.repo.name.clone()),
                file: Some(c.file.name.clone()),
                language: c.file.language.clone(),
                symbol: None,
                symbol_kind: None,
                lang_kind: None,
                token_class: None,
                span: None,
                count,
                no_symbols: false,
                no_matching_symbol: false,
                encoding: c.file.encoding.clone(),
                lossy: c.file.lossy,
            };
            let base = (c.org.name.clone(), c.repo.name.clone(), c.file.name.clone());
            let file_key = |g: Grain| -> Key {
                match g {
                    Grain::Repo => (base.0.clone(), base.1.clone(), String::new(), 0, 0),
                    Grain::Org => (base.0.clone(), String::new(), String::new(), 0, 0),
                    _ => (base.0.clone(), base.1.clone(), base.2.clone(), 0, 0),
                }
            };
            let roll = |hit: &mut Hit| match q.grain {
                Grain::Repo => {
                    hit.file = None;
                    hit.language = None;
                    hit.encoding = None;
                    hit.lossy = false;
                }
                Grain::Org => {
                    hit.repo = None;
                    hit.file = None;
                    hit.language = None;
                    hit.encoding = None;
                    hit.lossy = false;
                }
                _ => {}
            };
            if counts_only {
                let n_post = codec::posting_count(&post)?;
                let mut hit = base_hit(n_post);
                roll(&mut hit);
                rows.entry(file_key(q.grain))
                    .and_modify(|h| h.count += n_post)
                    .or_insert(hit);
                continue;
            }
            let raw = self
                .streams
                .get(fid)?
                .ok_or_else(|| StoreError::Corrupt(format!("file {fid} without stream")))?;
            let lazy = codec::decode_lazy(raw.value())?;
            if past_cut && !lazy.symbols()?.iter().any(|s| s.owner.is_some()) {
                continue;
            }
            // Read only the postings' ordinals through the checkpoints.
            let mut matches: Vec<(usize, TokRec)> = Vec::new();
            lazy.tokens_at(&codec::posting_ordinals(&post)?, |ord, t| {
                if t.term == term && q.class.is_none_or(|c| c == t.class) {
                    matches.push((ord, t.clone()));
                }
            })?;
            if matches.is_empty() {
                continue;
            }
            // Only the token and symbolic grains read the enclosing symbols.
            let need_chain = matches!(q.grain, Grain::Token) || q.grain.is_symbolic();
            let syms = if need_chain {
                lazy.symbols()?
            } else {
                Vec::new()
            };
            let syms = &syms;
            // Owner id -> the type-like symbol it resolves to in this file
            // (issue #137), cached across this file's matches.
            let mut owners: HashMap<u64, Option<usize>> = HashMap::new();
            // The same, unfiltered: consulted only under a kind filter, to
            // tell a rejected same-file type from no such type (issue #201).
            let mut owners_any: HashMap<u64, bool> = HashMap::new();
            for (ord, t) in matches {
                // Enclosing symbols, innermost first (with their indexes).
                let mut chain: Vec<Node> = Vec::new();
                let mut chain_idx: Vec<usize> = Vec::new();
                if need_chain {
                    let mut cur = t.parent;
                    while let Some(p) = cur {
                        chain.push(self.sym_node(fid, p as usize, syms)?);
                        chain_idx.push(p as usize);
                        // In range: `codec::decode_lazy` checks a parent is an earlier symbol.
                        cur = syms[p as usize].parent;
                    }
                }
                let qual = |syms: &[Node]| {
                    (!syms.is_empty()).then(|| {
                        syms.iter()
                            .rev()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join("::")
                    })
                };
                let mut hit = base_hit(1);
                let key: Key;
                match q.grain {
                    Grain::Token => {
                        hit.symbol = qual(&chain);
                        hit.symbol_kind = chain.first().and_then(|s| s.symbol_kind);
                        hit.lang_kind = chain.first().and_then(|s| s.lang_kind.clone());
                        hit.token_class = Some(t.class);
                        hit.span = Some(t.span);
                        key = (
                            base.0.clone(),
                            base.1.clone(),
                            base.2.clone(),
                            t.span.start,
                            sub_id(TAG_TOK, fid, ord),
                        );
                    }
                    Grain::Symbol | Grain::Method | Grain::Class => {
                        // Innermost enclosing symbol the grain accepts (any
                        // for symbol, a callable for method, a type or impl
                        // for class), narrowed by `symbol_kind` when given.
                        let pick = chain.iter().position(|s| {
                            grain_accepts(q.grain, s)
                                && q.symbol_kind.as_deref().is_none_or(|k| kind_matches(s, k))
                        });
                        // Class grain, nothing type-like encloses the token by
                        // span (whatever the kind filter): follow the innermost
                        // resolvable owner hint (issue #137) to a type-like
                        // symbol in this file.
                        let mut owned: Vec<Node> = Vec::new();
                        // The file `owned` lives in when it is not this one.
                        let mut owned_in: Option<u64> = None;
                        if pick.is_none()
                            && q.grain == Grain::Class
                            && !chain.iter().any(|s| grain_accepts(Grain::Class, s))
                        {
                            for &i in &chain_idx {
                                let Some(o) = syms[i].owner else { continue };
                                let j = match owners.get(&o) {
                                    Some(&j) => j,
                                    None => {
                                        let j = self.resolve_owner(
                                            fid,
                                            syms,
                                            o,
                                            q.symbol_kind.as_deref(),
                                        )?;
                                        owners.insert(o, j);
                                        j
                                    }
                                };
                                if let Some(j) = j {
                                    let mut cur = Some(j as u32);
                                    while let Some(p) = cur {
                                        owned.push(self.sym_node(fid, p as usize, syms)?);
                                        cur = syms[p as usize].parent;
                                    }
                                    break;
                                }
                                // A type of that name in this file that the
                                // kind filter rejects: no match, and no
                                // sibling search.
                                if q.symbol_kind.is_some() {
                                    let any = match owners_any.get(&o) {
                                        Some(&any) => any,
                                        None => {
                                            let any =
                                                self.resolve_owner(fid, syms, o, None)?.is_some();
                                            owners_any.insert(o, any);
                                            any
                                        }
                                    };
                                    if any {
                                        continue;
                                    }
                                }
                                // No such type in this file: a sibling file of
                                // the same directory (issue #149).
                                let dir = c
                                    .file
                                    .name
                                    .rsplit_once('/')
                                    .map_or(String::new(), |(d, _)| d.to_string());
                                let sk = (o, c.file.parent, dir, c.file.language.clone());
                                let found = match sib_owners.get(&sk) {
                                    Some(f) => f.clone(),
                                    None => {
                                        let f = self.resolve_owner_sibling(
                                            &c.file,
                                            o,
                                            q.symbol_kind.as_deref(),
                                            &mut sib_files,
                                        )?;
                                        sib_owners.insert(sk, f.clone());
                                        f
                                    }
                                };
                                if let Some((f2, ch)) = found {
                                    owned = ch;
                                    owned_in = Some(f2);
                                    break;
                                }
                            }
                        }
                        if let Some(f2) = owned_in {
                            let o = &sib_files[&f2];
                            hit.file = Some(o.file.name.clone());
                            hit.language = o.file.language.clone();
                            hit.encoding = o.file.encoding.clone();
                            hit.lossy = o.file.lossy;
                        }
                        let row_file = owned_in
                            .map_or_else(|| base.2.clone(), |f2| sib_files[&f2].file.name.clone());
                        let (pick, chain) = match pick {
                            Some(i) => (Some(i), &chain),
                            None if !owned.is_empty() => (Some(0), &owned),
                            None => (None, &chain),
                        };
                        match pick {
                            Some(i) => {
                                let s = &chain[i];
                                hit.symbol = qual(&chain[i..]);
                                hit.symbol_kind = s.symbol_kind;
                                hit.lang_kind = s.lang_kind.clone();
                                hit.span = s.span;
                                key = (
                                    base.0.clone(),
                                    base.1.clone(),
                                    row_file,
                                    s.span.map_or(0, |x| x.start),
                                    s.id,
                                );
                            }
                            None => {
                                if syms.is_empty() {
                                    hit.no_symbols = true;
                                } else {
                                    hit.no_matching_symbol = true;
                                }
                                key = file_key(Grain::File);
                            }
                        }
                    }
                    g => {
                        roll(&mut hit);
                        key = file_key(g);
                    }
                }
                rows.entry(key).and_modify(|h| h.count += 1).or_insert(hit);
            }
        }
        Ok(rows
            .into_values()
            .skip(q.offset.unwrap_or(0))
            .take(q.limit.unwrap_or(usize::MAX))
            .collect())
    }
}

/// Write side: every table one ingest touches.
struct W<'t> {
    meta: redb::Table<'t, &'static str, u64>,
    nodes: redb::Table<'t, u64, &'static [u8]>,
    names: redb::Table<'t, &'static str, u64>,
    children: redb::MultimapTable<'t, u64, u64>,
    dict: redb::Table<'t, &'static str, u64>,
    rev: redb::Table<'t, u64, &'static [u8]>,
    streams: redb::Table<'t, u64, &'static [u8]>,
    post: redb::Table<'t, (u64, u64), &'static [u8]>,
    sym_idx: redb::MultimapTable<'t, &'static str, u64>,
    cat: redb::Table<'t, &'static str, u64>,
    refs: redb::Table<'t, u64, u64>,
    content_files: redb::MultimapTable<'t, u64, u64>,
}

impl<'t> W<'t> {
    fn new(wt: &'t redb::WriteTransaction) -> Result<Self> {
        Ok(Self {
            meta: wt.open_table(META)?,
            nodes: wt.open_table(NODES)?,
            names: wt.open_table(NAMES)?,
            children: wt.open_multimap_table(CHILDREN)?,
            dict: wt.open_table(DICT)?,
            rev: wt.open_table(DICT_REV)?,
            streams: wt.open_table(STREAMS)?,
            post: wt.open_table(POST)?,
            sym_idx: wt.open_multimap_table(SYMBOLS)?,
            cat: wt.open_table(CATALOG)?,
            refs: wt.open_table(REFS)?,
            content_files: wt.open_multimap_table(CONTENT_FILES)?,
        })
    }

    fn text(&self, term: u64) -> Result<String> {
        dict_rev_lookup(&self.rev, term)?
            .ok_or_else(|| StoreError::Corrupt(format!("dangling term {term}")))
    }

    /// The dictionary key holding `text` (the entry for `id` when given, else
    /// any entry with that exact text) and its id, or the first free key it
    /// would take and `None`.
    fn dict_key(&self, text: &str, id: Option<u64>) -> Result<(String, Option<u64>)> {
        if !is_hashed(text) {
            let found = self.dict.get(text)?.map(|v| v.value());
            return Ok((text.to_string(), found));
        }
        for n in 0.. {
            let key = hashed_key(text, n);
            let Some(found) = self.dict.get(key.as_str())?.map(|v| v.value()) else {
                return Ok((key, None));
            };
            let same = match id {
                Some(i) => i == found,
                None => dict_rev_lookup(&self.rev, found)?.is_some_and(|t| t == text),
            };
            if same {
                return Ok((key, Some(found)));
            }
        }
        unreachable!()
    }

    fn intern(&mut self, text: &str, next_term: &mut u64) -> Result<u64> {
        let (key, found) = self.dict_key(text, None)?;
        if let Some(v) = found {
            return Ok(v);
        }
        let id = *next_term;
        *next_term += 1;
        self.dict.insert(key.as_str(), id)?;
        dict_rev_append(&mut self.rev, id, text)?;
        Ok(id)
    }

    /// Add (`d` = 1) or remove (`d` = -1) a file's symbols and tokens from the
    /// catalog tally.
    fn tally_stream(
        &self,
        tally: &mut Tally,
        scope: &Scope,
        file: u64,
        s: &Stream,
        d: i64,
    ) -> Result<()> {
        for r in &s.symbols {
            let mut n = blank(0, Some(file), NodeKind::Symbol, String::new());
            n.symbol_kind = Some(r.kind);
            n.lang_kind = r.lang_kind.map(|k| self.text(k)).transpose()?;
            tally.node(scope, &n, d);
        }
        for t in &s.tokens {
            let mut n = blank(0, Some(file), NodeKind::Token, String::new());
            n.token_class = Some(t.class);
            tally.node(scope, &n, d);
        }
        Ok(())
    }

    /// Decrement `file`'s content refcount and drop its `content_files`
    /// entry; delete its stream, postings and symbol index entries (not its
    /// entity row, name or catalog file count) only once the refcount
    /// reaches zero. Today `content_id(file) == file`, so the refcount is
    /// always exactly 1 before this call and the delete always happens
    /// immediately -- this is the seam for story 18's future
    /// content-sharing fan-out, not a functional change yet.
    fn remove_content(&mut self, file: u64, scope: &Scope, tally: &mut Tally) -> Result<()> {
        let Some(raw) = self.streams.get(file)? else {
            return Ok(());
        };
        let s = read_stats::uncounted(|| codec::decode(raw.value()))?;
        drop(raw);

        let cid = content_id(file);
        self.content_files.remove(cid, file)?;
        let count = self.refs.get(cid)?.map(|v| v.value()).unwrap_or(0);
        // A stream row implies a live refs entry (both are written together
        // in `ingest_validated`); an underflow here would mean the two
        // tables have already drifted apart, which is a bug, not a runtime
        // condition to handle -- `saturating_sub` still keeps release builds
        // safe if it ever does.
        debug_assert!(
            count > 0,
            "refs underflow: content id {cid} (file {file}) had refcount 0 before decrement"
        );
        let remaining = count.saturating_sub(1);
        if remaining > 0 {
            self.refs.insert(cid, remaining)?;
            return Ok(());
        }
        self.refs.remove(cid)?;

        self.streams.remove(file)?;
        self.tally_stream(tally, scope, file, &s, -1)?;
        let mut terms: BTreeSet<u64> = BTreeSet::new(); // ordered: reproducible file
        for t in &s.tokens {
            terms.insert(t.term);
        }
        for t in terms {
            self.post.remove((t, file))?;
        }
        for (i, r) in s.symbols.iter().enumerate() {
            let name = self.text(r.name)?;
            self.sym_idx
                .remove(name.as_str(), sub_id(TAG_SYM, file, i))?;
        }
        Ok(())
    }
}

/// Copy every row of `def` from `rt` into the same-named table of `wt`
/// (creating it), for [`V2Store::export_snapshot`].
fn copy_table<K: redb::Key + 'static, V: redb::Value + 'static>(
    rt: &ReadTransaction,
    wt: &redb::WriteTransaction,
    def: TableDefinition<K, V>,
) -> Result<()> {
    let src = rt.open_table(def)?;
    let mut dst = wt.open_table(def)?;
    for row in src.iter()? {
        let (k, v) = row?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

/// [`copy_table`] for a multimap table.
fn copy_multimap<K: redb::Key + 'static, V: redb::Key + 'static>(
    rt: &ReadTransaction,
    wt: &redb::WriteTransaction,
    def: MultimapTableDefinition<K, V>,
) -> Result<()> {
    let src = rt.open_multimap_table(def)?;
    let mut dst = wt.open_multimap_table(def)?;
    for row in src.iter()? {
        let (k, vals) = row?;
        for v in vals {
            dst.insert(k.value(), v?.value())?;
        }
    }
    Ok(())
}

/// Bring the catalog up to [`CATALOG_VERSION`] 2 (ADR 0007 C6, epic story
/// 43) inside `wt`: drop any `e`/`l` entries already there (so nothing is
/// counted twice), recount them from the File nodes and stamp the version.
/// It reads every `nodes` row (entity rows only: orgs, repos, files; never a
/// stream), so it is O(entity rows) in time and holds all of them in memory
/// at once (a `HashMap` of decoded nodes) while it runs. Only a schema-11
/// file needs it (its catalog predates the counts).
fn rebuild_encoding_catalog_in(wt: &redb::WriteTransaction) -> Result<()> {
    let nodes = wt.open_table(NODES)?;
    let mut cat = wt.open_table(CATALOG)?;
    let mut all: HashMap<NodeId, Node> = HashMap::new();
    for r in nodes.iter()? {
        let n = dec(r?.1.value())?;
        all.insert(n.id, n);
    }
    let mut stale: Vec<String> = Vec::new();
    for r in cat.iter()? {
        let k = r?.0.value().to_string();
        if k.starts_with("e\0") || k.starts_with("l\0") {
            stale.push(k);
        }
    }
    for k in stale {
        cat.remove(k.as_str())?;
    }
    let mut tally = Tally::default();
    for f in all.values().filter(|n| n.kind == NodeKind::File) {
        let repo = f.parent.and_then(|p| all.get(&p));
        let org = repo.and_then(|r| r.parent).and_then(|p| all.get(&p));
        let (Some(repo), Some(org)) = (repo, org) else {
            continue;
        };
        let s = Scope {
            org: &org.name,
            repo: &repo.name,
            lang: "",
            encoding: f.encoding.as_deref(),
            lossy: f.lossy,
        };
        tally.encoding(&s, 1);
    }
    tally.apply(&mut cat)?;
    wt.open_table(META)?
        .insert("catalog_version", CATALOG_VERSION)?;
    Ok(())
}

/// Recompute `refs`/`content_files` from scratch by scanning every live
/// file's stream row (the source of truth: a stream row exists iff its file
/// is live, see `W::remove_content`) and rewriting both tables to match,
/// stamping `derived_version_refs_content_files` to [`REFS_DERIVED_VERSION`]
/// in the same write transaction. Shared by [`V2Store::rebuild_refs`] (manual
/// call) and the self-heal check in `open`/`open_with_cache_bytes`.
fn rebuild_refs_in(db: &Database) -> Result<()> {
    let wt = db.begin_write()?;
    {
        let mut w = W::new(&wt)?;
        let mut want: BTreeMap<u64, u64> = BTreeMap::new();
        let mut files_by_cid: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for row in w.streams.iter()? {
            let (file, _) = row?;
            let file = file.value();
            let cid = content_id(file);
            *want.entry(cid).or_default() += 1;
            files_by_cid.entry(cid).or_default().push(file);
        }

        let mut stale_refs: Vec<u64> = Vec::new();
        for row in w.refs.iter()? {
            stale_refs.push(row?.0.value());
        }
        for k in stale_refs {
            w.refs.remove(k)?;
        }
        let mut stale_cf: Vec<(u64, u64)> = Vec::new();
        for row in w.content_files.iter()? {
            let (k, vals) = row?;
            let k = k.value();
            for v in vals {
                stale_cf.push((k, v?.value()));
            }
        }
        for (k, v) in stale_cf {
            w.content_files.remove(k, v)?;
        }

        for (cid, count) in want {
            w.refs.insert(cid, count)?;
        }
        for (cid, files) in files_by_cid {
            for file in files {
                w.content_files.insert(cid, file)?;
            }
        }

        w.meta
            .insert(DERIVED_VERSION_REFS_KEY, REFS_DERIVED_VERSION)?;
    }
    wt.commit()?;
    Ok(())
}

impl V2Store {
    /// Test oracle: recompute every derived table from the streams (the source
    /// of truth) and require the stored ones to match exactly: postings, the
    /// symbol index, the dictionary in both directions, and the describe
    /// catalog against a full scan. With `after_vacuum` the dictionary must
    /// also hold no dead term.
    #[cfg(test)]
    pub(crate) fn check_consistency(&self, after_vacuum: bool) {
        use std::collections::{BTreeMap, BTreeSet};
        let rt = self.db.begin_read().unwrap();
        let r = R::new(&rt).unwrap();
        let mut want_post: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
        let mut want_sym: BTreeSet<(String, u64)> = BTreeSet::new();
        let mut used: HashSet<u64> = HashSet::new();
        let mut want_refs: BTreeMap<u64, u64> = BTreeMap::new();
        let mut want_content_files: BTreeSet<(u64, u64)> = BTreeSet::new();
        for row in r.streams.iter().unwrap() {
            let (file, bytes) = row.unwrap();
            let file = file.value();
            let st = codec::decode(bytes.value()).unwrap();
            let node = r.node(file).unwrap().expect("stream without a file row");
            assert_eq!(node.kind, NodeKind::File);
            for (i, t) in st.tokens.iter().enumerate() {
                want_post.entry((t.term, file)).or_default().push(i);
                used.insert(t.term);
            }
            for (i, sy) in st.symbols.iter().enumerate() {
                want_sym.insert((
                    r.text(sy.name).unwrap().to_string(),
                    sub_id(TAG_SYM, file, i),
                ));
                used.insert(sy.name);
                used.extend(sy.lang_kind);
                used.extend(sy.owner);
            }
            // Every live stream owns exactly one refcount (content sharing
            // is off, so `content_id(file) == file`) and one `content_files`
            // entry pointing back at it.
            *want_refs.entry(content_id(file)).or_default() += 1;
            want_content_files.insert((content_id(file), file));
        }
        let mut got_refs: BTreeMap<u64, u64> = BTreeMap::new();
        for row in r.refs.iter().unwrap() {
            let (k, v) = row.unwrap();
            got_refs.insert(k.value(), v.value());
        }
        assert_eq!(got_refs, want_refs, "refs");
        let mut got_content_files: BTreeSet<(u64, u64)> = BTreeSet::new();
        for row in r.content_files.iter().unwrap() {
            let (k, vals) = row.unwrap();
            for v in vals {
                got_content_files.insert((k.value(), v.unwrap().value()));
            }
        }
        assert_eq!(got_content_files, want_content_files, "content_files");
        let mut got_post = BTreeMap::new();
        for row in r.post.iter().unwrap() {
            let (k, v) = row.unwrap();
            got_post.insert(k.value(), codec::posting_ordinals(v.value()).unwrap());
        }
        assert_eq!(got_post, want_post, "postings");
        let mut got_sym = BTreeSet::new();
        for row in r.sym_idx.iter().unwrap() {
            let (k, vals) = row.unwrap();
            for v in vals {
                got_sym.insert((k.value().to_string(), v.unwrap().value()));
            }
        }
        assert_eq!(got_sym, want_sym, "symbol index");
        let (mut ndict, mut nrev) = (0, 0);
        for row in r.dict.iter().unwrap() {
            let (k, id) = row.unwrap();
            ndict += 1;
            let text = r.text(id.value()).unwrap();
            assert_eq!(r.lookup(&text).unwrap(), Some(id.value()), "dict->rev");
            assert_eq!(is_hashed(k.value()), k.value().starts_with('\0'));
        }
        for row in r.rev.iter().unwrap() {
            let (_, block) = row.unwrap();
            for (id, text) in codec::decode_dict_block(block.value()).unwrap() {
                nrev += 1;
                assert_eq!(r.lookup(&text).unwrap(), Some(id), "rev->dict");
                if after_vacuum {
                    assert!(used.contains(&id), "dead term {id}");
                }
            }
        }
        assert_eq!(ndict, nrev, "dictionary directions");
        for t in &used {
            assert!(r.text(*t).is_ok(), "dangling term {t}");
        }
        assert_eq!(
            StoreRead::describe(self, None, None).unwrap(),
            r.describe_by_scan(None, None).unwrap(),
            "catalog"
        );
    }

    /// Test hook (ADR 0003 story 3, slice 3i differential test): `children`
    /// through the pre-3i eager `stream()` + `stream_tree` path, unconditionally
    /// -- calls [`R::children_fallback`] directly (the same function
    /// `R::children` falls back to when a range is not usable), so the
    /// differential test can compare it against the range-based result on
    /// the same (dense) data without risking drift from a duplicated body.
    #[cfg(test)]
    pub(crate) fn children_via_fallback(&self, id: NodeId) -> Result<Vec<Node>> {
        let rt = self.db.begin_read()?;
        let r = R::new(&rt)?;
        let (tag, file, i) = split_id(id);
        if tag != TAG_SYM {
            return Ok(Vec::new());
        }
        r.children_fallback(file, i)
    }

    /// Test hook, `descendants`' analog of [`V2Store::children_via_fallback`]:
    /// the pre-3i `file_walk` path, unconditionally.
    #[cfg(test)]
    pub(crate) fn descendants_via_fallback(&self, id: NodeId) -> Result<Vec<Node>> {
        let rt = self.db.begin_read()?;
        let r = R::new(&rt)?;
        let (tag, file, i) = split_id(id);
        let mut out = Vec::new();
        if tag == TAG_SYM {
            r.file_walk(file, Some(i), &mut out)?;
        }
        Ok(out)
    }

    /// Test hook, the file-level (slice 3j) analog of
    /// [`V2Store::children_via_fallback`]: `children` on a file through the
    /// pre-3j eager `stream()` + `stream_tree` path, unconditionally.
    #[cfg(test)]
    pub(crate) fn children_via_fallback_file(&self, file: NodeId) -> Result<Vec<Node>> {
        let rt = self.db.begin_read()?;
        let r = R::new(&rt)?;
        let Some(s) = r.stream(file)? else {
            return Ok(Vec::new());
        };
        let tree = stream_tree(&s);
        tree.top
            .iter()
            .map(|&it| r.item_node(file, &s, it))
            .collect()
    }

    /// Test hook, the file-level (slice 3j) analog of
    /// [`V2Store::descendants_via_fallback`]: `descendants` on a file through
    /// the pre-3j `file_walk` path, unconditionally.
    #[cfg(test)]
    pub(crate) fn descendants_via_fallback_file(&self, file: NodeId) -> Result<Vec<Node>> {
        let rt = self.db.begin_read()?;
        let r = R::new(&rt)?;
        let mut out = Vec::new();
        r.file_walk(file, None, &mut out)?;
        Ok(out)
    }

    /// Measurement hook (ADR 0003 story 3, slice 3j spike): compares the
    /// token records the eager `stream()` decode reads against what the
    /// range-based `children_ranged_file`/`descendants_ranged_file` path
    /// (now wired into `children`/`descendants`, see
    /// `file_level_complement_decode_cost_measured_on_this_repos_own_corpus`)
    /// actually costs, for one file entity id. `Ok(None)` when the file has
    /// no stream row or its ranges are not dense. Returns `(ntok,
    /// eager_decoded, ranged_decoded)`.
    #[cfg(test)]
    pub(crate) fn measure_file_level_complement(
        &self,
        file: NodeId,
    ) -> Result<Option<(usize, usize, usize)>> {
        let rt = self.db.begin_read()?;
        let r = R::new(&rt)?;

        codec::RECORDS_DECODED.with(|c| c.set(0));
        let Some(s) = r.stream(file)? else {
            return Ok(None);
        };
        let eager_decoded = codec::RECORDS_DECODED.with(|c| c.get());
        let ntok = s.tokens.len();

        codec::RECORDS_DECODED.with(|c| c.set(0));
        let ranged = r.children_ranged_file(file)?;
        let ranged_decoded = codec::RECORDS_DECODED.with(|c| c.get());

        Ok(ranged.map(|_| (ntok, eager_decoded, ranged_decoded)))
    }

    /// Test hook: add a raw symbol-index entry (to simulate a stale index).
    #[cfg(test)]
    pub(crate) fn inject_symbol_index(&self, name: &str, id: u64) {
        let wt = self.db.begin_write().unwrap();
        wt.open_multimap_table(SYMBOLS)
            .unwrap()
            .insert(name, id)
            .unwrap();
        wt.commit().unwrap();
    }

    /// Test hook: make `file`'s content id an *extra* reference on
    /// `sharing_file`'s already-ingested content (bumping `refs[content_id
    /// (sharing_file)]` and adding a `content_files` entry for `file`),
    /// without actually re-ingesting `file`'s stream under that id. This is
    /// the only way to exercise `remove_content`'s refcount-gated (not
    /// immediate) delete branch before story 18's real content-sharing
    /// fan-out exists: today `content_id(file) == file` always, so every
    /// real refcount is always exactly 1 and "decrement then delete only at
    /// zero" is otherwise indistinguishable from "always delete" (found by
    /// QA review, PR #42).
    #[cfg(test)]
    pub(crate) fn inject_extra_content_ref(&self, file: u64, sharing_file: u64) {
        let cid = content_id(sharing_file);
        let wt = self.db.begin_write().unwrap();
        {
            let mut refs = wt.open_table(REFS).unwrap();
            let count = refs.get(cid).unwrap().map(|v| v.value()).unwrap_or(0);
            refs.insert(cid, count + 1).unwrap();
            wt.open_multimap_table(CONTENT_FILES)
                .unwrap()
                .insert(cid, file)
                .unwrap();
        }
        wt.commit().unwrap();
    }

    /// Open or create a database file. Refuses a file of any other layout
    /// before writing anything of its own (redb may still repair a file that
    /// was not cleanly closed): a retired v1 file gets
    /// [`StoreError::LegacyFormat`] (re-index from source, or convert with
    /// the `v1-last` release).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_cache_bytes(path, None)
    }

    /// `open`, with an explicit cache size in bytes (redb's default is 1
    /// GiB, split 9:1 between its read and write caches). `None` keeps
    /// redb's default.
    pub fn open_with_cache_bytes(
        path: impl AsRef<Path>,
        cache_bytes: Option<usize>,
    ) -> Result<Self> {
        let mut builder = Database::builder();
        if let Some(bytes) = cache_bytes {
            builder.set_cache_size(bytes);
        }
        let db = builder.create(path.as_ref()).map_err(|e| match e {
            DatabaseError::DatabaseAlreadyOpen => {
                StoreError::Locked(path.as_ref().display().to_string())
            }
            e => open_failed(path.as_ref(), &e),
        })?;
        Self::from_db(db, path.as_ref(), cache_bytes)
    }

    /// [`open_with_cache_bytes`](Self::open_with_cache_bytes) over a redb
    /// [`redb::StorageBackend`] instead of a file (the server's power-cut
    /// durability tests, ADR 0004 D7). `path` only names the store in
    /// messages; nothing is read from or written to it. The file-based
    /// operations ([`compact`](Self::compact), `export_snapshot`,
    /// `install_snapshot`) are not meaningful on such a store.
    pub fn open_with_backend(
        path: impl AsRef<Path>,
        backend: impl redb::StorageBackend,
        cache_bytes: Option<usize>,
    ) -> Result<Self> {
        let mut builder = Database::builder();
        if let Some(bytes) = cache_bytes {
            builder.set_cache_size(bytes);
        }
        let db = builder
            .create_with_backend(backend)
            .map_err(|e| open_failed(path.as_ref(), &e))?;
        Self::from_db(db, path.as_ref(), cache_bytes)
    }

    /// Check (or stamp, for a new file) the schema of an opened database
    /// and wrap it.
    fn from_db(db: Database, path: &Path, cache_bytes: Option<usize>) -> Result<Self> {
        let found = {
            let rt = db.begin_read()?;
            match rt.open_table(META) {
                Ok(t) => t.get("schema_version")?.map(|v| v.value()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        };
        match found {
            Some(V2_SCHEMA_VERSION) => {}
            Some(v) if UPGRADABLE_SCHEMA_VERSIONS.contains(&v) => {
                // Issue #137, ADR 0007 C6: the old layouts are subsets of
                // the new one, so the upgrade is one small commit: the
                // restamp, plus (from 11, epic story 43) the catalog's
                // encoding counts recounted from the File nodes.
                let wt = db.begin_write()?;
                if v == 11 {
                    rebuild_encoding_catalog_in(&wt)?;
                } else {
                    // Every 9/10 file is all UTF-8: its catalog is already
                    // correct at version 2 (absence means zero).
                    wt.open_table(META)?
                        .insert("catalog_version", CATALOG_VERSION)?;
                }
                wt.open_table(META)?
                    .insert("schema_version", V2_SCHEMA_VERSION)?;
                wt.commit()?;
            }
            Some(v) if crate::LEGACY_SCHEMA_VERSIONS.contains(&v) => {
                return Err(StoreError::LegacyFormat {
                    path: path.display().to_string(),
                    version: v,
                })
            }
            Some(v) => return Err(StoreError::SchemaMismatch { found: v }),
            None => {
                let wt = db.begin_write()?;
                {
                    let mut m = wt.open_table(META)?;
                    m.insert("schema_version", V2_SCHEMA_VERSION)?;
                    m.insert("next_id", 1)?;
                    m.insert("next_term", 0)?;
                    m.insert("next_batch_id", 0)?;
                    m.insert("stream_format", u64::from(STREAM_FORMAT))?;
                    m.insert("catalog_version", CATALOG_VERSION)?;
                    // A brand-new file has no streams yet, so empty
                    // refs/content_files are already correct; stamp the
                    // current derived_version directly instead of running
                    // rebuild_refs on nothing.
                    m.insert(DERIVED_VERSION_REFS_KEY, REFS_DERIVED_VERSION)?;
                    wt.open_table(CATALOG)?;
                    wt.open_table(NODES)?;
                    wt.open_table(NAMES)?;
                    wt.open_table(DICT)?;
                    wt.open_table(DICT_REV)?;
                    wt.open_table(STREAMS)?;
                    wt.open_table(POST)?;
                    wt.open_multimap_table(CHILDREN)?;
                    wt.open_multimap_table(SYMBOLS)?;
                    wt.open_table(REFS)?;
                    wt.open_multimap_table(CONTENT_FILES)?;
                    wt.open_table(OPEN_BATCH)?;
                }
                wt.commit()?;
            }
        }

        // ADR 0007 C6 / epic story 43: a catalog whose version is missing or
        // lower than [CATALOG_VERSION] gets its encoding counts recounted
        // (a newer one is left alone: the schema gate above already refuses
        // a newer layout). A current file does not write at all.
        let catalog_version = {
            let rt = db.begin_read()?;
            match rt.open_table(META) {
                Ok(t) => t.get("catalog_version")?.map(|v| v.value()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        };
        if catalog_version.is_none_or(|v| v < CATALOG_VERSION) {
            let wt = db.begin_write()?;
            rebuild_encoding_catalog_in(&wt)?;
            wt.commit()?;
        }

        // Soft self-heal (ADR 0003 story 9): an existing file whose
        // derived_version lags or is missing (any v2 file written before
        // this mechanism existed) gets refs/content_files rebuilt
        // automatically, silently -- this is not an error, and never
        // refuses the open. A file already at the current version does not
        // write at all, so a plain reopen stays byte-identical.
        let derived_refs_version = {
            let rt = db.begin_read()?;
            match rt.open_table(META) {
                Ok(t) => t.get(DERIVED_VERSION_REFS_KEY)?.map(|v| v.value()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        };
        if derived_refs_version != Some(REFS_DERIVED_VERSION) {
            eprintln!(
                "memory-graph: v2 store {}: refs/content_files derived_version \
                 {derived_refs_version:?} != current {REFS_DERIVED_VERSION}; \
                 self-healing (rebuilding) on open",
                path.display()
            );
            rebuild_refs_in(&db)?;
        }

        Ok(Self {
            db,
            registry: Registry::default(),
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            #[cfg(test)]
            skip_open_batch_marker: false,
            cache_bytes,
            path: path.to_path_buf(),
            max_snapshot_age: DEFAULT_MAX_SNAPSHOT_AGE,
            snapshot_tracker: Arc::new(Mutex::new(SnapshotTracker::default())),
            marked_commit_hook: None,
        })
    }

    /// Install (or clear) the [`MarkedCommitHook`]. Test-only fault
    /// injection; never set outside tests.
    pub fn set_marked_commit_hook(&mut self, hook: Option<MarkedCommitHook>) {
        self.marked_commit_hook = hook;
    }

    fn before_marked_commit(&self, marker: &RaftMarker) -> Result<()> {
        match &self.marked_commit_hook {
            Some(h) => h(marker),
            None => Ok(()),
        }
    }

    /// Remove the Raft state (`RAFT_SM`: marker and membership) from this
    /// store, so a server restoring it with `--bootstrap --restore` starts a
    /// new cluster whose log begins at 0 (ADR 0004 D6). A store without the
    /// table is left untouched (no write at all).
    pub fn clear_raft_state(&self) -> Result<()> {
        {
            let rt = self.db.begin_read()?;
            match rt.open_table(RAFT_SM) {
                Ok(_) => {}
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        let wt = self.db.begin_write()?;
        wt.delete_table(RAFT_SM)?;
        wt.commit()?;
        Ok(())
    }

    /// Set the chunk cap of `index_batch`: the write transaction commits once
    /// the source bytes it has ingested reach `bytes` (at least 1), and the
    /// batch continues in a new one. The cap is soft: a file is never split,
    /// so one file larger than the cap is a chunk of its own.
    pub fn set_chunk_bytes(&mut self, bytes: usize) {
        self.chunk_bytes = bytes.max(1);
    }

    /// Set the max age (ADR 0003 story 10, Q6) a snapshot handle from
    /// `snapshot()` may live before reads through it start returning
    /// `StoreError::SnapshotExpired`. Applies only to snapshots taken after
    /// this call; already-open handles keep the max age they were created
    /// with. Default [`DEFAULT_MAX_SNAPSHOT_AGE`] (15 minutes).
    pub fn set_max_snapshot_age(&mut self, max_age: Duration) {
        self.max_snapshot_age = max_age;
    }

    /// Garbage-collect the dictionary (ADR story 3): remove every term that
    /// no posting, symbol name or symbol kind refers to any more. Replacing
    /// or pruning a file leaves its terms behind; term ids are never reused,
    /// so removing a dead one cannot change any live id. One write
    /// transaction; it reads every stream's symbol section and the postings'
    /// keys, so its cost is linear in the store.
    pub fn vacuum(&self) -> Result<VacuumStats> {
        let wt = self.db.begin_write()?;
        let (stats, nothing) = Self::vacuum_in(&wt)?;
        if nothing {
            // Nothing to remove: abandon the transaction so the file is
            // byte-for-byte unchanged (a commit would rewrite its header).
            wt.abort()?;
        } else {
            wt.commit()?;
        }
        Ok(stats)
    }

    /// The body of [`vacuum`](Self::vacuum) inside the caller's transaction;
    /// the flag is "nothing was removed" (the caller may then abort).
    fn vacuum_in(wt: &redb::WriteTransaction) -> Result<(VacuumStats, bool)> {
        let stats = {
            let w = W::new(wt)?;
            let mut live: HashSet<u64> = HashSet::new();
            for r in w.post.iter()? {
                live.insert(r?.0.value().0);
            }
            for r in w.streams.iter()? {
                let raw = r?.1;
                let syms = read_stats::uncounted(|| codec::decode_lazy(raw.value())?.symbols())?;
                for s in syms {
                    live.insert(s.name);
                    live.extend(s.lang_kind);
                    live.extend(s.owner);
                }
            }
            let mut all: Vec<(u64, String)> = Vec::new();
            for r in w.rev.iter()? {
                let (_, block) = r?;
                all.extend(codec::decode_dict_block(block.value())?);
            }
            let mut dead: Vec<(u64, String)> = Vec::new();
            let mut kept: Vec<(u64, String)> = Vec::new();
            for (id, text) in all {
                if live.contains(&id) {
                    kept.push((id, text));
                } else {
                    dead.push((id, text));
                }
            }
            let mut w = w;
            // Note: removing a middle key of a `.n` probe chain would leave a
            // hole that hides later keys from `lookup`. That needs a real
            // SHA-256 collision, which is practically unreachable, so it is
            // not handled (no re-keying or tombstone).
            for (id, text) in &dead {
                let (key, found) = w.dict_key(text, Some(*id))?;
                if found.is_some() {
                    w.dict.remove(key.as_str())?;
                }
            }
            if !dead.is_empty() {
                // Repack the reverse dictionary densely from `kept` (ADR
                // 0003 story 5): after this, block boundaries no longer
                // line up with `id / DICT_BLOCK` since dead ids leave gaps,
                // which is why lookups always binary-search by each block's
                // first id instead of assuming that arithmetic.
                let old_blocks = w.rev.len()?;
                for i in 0..old_blocks {
                    w.rev.remove(i)?;
                }
                for (i, block) in pack_dict_blocks(&kept).iter().enumerate() {
                    w.rev.insert(i as u64, block.as_slice())?;
                }
            }
            let stats = VacuumStats {
                terms_removed: dead.len(),
                terms_kept: kept.len(),
            };
            (stats, dead.is_empty())
        };
        Ok(stats)
    }

    // ----- Raft state-machine bookkeeping (ADR 0004 D5/D7) -----
    //
    // Not on the `Store` trait: only a server applying a replicated log
    // needs them. Every `*_marked` write is exactly its plain counterpart
    // plus, in the same (single) transaction, the marker and membership,
    // and it refuses an entry at or below the stored marker without writing.

    /// The stored [`RaftMarker`], or `Ok(None)` when the store has never
    /// been written by a marked write (the `raft_sm` table does not exist).
    pub fn raft_marker(&self) -> Result<Option<RaftMarker>> {
        let rt = self.db.begin_read()?;
        Self::raft_row(&rt, RAFT_LAST_APPLIED)?
            .as_deref()
            .map(RaftMarker::decode)
            .transpose()
    }

    /// The opaque membership bytes the server last stored, if any.
    pub fn raft_membership(&self) -> Result<Option<Vec<u8>>> {
        let rt = self.db.begin_read()?;
        Self::raft_row(&rt, RAFT_MEMBERSHIP)
    }

    fn raft_row(rt: &ReadTransaction, key: &str) -> Result<Option<Vec<u8>>> {
        match rt.open_table(RAFT_SM) {
            Ok(t) => Ok(t.get(key)?.map(|v| v.value().to_vec())),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Refuse `marker` if the store's marker is already at or past its
    /// index, else write it (and `membership` when given) into `wt`,
    /// creating `raft_sm` on first use. `Err` leaves `wt` to be dropped
    /// (aborted) by the caller, so nothing is written.
    fn stamp_marker(
        wt: &redb::WriteTransaction,
        marker: RaftMarker,
        membership: Option<&[u8]>,
    ) -> Result<()> {
        let mut t = wt.open_table(RAFT_SM)?;
        if let Some(cur) = t.get(RAFT_LAST_APPLIED)? {
            let cur = RaftMarker::decode(cur.value())?;
            if cur.index >= marker.index {
                return Err(StoreError::AlreadyApplied {
                    index: marker.index,
                });
            }
        }
        t.insert(RAFT_LAST_APPLIED, marker.encode().as_slice())?;
        if let Some(m) = membership {
            t.insert(RAFT_MEMBERSHIP, m)?;
        }
        Ok(())
    }

    /// [`Store::index_prepared`] forced into **one** transaction (the chunk
    /// cap is `usize::MAX` for this call whatever the store's setting) that
    /// also records `marker` and `membership` (ADR 0004 D5: apply is
    /// all-or-nothing with its marker, and transaction boundaries never
    /// depend on a per-node setting). [`StoreError::AlreadyApplied`]
    /// without writing when the stored marker's index is `>=
    /// marker.index`.
    pub fn index_prepared_marked(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
        marker: RaftMarker,
        membership: Option<&[u8]>,
    ) -> Result<Vec<Result<IngestStats>>> {
        let n = files.len();
        let mut it = files.into_iter();
        self.commit_each(
            org,
            repo,
            n,
            opts,
            usize::MAX,
            Some((marker, membership)),
            |_, _| Ok(it.next().expect("one prepared file per slot")),
        )
    }

    /// [`Store::prune_files`] (never a dry run) plus the marker, in one
    /// transaction; same marker rule as [`index_prepared_marked`](Self::index_prepared_marked).
    pub fn prune_files_marked(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        marker: RaftMarker,
        membership: Option<&[u8]>,
    ) -> Result<Vec<String>> {
        let wt = self.db.begin_write()?;
        Self::stamp_marker(&wt, marker, membership)?;
        let removed = Self::prune_in(&wt, org, repo, keep, false)?;
        self.before_marked_commit(&marker)?;
        wt.commit()?;
        Ok(removed)
    }

    /// [`Store::vacuum`] plus the marker, in one transaction, which always
    /// commits (the marker is written even when no term was dead); same
    /// marker rule as [`index_prepared_marked`](Self::index_prepared_marked).
    pub fn vacuum_marked(
        &self,
        marker: RaftMarker,
        membership: Option<&[u8]>,
    ) -> Result<VacuumStats> {
        let wt = self.db.begin_write()?;
        Self::stamp_marker(&wt, marker, membership)?;
        let (stats, _) = Self::vacuum_in(&wt)?;
        self.before_marked_commit(&marker)?;
        wt.commit()?;
        Ok(stats)
    }

    /// [`Store::ingest_file_with_origin`] plus the marker, in one
    /// transaction; same marker rule as
    /// [`index_prepared_marked`](Self::index_prepared_marked).
    #[allow(clippy::too_many_arguments)]
    pub fn ingest_file_marked(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
        marker: RaftMarker,
        membership: Option<&[u8]>,
    ) -> Result<IngestStats> {
        validate_spans(ex)?;
        let wt = self.db.begin_write()?;
        Self::stamp_marker(&wt, marker, membership)?;
        let stats = Self::ingest_validated(
            &wt,
            org,
            repo,
            path,
            language,
            ex,
            FileMeta::extracted(origin),
        )?;
        self.before_marked_commit(&marker)?;
        wt.commit()?;
        Ok(stats)
    }

    /// Record `marker` (and `membership`) and nothing else: a log entry
    /// with no store effect (openraft blank/membership entries); same
    /// marker rule as [`index_prepared_marked`](Self::index_prepared_marked).
    pub fn mark_only(&self, marker: RaftMarker, membership: Option<&[u8]>) -> Result<()> {
        let wt = self.db.begin_write()?;
        Self::stamp_marker(&wt, marker, membership)?;
        self.before_marked_commit(&marker)?;
        wt.commit()?;
        Ok(())
    }

    /// Recompute `refs` and `content_files` from scratch by scanning every
    /// live file's stream row (the source of truth: a stream row exists iff
    /// its file is live, see `remove_content`) and rewriting both tables to
    /// match, in one write transaction. Mirrors the oracle already used by
    /// `check_consistency` (ADR 0003 story 3, slice 3l), so this method and
    /// that test-only oracle agree on what "correct" means by construction.
    ///
    /// Today `content_id(file) == file` always (content sharing, story 18, is
    /// off), so the rebuilt state is simply refcount 1 and one
    /// `content_files` entry per live file; the loop is written in terms of
    /// `content_id` so it stays correct once fan-out lands.
    ///
    /// Wired into [`V2Store::open`]/[`V2Store::open_with_cache_bytes`]
    /// (ADR 0003 story 9): a file whose `derived_version_refs_content_files`
    /// (see [`REFS_DERIVED_VERSION`]) lags or is missing is rebuilt
    /// automatically on open, so calling this method by hand is normally
    /// only needed for diagnostics or a manual repair.
    pub fn rebuild_refs(&self) -> Result<()> {
        rebuild_refs_in(&self.db)
    }

    /// Rebuild the store into a brand-new file by copying every row of every
    /// table verbatim (no liveness decisions here: run [`V2Store::vacuum`]
    /// first to drop dead dictionary terms), then atomically rename the new
    /// file over the original path (ADR 0003, "Snapshot isolation (decision
    /// D3)": rebuild into a new file, then atomic rename). Closes the gap
    /// measured by the `churn`/`prune_churn` harnesses: redb never shrinks a
    /// file on its own, even after `vacuum`; `compact` is the only way to
    /// reclaim the freed pages.
    ///
    /// Single-process only: this claims no cross-process safety guarantee
    /// beyond what today's CLI already has (one process at a time holds
    /// redb's exclusive file lock). No daemon exists yet (ADR story 12a).
    ///
    /// Consumes `self` (rather than taking `&mut self`) so the old
    /// `Database` handle is dropped by ordinary ownership before the rename,
    /// as defense in depth against a platform or redb version where a
    /// `Database` still open on `path` would make the rename fail (not
    /// reproduced as a failure on this repo's current dev/CI platforms, but
    /// cheap to guarantee by construction rather than assume away). Returns
    /// a fresh `V2Store` reopened from the renamed file, with this store's
    /// `chunk_bytes` and extractor registry carried over.
    ///
    /// On `Err`, `self` is gone (consumed) but the original file at `path`
    /// is untouched and safely reopenable with [`V2Store::open`]: every
    /// failure path removes the temp file and returns before the old handle
    /// is dropped or the rename is attempted, so nothing is ever renamed
    /// over `path` unless the whole copy already committed.
    pub fn compact(self) -> Result<(Self, CompactStats)> {
        let io = |e: std::io::Error| StoreError::Storage(e.to_string());
        let path = self.path.clone();
        // PID alone collides if `compact` is ever called more than once
        // concurrently in one process (not today's one-shot CLI, but a
        // future embedder might); the nanosecond timestamp is cheap,
        // dependency-free insurance against that.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp = path.with_extension(format!("compact-{}-{unique}.redb.tmp", std::process::id()));
        // Best-effort cleanup of a leftover temp file from a prior crashed run
        // at this exact (pid, nanos) pair: vanishingly unlikely to collide,
        // but free to guard. A process that is hard-killed (SIGKILL /
        // `Stop-Process -Force`) mid-`compact` never reaches the `Err`
        // cleanup below, so it can leave its own `.compact-<pid>-<nanos>.tmp`
        // file behind under a *different* pid/nanos than any later run --
        // this is disk clutter only (see issue #32). `path` itself is never
        // touched until the rename below succeeds, so a subsequent `compact`
        // is unaffected and always produces a correct result; it just does
        // not reclaim orphans from earlier kills. Actively globbing the
        // directory for stale `*.compact-*.tmp` files and deleting them was
        // considered and rejected: this backend does not track which other
        // OS processes may have `--db` pointed at the same file mid-compact
        // (the CLI is one-shot per invocation, but nothing prevents two
        // processes from being pointed at the same path), and the encoded
        // pid alone cannot distinguish "a live compact in another process on
        // this pid" from "a dead pid that has since been recycled by the OS"
        // without external liveness infrastructure this crate does not have.
        // Deleting a temp file out from under a concurrently-running compact
        // (same or other process) would be a correctness bug, which is worse
        // than the disk clutter it would fix (the retired `migrate` made the
        // same call for the same reason). Follow-up
        // if orphan accumulation becomes an operational concern: a
        // `repair`/`vacuum` CLI step that globs and removes stale
        // `*.compact-*.tmp` files with a human confirming no other process
        // has the store open (see issue #32).
        let _ = std::fs::remove_file(&tmp);

        let before_bytes = match self.export_snapshot(&tmp) {
            Ok(stats) => stats.before_bytes,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };

        let V2Store {
            db,
            registry,
            chunk_bytes,
            cache_bytes,
            path,
            max_snapshot_age,
            snapshot_tracker: _,
            marked_commit_hook,
            #[cfg(test)]
                skip_open_batch_marker: _,
        } = self;
        // Drop the old handle before renaming over its path (Windows will
        // not allow the rename while any `Database` still has it open).
        drop(db);
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }

        let mut reopened = Self::open_with_cache_bytes(&path, cache_bytes)?;
        reopened.chunk_bytes = chunk_bytes;
        reopened.registry = registry;
        // Any snapshot handles taken on the pre-compaction store are already
        // invalidated by construction (`compact` consumes `self`, so no
        // caller can still hold one); the reopened store's tracker starts
        // empty and carries over only the configured max age.
        reopened.max_snapshot_age = max_snapshot_age;
        reopened.marked_commit_hook = marked_commit_hook;
        let after_bytes = std::fs::metadata(&path).map_err(io)?.len();
        Ok((
            reopened,
            CompactStats {
                before_bytes,
                after_bytes,
            },
        ))
    }

    /// Write a consistent copy of this store to a brand-new redb file at
    /// `dest` (ADR 0004 D7: a Raft snapshot; also the copy step of
    /// [`compact`](Self::compact)): one read transaction, every table copied
    /// row by row (`raft_sm` too, when it exists), one write transaction on
    /// the new file. The store stays open and usable; `dest` must not exist
    /// (`Rejected`) and is removed again if the copy fails. Returns the
    /// sizes of this file and of the copy.
    pub fn export_snapshot(&self, dest: &Path) -> Result<CompactStats> {
        let io = |e: std::io::Error| StoreError::Storage(e.to_string());
        if dest.exists() {
            return Err(StoreError::Rejected(format!(
                "snapshot destination `{}` already exists",
                dest.display()
            )));
        }
        let before_bytes = std::fs::metadata(&self.path).map_err(io)?.len();
        let build = || -> Result<()> {
            let new_db = Database::create(dest)?;
            let rt = self.db.begin_read()?;
            let wt = new_db.begin_write()?;
            copy_table(&rt, &wt, META)?;
            copy_table(&rt, &wt, CATALOG)?;
            copy_table(&rt, &wt, NODES)?;
            copy_table(&rt, &wt, NAMES)?;
            copy_multimap(&rt, &wt, CHILDREN)?;
            copy_multimap(&rt, &wt, SYMBOLS)?;
            copy_table(&rt, &wt, DICT)?;
            copy_table(&rt, &wt, DICT_REV)?;
            copy_table(&rt, &wt, STREAMS)?;
            copy_table(&rt, &wt, POST)?;
            copy_table(&rt, &wt, REFS)?;
            copy_multimap(&rt, &wt, CONTENT_FILES)?;
            copy_table(&rt, &wt, OPEN_BATCH)?;
            // Lazily created by the marked writes (ADR 0004 D7): copied
            // when present, never created here, so an unreplicated store's
            // copy has no `raft_sm` either.
            match rt.open_table(RAFT_SM) {
                Ok(src) => {
                    let mut dst = wt.open_table(RAFT_SM)?;
                    for row in src.iter()? {
                        let (k, v) = row?;
                        dst.insert(k.value(), v.value())?;
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(e.into()),
            }
            wt.commit()?;
            drop(rt);
            drop(new_db);
            Ok(())
        };
        if let Err(e) = build() {
            let _ = std::fs::remove_file(dest);
            return Err(e);
        }
        let after_bytes = std::fs::metadata(dest).map_err(io)?.len();
        Ok(CompactStats {
            before_bytes,
            after_bytes,
        })
    }

    /// Replace the store file at `path` by the snapshot file at `src` (ADR
    /// 0004 D7: a follower installing a leader's snapshot). `src` must be a
    /// store in the current format ([`detect_format`](crate::detect_format);
    /// a file with no schema is `Rejected`). The caller guarantees no store
    /// is open on `path` (Windows refuses to rename over an open file). The
    /// swap is rename-based: an existing `path` is first moved aside to
    /// `<path>.old`, `src` is renamed into place, and the old copy is
    /// removed; if the second rename fails the old copy is moved back, so
    /// `path` is never left missing. `src` is consumed on success.
    pub fn install_snapshot(path: &Path, src: &Path) -> Result<()> {
        let io = |e: std::io::Error| StoreError::Storage(e.to_string());
        if crate::detect_format(src)?.is_none() {
            return Err(StoreError::Rejected(format!(
                "`{}` is not a store file (no schema version)",
                src.display()
            )));
        }
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        let had_old = path.exists();
        if had_old {
            std::fs::rename(path, &old).map_err(io)?;
        }
        if let Err(e) = std::fs::rename(src, path) {
            if had_old {
                let _ = std::fs::rename(&old, path);
            }
            return Err(io(e));
        }
        if had_old {
            let _ = std::fs::remove_file(&old);
        }
        Ok(())
    }

    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.registry.register(e);
    }

    #[allow(clippy::too_many_arguments)]
    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats> {
        // ADR 0007 C2: the single-file path decodes, detects and
        // fingerprints through `prepare_file`, like every other write path,
        // so it cannot drift from `index_batch`, `--server` or Raft.
        let f = BatchFile {
            path,
            bytes,
            language,
            origin,
            encoding: opts.encoding,
            strict_encoding: opts.strict_encoding,
        };
        // The unchanged check runs inside this write transaction, so it is
        // authoritative, and nothing is extracted for an unchanged file.
        let wt = self.db.begin_write()?;
        let mut unchanged = None;
        let mut p = prepare_file(&self.registry, org, repo, &f, opts, |p, l, fp| {
            unchanged = check_unchanged(&wt, org, repo, p, l, fp, origin)?;
            Ok(unchanged.is_some())
        })?;
        let work = std::mem::replace(
            &mut p.work,
            crate::api::Prepared::Rejected(StoreError::Rejected(String::new())),
        );
        let ex = match work {
            crate::api::Prepared::Rejected(StoreError::InvalidSpan(why)) => {
                // The single-file path reports the span error bare (as it
                // did before it went through `prepare_file`).
                let why = why
                    .strip_prefix(&format!("`{}`: ", p.path))
                    .map(str::to_owned)
                    .unwrap_or(why);
                return Err(StoreError::InvalidSpan(why));
            }
            crate::api::Prepared::Rejected(e) => return Err(e),
            crate::api::Prepared::Unchanged(_) => {
                // The check ran in this transaction and refreshed `origin`
                // if it differed; commit that.
                let (stats, dirty) = unchanged.expect("found unchanged in this transaction");
                if dirty {
                    wt.commit()?;
                }
                return Ok(stats);
            }
            crate::api::Prepared::Extracted(ex) => ex,
            crate::api::Prepared::Remote(_) => unreachable!("prepare_file never keeps raw bytes"),
        };
        let mut stats = Self::ingest_prepped(
            &wt,
            org,
            repo,
            &p.path,
            &p.language,
            &ex,
            FileMeta::of(&p),
            None,
        )?;
        wt.commit()?;
        stats.span_warning = p.span_warning.take();
        Ok(stats)
    }

    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        validate_spans(ex)?;
        let wt = self.db.begin_write()?;
        let stats = Self::ingest_validated(
            &wt,
            org,
            repo,
            path,
            language,
            ex,
            FileMeta::extracted(origin),
        )?;
        wt.commit()?;
        Ok(stats)
    }

    /// Index many files of one repo in chunked write transactions.
    ///
    /// Atomicity: a batch is atomic **per chunk**, not as a whole. A chunk
    /// commits when the source bytes it took in reach the chunk cap
    /// ([`DEFAULT_CHUNK_BYTES`], see [`V2Store::set_chunk_bytes`]) and at the
    /// end of the batch. A storage error aborts only the chunk in progress:
    /// earlier chunks stay committed and visible, the current chunk leaves
    /// nothing behind, and later files are not extracted. Per-file failures
    /// (binary, a lossy strict decode, too large, invalid spans) yield a per-file `Err` and never
    /// abort a chunk. A batch smaller than the cap is one transaction, so it
    /// is all-or-nothing, as before. Re-running the batch after a failure
    /// skips the files already stored (unchanged fingerprint).
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        self.commit_each(
            org,
            repo,
            files.len(),
            opts,
            self.chunk_bytes,
            None,
            |wt, i| {
                let f = &files[i];
                prepare_file(&self.registry, org, repo, f, opts, |path, lang, fp| {
                    Ok(check_unchanged(wt, org, repo, path, lang, fp, f.origin)?.is_some())
                })
            },
        )
    }

    /// See [`Store::prepare`]: a one-file
    /// [`prepare_with`](Self::prepare_with), whose snapshot is the last
    /// committed state. Its read transaction is not ordered against commits,
    /// so a caller that needs the file bytes independent of thread timing
    /// (the CLI's `index`, #158) takes one snapshot per batch before any
    /// commit and calls `prepare_with` instead (#172). Any other caller
    /// that prepares concurrently with commits still gets correct results,
    /// but the database bytes then depend on thread timing, so it should
    /// prefer `prepare_with` too.
    fn prepare(
        &self,
        org: &str,
        repo: &str,
        f: &BatchFile<'_>,
        opts: IndexOptions,
    ) -> Result<PreparedFile> {
        let mut p = prepare_file(&self.registry, org, repo, f, opts, |path, _, fp| {
            PRECHECK_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            stored_fingerprint_matches(&self.db.begin_read()?, org, repo, path, fp)
        })?;
        Self::build_v2_prep(&mut p);
        Ok(p)
    }

    /// Lay out a prepared extraction off the commit path.
    fn build_v2_prep(p: &mut PreparedFile) {
        if let crate::api::Prepared::Extracted(ex) = &p.work {
            p.v2 = Some(Box::new(V2Prep::build(ex)));
        }
    }

    /// See [`Store::fingerprint_snapshot`]: one read transaction, closed
    /// before this returns.
    fn fingerprint_snapshot(&self, org: &str, repo: &str) -> Result<crate::FingerprintSnapshot> {
        let fps = stored_fingerprints(&self.db.begin_read()?, org, repo)?;
        Ok(crate::FingerprintSnapshot {
            org: org.to_string(),
            repo: repo.to_string(),
            fps,
            taken: true,
        })
    }

    /// See [`Store::prepare_with`]. Opens no transaction: the pre-check
    /// reads only `known` (a snapshot for another org/repo, or none, means
    /// "not known unchanged", and the commit decides).
    fn prepare_with(
        &self,
        org: &str,
        repo: &str,
        f: &BatchFile<'_>,
        opts: IndexOptions,
        known: &crate::FingerprintSnapshot,
    ) -> Result<PreparedFile> {
        if !known.is_for(org, repo) {
            return self.prepare(org, repo, f, opts);
        }
        let mut p = prepare_file(&self.registry, org, repo, f, opts, |path, _, fp| {
            Ok(known.matches(org, repo, path, fp).unwrap_or(false))
        })?;
        Self::build_v2_prep(&mut p);
        Ok(p)
    }

    /// See [`Store::index_prepared`]: the same chunked transactions and
    /// open-batch marker as `index_batch`.
    fn index_prepared(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        Ok(self.index_prepared_counted(org, repo, files, opts)?.0)
    }

    /// See [`Store::index_prepared_counted`]: `index_prepared` plus how many
    /// redb write transactions it committed.
    fn index_prepared_counted(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<(Vec<Result<IngestStats>>, u64)> {
        let n = files.len();
        let mut it = files.into_iter();
        self.commit_each_counted(org, repo, n, opts, self.chunk_bytes, None, |_, _| {
            Ok(it.next().expect("one prepared file per slot"))
        })
    }

    /// The chunked write loop shared by `index_batch`, `index_prepared` and
    /// `index_prepared_marked`. `next` yields the `i`-th prepared file (it
    /// may read the current transaction), committed before the next one is
    /// asked for. `chunk_bytes` is the chunk cap for this call; a `marker`
    /// (with `usize::MAX`, so the batch is one transaction) is stamped into
    /// that transaction first, and refused before anything is written when
    /// it is not above the stored one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_each(
        &self,
        org: &str,
        repo: &str,
        n: usize,
        opts: IndexOptions,
        chunk_bytes: usize,
        marker: Option<(RaftMarker, Option<&[u8]>)>,
        next: impl FnMut(&redb::WriteTransaction, usize) -> Result<PreparedFile>,
    ) -> Result<Vec<Result<IngestStats>>> {
        Ok(self
            .commit_each_counted(org, repo, n, opts, chunk_bytes, marker, next)?
            .0)
    }

    /// `commit_each`, also returning how many write transactions committed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_each_counted(
        &self,
        org: &str,
        repo: &str,
        n: usize,
        opts: IndexOptions,
        chunk_bytes: usize,
        marker: Option<(RaftMarker, Option<&[u8]>)>,
        mut next: impl FnMut(&redb::WriteTransaction, usize) -> Result<PreparedFile>,
    ) -> Result<(Vec<Result<IngestStats>>, u64)> {
        let mut commits = 0u64;
        // A marked batch is one transaction whatever the caller passed: its
        // marker must commit atomically with all of its data.
        let chunk_bytes = if marker.is_some() {
            usize::MAX
        } else {
            chunk_bytes
        };
        let mut wt = self.db.begin_write()?;
        if let Some((marker, membership)) = marker {
            Self::stamp_marker(&wt, marker, membership)?;
        }
        let batch_id = Self::next_batch_id(&wt)?;
        #[cfg(test)]
        let skip_marker = self.skip_open_batch_marker;
        #[cfg(not(test))]
        let skip_marker = false;
        if !skip_marker {
            Self::mark_open_batch(&wt, batch_id, org, repo)?;
        }
        let mut in_txn = 0usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let p = next(&wt, i)?;
            let len = p.bytes_len;
            let r = commit_prepared(&wt, &self.registry, org, repo, p, opts, |p, ex| {
                let prep = p.v2.take();
                Self::ingest_prepped(
                    &wt,
                    org,
                    repo,
                    &p.path,
                    &p.language,
                    ex,
                    FileMeta::of(p),
                    prep.map(|b| *b),
                )
            })?;
            // Only stored (not skipped or rejected) files count to the chunk.
            let stored = matches!(&r, Ok(s) if !s.unchanged);
            out.push(r);
            // Chunked commit: bound the size of one write transaction.
            if !stored {
                continue;
            }
            in_txn += len;
            if in_txn >= chunk_bytes {
                wt.commit()?;
                commits += 1;
                wt = self.db.begin_write()?;
                // This chunk is not (yet) known to be the batch's last, so
                // re-stamp the marker in the new transaction; if it turns out
                // to be the last, the clear below overwrites it in that same
                // transaction before it ever commits.
                if !skip_marker {
                    Self::mark_open_batch(&wt, batch_id, org, repo)?;
                }
                in_txn = 0;
            }
        }
        // The batch completed: clear the marker in this final transaction,
        // atomically with (or, if the last data chunk just committed above,
        // immediately after) the last chunk's data.
        if !skip_marker {
            Self::clear_open_batch(&wt)?;
        }
        if let Some((m, _)) = &marker {
            self.before_marked_commit(m)?;
        }
        wt.commit()?;
        Ok((out, commits + 1))
    }

    /// Allocate the next monotonic batch id from `meta.next_batch_id`,
    /// mirroring the existing `next_id`/`next_term` counters' read-then-bump
    /// shape. A monotonic counter (rather than a random id) is simpler to
    /// assert on in tests.
    fn next_batch_id(wt: &redb::WriteTransaction) -> Result<u64> {
        let mut m = wt.open_table(META)?;
        let id = m.get("next_batch_id")?.map_or(0, |v| v.value());
        m.insert("next_batch_id", id + 1)?;
        Ok(id)
    }

    /// Stamp `wt` with the open-batch marker (`OPEN_BATCH` org/repo plus
    /// `meta.open_batch_id`). Called at the start of `index_batch` and again
    /// in every subsequent chunk transaction, so a reader mid-batch (slice
    /// 3o) always finds it in whichever transaction it observes.
    fn mark_open_batch(
        wt: &redb::WriteTransaction,
        batch_id: u64,
        org: &str,
        repo: &str,
    ) -> Result<()> {
        wt.open_table(META)?.insert("open_batch_id", batch_id)?;
        let mut ob = wt.open_table(OPEN_BATCH)?;
        ob.insert("org", org)?;
        ob.insert("repo", repo)?;
        Ok(())
    }

    /// Clear the open-batch marker. Called in the transaction that commits
    /// the batch's final chunk.
    fn clear_open_batch(wt: &redb::WriteTransaction) -> Result<()> {
        wt.open_table(META)?.remove("open_batch_id")?;
        let mut ob = wt.open_table(OPEN_BATCH)?;
        ob.remove("org")?;
        ob.remove("repo")?;
        Ok(())
    }

    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        let wt = self.db.begin_write()?;
        let removed = Self::prune_in(&wt, org, repo, keep, dry_run)?;
        if dry_run {
            wt.abort()?;
        } else {
            wt.commit()?;
        }
        Ok(removed)
    }

    /// The body of [`prune_files`](Self::prune_files) inside the caller's
    /// transaction (which the caller aborts for a dry run).
    fn prune_in(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        // Stored paths are normalized; so is the keep set, or a `\` keep set
        // (a Windows caller) would match nothing and prune everything.
        let keep: HashSet<String> = keep.iter().map(|k| normalize_path(k)).collect();
        let mut removed = Vec::new();
        {
            let mut w = W::new(wt)?;
            let mut tally = Tally::default();
            let org_id = w
                .names
                .get(name_key(None, NodeKind::Org, org).as_str())?
                .map(|v| v.value());
            let repo_id = match org_id {
                Some(o) => w
                    .names
                    .get(name_key(Some(o), NodeKind::Repo, repo).as_str())?
                    .map(|v| v.value()),
                None => None,
            };
            if let Some(repo_id) = repo_id {
                let files: Vec<NodeId> = w
                    .children
                    .get(repo_id)?
                    .map(|v| v.map(|g| g.value()))
                    .collect::<std::result::Result<_, _>>()?;
                for fid in files {
                    let f = dec(w
                        .nodes
                        .get(fid)?
                        .ok_or_else(|| StoreError::Corrupt("dangling file".into()))?
                        .value())?;
                    if keep.contains(&f.name) || f.origin.as_deref() != Some(ORIGIN_DIRECTORY) {
                        continue;
                    }
                    if dry_run {
                        removed.push(f.name);
                        continue;
                    }
                    let scope = Scope {
                        org,
                        repo,
                        lang: f.language.as_deref().unwrap_or("unknown"),
                        encoding: f.encoding.as_deref(),
                        lossy: f.lossy,
                    };
                    w.remove_content(fid, &scope, &mut tally)?;
                    tally.file(&scope, -1);
                    w.nodes.remove(fid)?;
                    w.names
                        .remove(name_key(Some(repo_id), NodeKind::File, &f.name).as_str())?;
                    w.children.remove(repo_id, fid)?;
                    removed.push(f.name);
                }
            }
            tally.apply(&mut w.cat)?;
        }
        removed.sort();
        Ok(removed)
    }

    /// Write an extraction whose spans `validate_spans` accepted.
    fn ingest_validated(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        meta: FileMeta<'_>,
    ) -> Result<IngestStats> {
        Self::ingest_prepped(wt, org, repo, path, language, ex, meta, None)
    }

    /// Write an extraction whose spans `validate_spans` accepted, using the
    /// `V2Prep` built while preparing (built here when there is none).
    #[allow(clippy::too_many_arguments)]
    fn ingest_prepped(
        wt: &redb::WriteTransaction,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        meta: FileMeta<'_>,
        prep: Option<V2Prep>,
    ) -> Result<IngestStats> {
        if org.is_empty() || repo.is_empty() {
            return Err(StoreError::Rejected(
                "org and repo must not be empty".into(),
            ));
        }
        let nul = |what: &str, v: &str| {
            if v.contains('\0') {
                Err(StoreError::Rejected(format!(
                    "{what} must not contain NUL bytes"
                )))
            } else {
                Ok(())
            }
        };
        nul("org", org)?;
        nul("repo", repo)?;
        nul("language", language)?;
        for sd in &ex.symbols {
            if let Some(k) = &sd.lang_kind {
                nul("symbol lang_kind", k)?;
            }
            if let Some(o) = &sd.owner {
                nul("symbol owner", o)?;
            }
        }
        let language = language.to_ascii_lowercase();
        let language = language.as_str();
        // Every write path lands here, including a pre-extracted ingest
        // (NDJSON, `IngestExtraction` from a remote client): the stored path
        // is `/`-separated and normalized whatever the caller sent (#100, #120).
        let path = normalize_path(path);
        let path = path.as_str();
        let mut w = W::new(wt)?;
        let mut tally = Tally::default();
        let mut next = w.meta.get("next_id")?.map_or(1, |v| v.value());
        let mut next_term = w.meta.get("next_term")?.map_or(0, |v| v.value());

        let mut ensure = |w: &mut W,
                          parent: Option<NodeId>,
                          kind: NodeKind,
                          name: &str,
                          language: Option<&str>|
         -> Result<(NodeId, bool)> {
            let key = name_key(parent, kind, name);
            if let Some(v) = w.names.get(key.as_str())? {
                return Ok((v.value(), true));
            }
            if next > MAX_LOCAL {
                return Err(StoreError::Storage("entity id space exhausted".into()));
            }
            let id = next;
            next += 1;
            let mut node = blank(id, parent, kind, name.into());
            node.language = language.map(Into::into);
            w.nodes.insert(id, enc(&node).as_slice())?;
            w.names.insert(key.as_str(), id)?;
            if let Some(p) = parent {
                w.children.insert(p, id)?;
            }
            Ok((id, false))
        };
        let (org_id, _) = ensure(&mut w, None, NodeKind::Org, org, None)?;
        let (repo_id, _) = ensure(&mut w, Some(org_id), NodeKind::Repo, repo, None)?;
        let (file_id, existed) =
            ensure(&mut w, Some(repo_id), NodeKind::File, path, Some(language))?;
        w.cat.insert(format!("r\0{org}\0{repo}").as_str(), 0)?;
        let scope = Scope {
            org,
            repo,
            lang: language,
            encoding: meta.encoding,
            lossy: meta.lossy,
        };
        tally.file(&scope, 1);
        if existed {
            let old = dec(w
                .nodes
                .get(file_id)?
                .ok_or_else(|| StoreError::Corrupt("dangling file".into()))?
                .value())?;
            let old_scope = Scope {
                org,
                repo,
                lang: old.language.as_deref().unwrap_or("unknown"),
                encoding: old.encoding.as_deref(),
                lossy: old.lossy,
            };
            tally.file(&old_scope, -1);
            w.remove_content(file_id, &old_scope, &mut tally)?;
        }
        let mut f = blank(file_id, Some(repo_id), NodeKind::File, path.into());
        f.language = Some(language.into());
        f.has_errors = ex.has_errors;
        f.origin = meta.origin.map(Into::into);
        f.fingerprint = meta.fingerprint.map(Into::into);
        f.encoding = meta.encoding.map(Into::into);
        f.lossy = meta.lossy;
        w.nodes.insert(file_id, enc(&f).as_slice())?;

        let V2Prep {
            terms,
            mut stream,
            postings,
            tally_nodes,
        } = prep.unwrap_or_else(|| V2Prep::build(ex));
        // Intern each distinct term once, in first-use order: the ids come out
        // exactly as interning every occurrence in stream order would give.
        let ids = terms
            .iter()
            .map(|t| w.intern(t, &mut next_term))
            .collect::<Result<Vec<u64>>>()?;
        for (idx, s) in stream.symbols.iter_mut().enumerate() {
            w.sym_idx.insert(
                terms[s.name as usize].as_str(),
                sub_id(TAG_SYM, file_id, idx),
            )?;
            s.name = ids[s.name as usize];
            s.lang_kind = s.lang_kind.map(|k| ids[k as usize]);
            s.owner = s.owner.map(|k| ids[k as usize]);
        }
        for t in &mut stream.tokens {
            t.term = ids[t.term as usize];
        }
        // Inserted in term-id order, so the file is byte-for-byte
        // reproducible (whatever `--jobs` is).
        let mut postings: Vec<(u64, Vec<u8>)> = postings
            .into_iter()
            .map(|(local, bytes)| (ids[local as usize], bytes))
            .collect();
        postings.sort_by_key(|p| p.0);
        for (term, bytes) in postings {
            w.post.insert((term, file_id), bytes.as_slice())?;
        }
        w.streams
            .insert(file_id, codec::encode(&stream).as_slice())?;
        // Install the refcount/content_files seam (ADR 0003 story 3, Q2): the
        // count is always 1 today because `content_id` is the identity while
        // content sharing (story 18) is off, but every write goes through
        // these two tables now so enabling fan-out later needs no format
        // change here.
        let cid = content_id(file_id);
        w.refs.insert(cid, 1)?;
        w.content_files.insert(cid, file_id)?;
        for (n, count) in &tally_nodes {
            tally.node(&scope, n, *count);
        }
        w.meta.insert("next_id", next)?;
        w.meta.insert("next_term", next_term)?;
        tally.apply(&mut w.cat)?;
        Ok(IngestStats {
            file_id,
            symbols: stream.symbols.len(),
            tokens: stream.tokens.len(),
            replaced: existed,
            unchanged: false,
            has_errors: ex.has_errors,
            path: path.to_string(),
            language: language.to_string(),
            encoding: meta.encoding.map(Into::into),
            lossy: meta.lossy,
            span_warning: None,
        })
    }
}

macro_rules! store_read {
    ($ty:ty, |$s:ident| $rt:expr) => {
        impl StoreRead for $ty {
            fn get(&self, id: NodeId) -> Result<Option<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.get(id)
            }
            fn parent(&self, id: NodeId) -> Result<Option<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.parent(id)
            }
            fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.count_nodes(kind)
            }
            fn roots(&self) -> Result<Vec<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.roots()
            }
            fn children(&self, id: NodeId) -> Result<Vec<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.children(id)
            }
            fn descendants(&self, id: NodeId) -> Result<Vec<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.descendants(id)
            }
            fn ancestors(&self, id: NodeId) -> Result<Vec<Node>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.ancestors(id)
            }
            fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.file_tokens(org, repo, path)
            }
            fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                describe_in(&g, org, repo)
            }
            fn describe_by_scan(
                &self,
                org: Option<&str>,
                repo: Option<&str>,
            ) -> Result<Vec<RepoInfo>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.describe_by_scan(org, repo)
            }
            fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.search_symbols(q)
            }
            fn search(&self, q: &Query) -> Result<Vec<Hit>> {
                let $s = self;
                $s.check_not_expired()?;
                let g = $rt;
                R::new(&g)?.search(q)
            }
        }
    };
}

store_read!(V2Store, |s| {
    read_stats::bump(Counter::ReadTxns);
    s.db.begin_read()?
});
store_read!(V2Snapshot, |s| &s.rt);

impl V2Store {
    /// [`Store::snapshot`] as an owned handle with no borrow of the store
    /// (ADR 0004 D1: a server keeps snapshot handles in a table for its
    /// clients, outliving any one request). A `V2Snapshot` holds its own
    /// read transaction and a shared tracker, so it needs no borrow; the
    /// trait method only ties the lifetime for object safety. Same max age,
    /// same accounting in [`Store::snapshot_stats`].
    pub fn snapshot_owned(&self) -> Result<V2Snapshot> {
        let rt = self.db.begin_read()?;
        let created_at = Instant::now();
        let tracker_id = {
            let mut t = self
                .snapshot_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = t.next_id;
            t.next_id += 1;
            t.open.insert(id, created_at);
            id
        };
        Ok(V2Snapshot {
            rt,
            tracker: Arc::clone(&self.snapshot_tracker),
            tracker_id,
            created_at,
            max_age: self.max_snapshot_age,
            warned: Cell::new(false),
        })
    }

    /// The file this store was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configured snapshot max age ([`set_max_snapshot_age`](Self::set_max_snapshot_age)).
    pub fn max_snapshot_age(&self) -> Duration {
        self.max_snapshot_age
    }
}

impl Store for V2Store {
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>> {
        let rt = self.db.begin_read()?;
        let created_at = Instant::now();
        let tracker_id = {
            let mut t = self
                .snapshot_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = t.next_id;
            t.next_id += 1;
            t.open.insert(id, created_at);
            id
        };
        Ok(Box::new(V2Snapshot {
            rt,
            tracker: Arc::clone(&self.snapshot_tracker),
            tracker_id,
            created_at,
            max_age: self.max_snapshot_age,
            warned: Cell::new(false),
        }))
    }

    fn snapshot_stats(&self) -> SnapshotStats {
        let t = self
            .snapshot_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SnapshotStats {
            open_count: t.open.len(),
            oldest_age: t.open.values().min().map(|created| created.elapsed()),
            store_size_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        }
    }
    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats> {
        V2Store::index_bytes_opts(self, org, repo, path, bytes, language, origin, opts)
    }
    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        V2Store::ingest_file_with_origin(self, org, repo, path, language, ex, origin)
    }
    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        V2Store::index_batch(self, org, repo, files, opts)
    }
    fn prepare(
        &self,
        org: &str,
        repo: &str,
        file: &BatchFile<'_>,
        opts: IndexOptions,
    ) -> Result<PreparedFile> {
        V2Store::prepare(self, org, repo, file, opts)
    }
    fn fingerprint_snapshot(&self, org: &str, repo: &str) -> Result<crate::FingerprintSnapshot> {
        V2Store::fingerprint_snapshot(self, org, repo)
    }
    fn prepare_with(
        &self,
        org: &str,
        repo: &str,
        file: &BatchFile<'_>,
        opts: IndexOptions,
        known: &crate::FingerprintSnapshot,
    ) -> Result<PreparedFile> {
        V2Store::prepare_with(self, org, repo, file, opts, known)
    }
    fn index_prepared(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        V2Store::index_prepared(self, org, repo, files, opts)
    }
    fn index_prepared_counted(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<(Vec<Result<IngestStats>>, u64)> {
        V2Store::index_prepared_counted(self, org, repo, files, opts)
    }
    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        V2Store::prune_files(self, org, repo, keep, dry_run)
    }
    fn vacuum(&self) -> Result<VacuumStats> {
        V2Store::vacuum(self)
    }
    fn extractor_gaps(
        &self,
        org: Option<&str>,
        repo: Option<&str>,
    ) -> Result<Vec<crate::ExtractorGap>> {
        crate::common::extractor_gaps_in(&self.db.begin_read()?, &self.registry, org, repo)
    }
    fn space_usage(&self) -> Result<Option<crate::SpaceUsage>> {
        // Pages freed by a commit are released only by later commits: two
        // empty ones make `allocated_pages` exact (as `compact` does).
        for _ in 0..2 {
            self.db.begin_write()?.commit()?;
        }
        let wt = self.db.begin_write()?;
        let s = wt.stats()?;
        wt.abort()?;
        Ok(Some(crate::SpaceUsage {
            file_bytes: std::fs::metadata(&self.path).map_or(0, |m| m.len()),
            live_bytes: s.allocated_pages() * s.page_size() as u64,
        }))
    }
}

/// What a File node records besides its extraction: how the ingest was
/// requested, the fingerprint (absent for a pre-extracted ingest) and the
/// source encoding (ADR 0007 C6; `None` for UTF-8).
#[derive(Clone, Copy)]
pub(crate) struct FileMeta<'a> {
    pub(crate) origin: Option<&'a str>,
    pub(crate) fingerprint: Option<&'a str>,
    pub(crate) encoding: Option<&'static str>,
    pub(crate) lossy: bool,
}

impl<'a> FileMeta<'a> {
    /// A caller-supplied extraction: no source, so no fingerprint and no
    /// encoding.
    pub(crate) fn extracted(origin: Option<&'a str>) -> Self {
        Self {
            origin,
            fingerprint: None,
            encoding: None,
            lossy: false,
        }
    }

    /// A file decoded and fingerprinted by `prepare_file`.
    pub(crate) fn of(p: &'a PreparedFile) -> Self {
        Self {
            origin: p.origin.as_deref(),
            fingerprint: Some(&p.fingerprint),
            encoding: p.encoding.map(|e| e.name()),
            lossy: p.lossy,
        }
    }
}

/// The v2 per-file work that needs no database: done by `Store::prepare` on
/// the parse threads, so the single writer only interns terms and inserts.
pub(crate) struct V2Prep {
    /// The file's distinct terms (token texts, symbol names and kinds), in
    /// first-use order along the stream.
    terms: Vec<String>,
    /// The stream with `terms` indexes where term ids go.
    stream: Stream,
    /// Encoded posting list (token ordinals) per term index.
    postings: Vec<(u64, Vec<u8>)>,
    /// Catalog deltas: one sample node per symbol kind / token class, and
    /// how many of them the file has.
    tally_nodes: Vec<(Node, i64)>,
}

impl V2Prep {
    /// About how many bytes of heap this takes.
    pub(crate) fn footprint(&self) -> usize {
        use std::mem::size_of;
        size_of::<Self>()
            + self.terms.capacity() * size_of::<String>()
            + self.terms.iter().map(String::capacity).sum::<usize>()
            + self.stream.symbols.capacity() * size_of::<SymRec>()
            + self.stream.tokens.capacity() * size_of::<TokRec>()
            + self.postings.capacity() * size_of::<(u64, Vec<u8>)>()
            + self
                .postings
                .iter()
                .map(|(_, v)| v.capacity())
                .sum::<usize>()
            + self.tally_nodes.capacity() * size_of::<(Node, i64)>()
    }
}

/// The file-local number of `t`, numbering new terms in first-use order.
fn intern_local<'a>(t: &'a str, terms: &mut Vec<String>, local: &mut HashMap<&'a str, u64>) -> u64 {
    *local.entry(t).or_insert_with(|| {
        terms.push(t.to_string());
        terms.len() as u64 - 1
    })
}

impl V2Prep {
    /// Walk symbols and tokens in span order (symbols first on ties, the
    /// outermost first), nesting each under the innermost open symbol.
    pub(crate) fn build(ex: &Extraction) -> Self {
        let mut syms: Vec<_> = ex.symbols.iter().collect();
        syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
        let mut toks: Vec<_> = ex.tokens.iter().collect();
        toks.sort_by_key(|t| t.span.start);

        // File-local term numbers, in first-use order.
        let mut terms: Vec<String> = Vec::new();
        let mut local: HashMap<&str, u64> = HashMap::new();
        let mut stream = Stream::default();
        let mut open: Vec<(u32, u32)> = Vec::new();
        let (mut si, mut ti) = (0, 0);
        // Keyed by name (the kinds are not `Ord`), with the kind kept.
        type SymKey = (&'static str, Option<String>);
        let mut sym_counts: BTreeMap<SymKey, (SymbolKind, i64)> = BTreeMap::new();
        let mut class_counts: BTreeMap<&'static str, (TokenClass, i64)> = BTreeMap::new();
        while si < syms.len() || ti < toks.len() {
            let take_sym =
                si < syms.len() && (ti >= toks.len() || syms[si].span.start <= toks[ti].span.start);
            let pos = if take_sym {
                syms[si].span.start
            } else {
                toks[ti].span.start
            };
            while open.last().is_some_and(|&(_, end)| end <= pos) {
                open.pop();
            }
            let parent = open.last().map(|&(i, _)| i);
            if take_sym {
                let s = syms[si];
                si += 1;
                let name = intern_local(&s.name, &mut terms, &mut local);
                let lang_kind = s
                    .lang_kind
                    .as_deref()
                    .map(|k| intern_local(k, &mut terms, &mut local));
                let owner = s
                    .owner
                    .as_deref()
                    .map(|o| intern_local(o, &mut terms, &mut local));
                sym_counts
                    .entry((s.kind.as_str(), s.lang_kind.clone()))
                    .or_insert((s.kind, 0))
                    .1 += 1;
                let idx = stream.symbols.len();
                stream.symbols.push(SymRec {
                    owner,
                    name,
                    kind: s.kind,
                    lang_kind,
                    parent,
                    span: s.span,
                    // `codec::encode` derives the real transitive range from
                    // `stream.tokens`' parent chains; this value is ignored.
                    toks: None,
                });
                open.push((idx as u32, s.span.end));
            } else {
                let t = toks[ti];
                ti += 1;
                class_counts
                    .entry(t.class.as_str())
                    .or_insert((t.class, 0))
                    .1 += 1;
                stream.tokens.push(TokRec {
                    term: intern_local(&t.text, &mut terms, &mut local),
                    class: t.class,
                    parent,
                    span: t.span,
                });
            }
        }
        let mut ords: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (i, t) in stream.tokens.iter().enumerate() {
            ords.entry(t.term).or_default().push(i);
        }
        let postings = ords
            .into_iter()
            .map(|(t, o)| (t, codec::encode_posting(&o)))
            .collect();
        let mut tally_nodes = Vec::new();
        for ((_, lang_kind), (kind, n)) in sym_counts {
            let mut node = blank(0, None, NodeKind::Symbol, String::new());
            node.symbol_kind = Some(kind);
            node.lang_kind = lang_kind;
            tally_nodes.push((node, n));
        }
        for (_, (class, n)) in class_counts {
            let mut node = blank(0, None, NodeKind::Token, String::new());
            node.token_class = Some(class);
            tally_nodes.push((node, n));
        }
        Self {
            terms,
            stream,
            postings,
            tally_nodes,
        }
    }
}
