//! Shared, backend-independent pieces of the store: the entity tables every
//! layout keeps (`meta`, `nodes`, `names`, `children`, `symbols_by_name`,
//! `catalog`), node encoding, fingerprints, the pure half of indexing one
//! file (`prepare_file`) and its committing half (`commit_prepared`), span
//! validation and the catalog bookkeeping behind `describe`. Nothing here
//! knows how tokens are stored: that is `v2.rs`.
use crate::api::{Prepared, PreparedFile};
use crate::{BatchFile, IndexOptions, IngestStats, RepoInfo, StoreError};
use encoding_rs as encoding;
use graph_core::encoding::{Encoding, DECODER_VERSION};
use graph_core::{normalize_path, Extraction, Node, NodeId, NodeKind, Registry, SymbolKind};
use redb::{
    DatabaseError, MultimapTableDefinition, ReadTransaction, ReadableTable, TableDefinition,
};
use std::collections::BTreeMap;
use std::path::Path;

type Result<T> = std::result::Result<T, StoreError>;

/// Version of the file fingerprint scheme (what goes into `Node::fingerprint`).
/// Bump it to force every file to re-index once.
pub const FINGERPRINT_FORMAT_VERSION: u64 = 1;
/// Spans are `u32` byte offsets.
pub const MAX_SOURCE_BYTES: usize = u32::MAX as usize;

pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Entity rows (org, repo, file) as JSON `Node`s.
pub(crate) const NODES: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");
/// `parent\0kind\0name` -> node id, for idempotent org/repo/file lookup.
pub(crate) const NAMES: TableDefinition<&str, u64> = TableDefinition::new("names");
pub(crate) const CHILDREN: MultimapTableDefinition<u64, u64> =
    MultimapTableDefinition::new("children");
/// Symbol name -> symbol node ids (exact and prefix lookup).
pub(crate) const SYMBOLS: MultimapTableDefinition<&str, u64> =
    MultimapTableDefinition::new("symbols_by_name");

/// Derived counters behind `describe`, so it never decodes nodes. Keys (fields
/// separated by NUL): `r org repo` (repo exists), `f|s|t org repo lang` (files,
/// symbols, tokens), `k org repo lang label` (symbols per kind label),
/// `c org repo class` (tokens per class). Zero counts are absent.
pub(crate) const CATALOG: TableDefinition<&str, u64> = TableDefinition::new("catalog");

/// Map an open failure; the read-only hint is given only for permission errors,
/// and never for a directory: Windows reports opening a directory as a file
/// as `PermissionDenied` (os error 5), where Linux says `EISDIR` (#100), so a
/// directory gets its own hint instead of a misleading "must be writable".
pub(crate) fn open_failed(path: &Path, e: &DatabaseError) -> StoreError {
    if path.is_dir() {
        return StoreError::OpenFailed {
            path: path.display().to_string(),
            reason: format!("{e} (the path is a directory, not a database file)"),
        };
    }
    let denied = matches!(
        e,
        DatabaseError::Storage(redb::StorageError::Io(io))
            if matches!(
                io.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            )
    );
    let hint = if denied {
        " (the database file and its directory must be writable; read-only databases are not supported)"
    } else {
        ""
    };
    StoreError::OpenFailed {
        path: path.display().to_string(),
        reason: format!("{e}{hint}"),
    }
}

/// Check that an extraction's symbols and tokens nest properly (no span starts
/// after it ends, none partially overlaps an enclosing symbol). The only span
/// validation: every write path runs it before its first write.
pub(crate) fn validate_spans(ex: &Extraction) -> Result<()> {
    let mut syms: Vec<_> = ex.symbols.iter().map(|s| s.span).collect();
    syms.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut toks: Vec<_> = ex.tokens.iter().map(|t| t.span).collect();
    toks.sort_by_key(|t| t.start);
    let (mut si, mut ti) = (0, 0);
    let mut open: Vec<u32> = Vec::new();
    while si < syms.len() || ti < toks.len() {
        let take_sym = si < syms.len() && (ti >= toks.len() || syms[si].start <= toks[ti].start);
        let sp = if take_sym { syms[si] } else { toks[ti] };
        if take_sym {
            si += 1;
        } else {
            ti += 1;
        }
        while open.last().is_some_and(|&end| end <= sp.start) {
            open.pop();
        }
        if sp.start > sp.end {
            return Err(StoreError::InvalidSpan(format!(
                "start {} > end {}",
                sp.start, sp.end
            )));
        }
        if let Some(&end) = open.last() {
            if sp.end > end {
                return Err(StoreError::InvalidSpan(format!(
                    "bytes {}..{} partially overlap an enclosing symbol ending at {end}",
                    sp.start, sp.end
                )));
            }
        }
        if take_sym {
            open.push(sp.end);
        }
    }
    Ok(())
}

/// Fingerprint of `bytes` indexed as `lang` with `registry`'s extractor,
/// decoded from `encoding` (ADR 0007 C7). A clean UTF-8 decode adds nothing,
/// so a UTF-8 file's fingerprint is what it always was; any other decode
/// appends `|enc=<WHATWG name>[+lossy]@<DECODER_VERSION>`. The hash is of the
/// raw bytes either way. Pure, so it can run on any thread.
pub(crate) fn fingerprint(
    registry: &Registry,
    bytes: &[u8],
    lang: &str,
    encoding: &'static Encoding,
    lossy: bool,
) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut fp = format!(
        "sha256:{hash}|{}|{}|{FINGERPRINT_FORMAT_VERSION}",
        lang.to_ascii_lowercase(),
        registry.version(lang)
    );
    if encoding != encoding::UTF_8 || lossy {
        fp.push_str(&format!(
            "|enc={}{}@{DECODER_VERSION}",
            encoding.name(),
            if lossy { "+lossy" } else { "" }
        ));
    }
    fp
}

/// Whether the file stored under org/repo/path (as of `rt`) carries `fp`.
pub(crate) fn stored_fingerprint_matches(
    rt: &ReadTransaction,
    org: &str,
    repo: &str,
    path: &str,
    fp: &str,
) -> Result<bool> {
    fn open<T>(r: std::result::Result<T, redb::TableError>) -> Result<Option<T>> {
        match r {
            Ok(t) => Ok(Some(t)),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    let (Some(names), Some(nodes)) = (open(rt.open_table(NAMES))?, open(rt.open_table(NODES))?)
    else {
        return Ok(false);
    };
    let find = |parent: Option<NodeId>, kind: NodeKind, name: &str| -> Result<Option<NodeId>> {
        Ok(names
            .get(name_key(parent, kind, name).as_str())?
            .map(|v| v.value()))
    };
    let Some(org_id) = find(None, NodeKind::Org, org)? else {
        return Ok(false);
    };
    let Some(repo_id) = find(Some(org_id), NodeKind::Repo, repo)? else {
        return Ok(false);
    };
    let Some(file_id) = find(Some(repo_id), NodeKind::File, path)? else {
        return Ok(false);
    };
    let Some(raw) = nodes.get(file_id)? else {
        return Ok(false);
    };
    Ok(dec(raw.value())?.fingerprint.as_deref() == Some(fp))
}

/// If the file already stored under org/repo/path carries `fp`, leave its
/// nodes alone (only refreshing `origin` if it differs) and return its
/// stats plus whether anything was written.
pub(crate) fn check_unchanged(
    wt: &redb::WriteTransaction,
    org: &str,
    repo: &str,
    path: &str,
    lang: &str,
    fp: &str,
    origin: Option<&str>,
) -> Result<Option<(IngestStats, bool)>> {
    let names = wt.open_table(NAMES)?;
    let mut nodes = wt.open_table(NODES)?;
    let find = |parent: Option<NodeId>, kind: NodeKind, name: &str| -> Result<Option<NodeId>> {
        Ok(names
            .get(name_key(parent, kind, name).as_str())?
            .map(|v| v.value()))
    };
    let Some(org_id) = find(None, NodeKind::Org, org)? else {
        return Ok(None);
    };
    let Some(repo_id) = find(Some(org_id), NodeKind::Repo, repo)? else {
        return Ok(None);
    };
    let Some(file_id) = find(Some(repo_id), NodeKind::File, path)? else {
        return Ok(None);
    };
    let Some(raw) = nodes.get(file_id)? else {
        return Ok(None);
    };
    let mut f = dec(raw.value())?;
    drop(raw);
    if f.fingerprint.as_deref() != Some(fp) {
        return Ok(None);
    }
    let dirty = f.origin.as_deref() != origin;
    let stats = IngestStats {
        file_id,
        unchanged: true,
        has_errors: f.has_errors,
        path: path.to_string(),
        language: lang.to_string(),
        encoding: f.encoding.clone(),
        lossy: f.lossy,
        ..IngestStats::default()
    };
    if dirty {
        f.origin = origin.map(Into::into);
        nodes.insert(file_id, enc(&f).as_slice())?;
    }
    Ok(Some((stats, dirty)))
}

/// The pure half of indexing one file (see [`crate::Store::prepare`]): size
/// and binary checks, decoding (ADR 0007 C2: the one decode site of every
/// write path), path normalization, language detection on the decoded text,
/// fingerprint,
/// then extraction and span validation, unless `unchanged(path, lang, fp)`
/// says the stored copy already carries this fingerprint (asked only without
/// `reindex`). Per-file rejections land in the result; only an error from
/// `unchanged` (storage) is returned as `Err`.
pub(crate) fn prepare_file(
    registry: &Registry,
    org: &str,
    repo: &str,
    f: &BatchFile<'_>,
    opts: IndexOptions,
    unchanged: impl FnOnce(&str, &str, &str) -> Result<bool>,
) -> Result<PreparedFile> {
    let path = normalize_path(f.path);
    let mut p = PreparedFile {
        org: org.into(),
        repo: repo.into(),
        path,
        language: String::new(),
        fingerprint: String::new(),
        bytes_len: f.bytes.len(),
        origin: f.origin.map(Into::into),
        work: Prepared::Rejected(StoreError::Rejected(String::new())),
        v2: None,
        encoding: None,
        lossy: false,
        hint: None,
        strict_encoding: false,
    };
    if f.bytes.len() > MAX_SOURCE_BYTES {
        p.work = Prepared::Rejected(StoreError::TooLarge(format!("`{}`", f.path)));
        return Ok(p);
    }
    let hint = f.encoding.or(opts.encoding);
    let strict = f.strict_encoding || opts.strict_encoding;
    if graph_core::encoding::is_binary_with_hint(f.bytes, hint) {
        p.work = Prepared::Rejected(StoreError::Binary(format!("`{}`", f.path)));
        return Ok(p);
    }
    let decoded = graph_core::encoding::decode(f.bytes, hint);
    if strict && decoded.lossy {
        p.work = Prepared::Rejected(strict_refusal(f.path, decoded.encoding));
        return Ok(p);
    }
    let src: &str = &decoded.text;
    p.language = f.language.map_or_else(
        || registry.detect_language(&p.path, src),
        str::to_ascii_lowercase,
    );
    p.fingerprint = fingerprint(
        registry,
        f.bytes,
        &p.language,
        decoded.encoding,
        decoded.lossy,
    );
    p.encoding = (decoded.encoding != encoding::UTF_8).then_some(decoded.encoding);
    p.lossy = decoded.lossy;
    p.work = if !opts.reindex && unchanged(&p.path, &p.language, &p.fingerprint)? {
        // Keep the source: if the file changed by commit time, the commit
        // extracts it after all (as `index_batch` would have).
        Prepared::Unchanged(src.to_owned())
    } else {
        extract_checked(registry, &p.path, &p.language, src)
    };
    Ok(p)
}

/// The per-file refusal of a lossy decode under `strict_encoding` (ADR 0007
/// C3, C8): today's `NotUtf8` when the file was decoded as UTF-8 (a `utf-8`
/// hint or a UTF-8 BOM), so `--encoding utf-8 --strict-encoding` reproduces
/// the old refusal exactly; a typed `StrictEncoding` naming the encoding
/// otherwise (#180).
fn strict_refusal(path: &str, encoding: &'static Encoding) -> StoreError {
    if encoding == encoding::UTF_8 {
        StoreError::NotUtf8(format!("`{path}`"))
    } else {
        StoreError::StrictEncoding {
            path: path.to_owned(),
            encoding: encoding.name().to_owned(),
        }
    }
}

/// Extract and validate spans, so a bad file only fails itself.
pub(crate) fn extract_checked(registry: &Registry, path: &str, lang: &str, src: &str) -> Prepared {
    let ex = registry.extract(lang, src);
    match validate_spans(&ex) {
        Err(StoreError::InvalidSpan(why)) => {
            Prepared::Rejected(StoreError::InvalidSpan(format!("`{path}`: {why}")))
        }
        _ => Prepared::Extracted(ex),
    }
}

/// The committing half, inside the caller's write transaction: re-run the
/// unchanged check authoritatively (the prepare-time one only saved an
/// extraction), then store the extraction through `ingest`, extracting now
/// if a file prepared as unchanged has changed since (e.g. the same path
/// earlier in the batch). `Ok(Err(_))` is a per-file outcome; `Err` is a
/// storage error, or a file prepared for another org/repo, and aborts the
/// transaction.
pub(crate) fn commit_prepared(
    wt: &redb::WriteTransaction,
    registry: &Registry,
    org: &str,
    repo: &str,
    mut p: PreparedFile,
    opts: IndexOptions,
    ingest: impl FnOnce(&mut PreparedFile, &Extraction) -> Result<IngestStats>,
) -> Result<Result<IngestStats>> {
    if p.org != org || p.repo != repo {
        return Err(StoreError::Rejected(format!(
            "`{}` was prepared for {}/{}, not {org}/{repo}",
            p.path, p.org, p.repo
        )));
    }
    let work = std::mem::replace(
        &mut p.work,
        Prepared::Rejected(StoreError::Rejected(String::new())),
    );
    if let Prepared::Rejected(e) = work {
        return Ok(Err(e));
    }
    if let Prepared::Remote(_) = work {
        // ADR 0004 D2: the bytes were kept for a server to parse; this
        // store has neither fingerprint nor extraction for them.
        return Ok(Err(StoreError::Rejected(REMOTE_PREPARED_REJECTION.into())));
    }
    if !opts.reindex {
        if let Some((stats, _)) = check_unchanged(
            wt,
            org,
            repo,
            &p.path,
            &p.language,
            &p.fingerprint,
            p.origin.as_deref(),
        )? {
            return Ok(Ok(stats));
        }
    }
    let work = match work {
        Prepared::Unchanged(src) => extract_checked(registry, &p.path, &p.language, &src),
        w => w,
    };
    match work {
        Prepared::Extracted(ex) => Ok(Ok(ingest(&mut p, &ex)?)),
        Prepared::Rejected(e) => Ok(Err(e)),
        Prepared::Unchanged(_) => unreachable!("extracted above"),
        Prepared::Remote(_) => unreachable!("rejected above"),
    }
}

/// The per-file rejection an embedded store gives a
/// [`PreparedFile::remote`] file (ADR 0004 D2).
pub const REMOTE_PREPARED_REJECTION: &str =
    "remote-prepared file cannot be committed to an embedded store";

/// A symbol matches a kind name if it is its generic kind or its
/// language-specific kind string (ASCII case-insensitive, like `--language`).
/// Language-specific kinds that contain methods without being a generic
/// `type`: a Rust `impl` block (generic `other`). The one language-specific
/// name the store knows; the alternative (the Rust extractor calling an impl
/// a type) would change `symbols --kind` results and force a re-index.
const CLASS_CONTAINER_LANG_KINDS: &[&str] = &["impl"];

/// Whether `sym` can be a row of `grain`: any symbol for `Symbol`, a callable
/// (`method` or `function`) for `Method`, a `type` or a class container for
/// `Class`. Never true for the token, file, repo and org grains.
pub(crate) fn grain_accepts(grain: crate::Grain, sym: &Node) -> bool {
    match grain {
        crate::Grain::Symbol => true,
        crate::Grain::Method => matches!(
            sym.symbol_kind,
            Some(SymbolKind::Method | SymbolKind::Function)
        ),
        crate::Grain::Class => {
            sym.symbol_kind == Some(SymbolKind::Type)
                || CLASS_CONTAINER_LANG_KINDS
                    .iter()
                    .any(|k| kind_matches(sym, k))
        }
        _ => false,
    }
}

pub(crate) fn kind_matches(sym: &Node, kind: &str) -> bool {
    sym.symbol_kind
        .unwrap_or(SymbolKind::Other)
        .as_str()
        .eq_ignore_ascii_case(kind)
        || sym
            .lang_kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case(kind))
}

pub(crate) fn enc(n: &Node) -> Vec<u8> {
    serde_json::to_vec(n).expect("node serializes")
}

pub(crate) fn dec(b: &[u8]) -> Result<Node> {
    serde_json::from_slice(b).map_err(|e| StoreError::Corrupt(e.to_string()))
}

pub(crate) fn name_key(parent: Option<NodeId>, kind: NodeKind, name: &str) -> String {
    format!("{}\0{:?}\0{}", parent.unwrap_or(0), kind, name)
}

/// `generic` or `generic/language-specific` label of a symbol node.
pub(crate) fn kind_label(n: &Node) -> String {
    let generic = n.symbol_kind.unwrap_or(SymbolKind::Other).as_str();
    match &n.lang_kind {
        Some(k) if k != generic => format!("{generic}/{k}"),
        _ => generic.to_string(),
    }
}

/// Where a file lives, for catalog bookkeeping.
pub(crate) struct Scope<'a> {
    pub(crate) org: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) lang: &'a str,
    /// The file's non-UTF-8 encoding (ADR 0007), `None` for UTF-8.
    pub(crate) encoding: Option<&'a str>,
    /// The file was decoded lossily.
    pub(crate) lossy: bool,
}

/// Catalog deltas accumulated during one write transaction and applied to the
/// table just before commit (so an aborted transaction changes nothing).
#[derive(Default)]
pub(crate) struct Tally(BTreeMap<String, i64>);

impl Tally {
    pub(crate) fn add(&mut self, key: String, d: i64) {
        *self.0.entry(key).or_default() += d;
    }
    pub(crate) fn file(&mut self, s: &Scope, d: i64) {
        self.add(format!("f\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
        self.encoding(s, d);
    }
    /// The encoding entries of a file (see [`Self::file`]).
    pub(crate) fn encoding(&mut self, s: &Scope, d: i64) {
        // ADR 0007 C6: non-UTF-8 files per encoding (UTF-8 is the
        // remainder) and lossy files; absent means zero.
        if let Some(e) = s.encoding {
            self.add(format!("e\0{}\0{}\0{e}", s.org, s.repo), d);
        }
        if s.lossy {
            self.add(format!("l\0{}\0{}", s.org, s.repo), d);
        }
    }
    /// A symbol or token node appearing (`d` = 1) or disappearing (`d` = -1).
    pub(crate) fn node(&mut self, s: &Scope, n: &Node, d: i64) {
        match n.kind {
            NodeKind::Symbol => {
                self.add(format!("s\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
                let label = kind_label(n);
                self.add(format!("k\0{}\0{}\0{}\0{label}", s.org, s.repo, s.lang), d);
            }
            NodeKind::Token => {
                self.add(format!("t\0{}\0{}\0{}", s.org, s.repo, s.lang), d);
                if let Some(c) = n.token_class {
                    self.add(format!("c\0{}\0{}\0{}", s.org, s.repo, c.as_str()), d);
                }
            }
            _ => {}
        }
    }
    pub(crate) fn apply(self, cat: &mut redb::Table<&str, u64>) -> Result<()> {
        for (k, d) in self.0 {
            if d == 0 {
                continue;
            }
            let cur = cat.get(k.as_str())?.map_or(0, |v| v.value()) as i64;
            let new = cur + d;
            if new > 0 {
                cat.insert(k.as_str(), new as u64)?;
            } else {
                cat.remove(k.as_str())?;
            }
        }
        Ok(())
    }
}

/// `describe` from the catalog (O(repos), never decodes a node), inside the
/// caller's read transaction, optionally scoped to an org and/or repo. The
/// `open_batch` marker (ADR 0003 story 3, decision D3) is read inside this
/// same transaction so the flag is snapshot-consistent with the counts.
pub(crate) fn describe_in(
    rt: &ReadTransaction,
    org: Option<&str>,
    repo: Option<&str>,
) -> Result<Vec<RepoInfo>> {
    let cat = rt.open_table(CATALOG)?;
    let mut infos: BTreeMap<(String, String), RepoInfo> = BTreeMap::new();
    let corrupt = || StoreError::Corrupt("bad catalog key".into());
    let mut seen_repo = std::collections::HashSet::new();
    for r in cat.iter()? {
        let (k, v) = r?;
        let (k, v) = (k.value(), v.value() as usize);
        let f: Vec<&str> = k.split('\0').collect();
        if f.len() < 3 {
            return Err(corrupt());
        }
        if org.is_some_and(|o| f[1] != o) || repo.is_some_and(|r| f[2] != r) {
            continue;
        }
        if f[0] == "r" {
            seen_repo.insert((f[1].to_string(), f[2].to_string()));
        }
        let info = infos
            .entry((f[1].to_string(), f[2].to_string()))
            .or_insert_with(|| RepoInfo {
                org: f[1].to_string(),
                repo: f[2].to_string(),
                files: 0,
                languages: BTreeMap::new(),
                token_classes: BTreeMap::new(),
                open_batch: false,
                encodings: BTreeMap::new(),
                lossy: 0,
            });
        match (f[0], f.len()) {
            ("r", 3) => {}
            ("e", 4) => {
                info.encodings.insert(f[3].to_string(), v);
            }
            ("l", 3) => info.lossy = v,
            ("c", 4) => {
                info.token_classes.insert(f[3].to_string(), v);
            }
            ("f", 4) => {
                info.files += v;
                info.languages.entry(f[3].to_string()).or_default().files = v;
            }
            ("s", 4) => info.languages.entry(f[3].to_string()).or_default().symbols = v,
            ("t", 4) => info.languages.entry(f[3].to_string()).or_default().tokens = v,
            ("k", 5) => {
                info.languages
                    .entry(f[3].to_string())
                    .or_default()
                    .symbol_kinds
                    .insert(f[4].to_string(), v);
            }
            _ => return Err(corrupt()),
        }
    }
    if infos.keys().any(|k| !seen_repo.contains(k)) {
        return Err(StoreError::Corrupt(
            "catalog has counters for a repo without its marker row".into(),
        ));
    }
    let open_batch: Option<(String, String)> = match rt.open_table(crate::v2::OPEN_BATCH) {
        Ok(t) => {
            let org = t.get("org")?.map(|v| v.value().to_string());
            let repo = t.get("repo")?.map(|v| v.value().to_string());
            org.zip(repo)
        }
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(key) = &open_batch {
        if let Some(info) = infos.get_mut(key) {
            info.open_batch = true;
        }
    }
    Ok(infos.into_values().collect())
}

/// The extractor version inside a fingerprint of a file stored as `lang`
/// (`sha256:<hex>|<lang>|<version>|<format>`, see [`fingerprint`]), if it
/// has that shape. Anchored on the known language and format version, so
/// a `|` inside the language or the version cannot shift the fields.
pub(crate) fn fingerprint_extractor_version<'a>(fp: &'a str, lang: &str) -> Option<&'a str> {
    let fp = strip_encoding_suffix(fp);
    let rest = fp.strip_prefix("sha256:")?;
    // The hash is hex: its end is the first `|`.
    let (_, rest) = rest.split_once('|')?;
    let rest = rest.strip_prefix(lang.to_ascii_lowercase().as_str())?;
    let rest = rest.strip_prefix('|')?;
    let suffix = format!("|{FINGERPRINT_FORMAT_VERSION}");
    rest.strip_suffix(suffix.as_str())
}

/// `fp` without its optional `|enc=<name>[+lossy]@<decoder version>` suffix
/// (ADR 0007 C7). Only a well-formed suffix is removed: a WHATWG name (no
/// `|`), an optional `+lossy`, `@` and a decimal version.
fn strip_encoding_suffix(fp: &str) -> &str {
    let Some(at) = fp.rfind("|enc=") else {
        return fp;
    };
    let tail = &fp[at + "|enc=".len()..];
    let Some((name, version)) = tail.rsplit_once('@') else {
        return fp;
    };
    let name = name.strip_suffix("+lossy").unwrap_or(name);
    let well_formed = !name.is_empty()
        && !name.contains(['|', '@'])
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit());
    if well_formed {
        &fp[..at]
    } else {
        fp
    }
}

/// [`crate::Store::extractor_gaps`] inside the caller's read transaction.
pub(crate) fn extractor_gaps_in(
    rt: &ReadTransaction,
    registry: &Registry,
    org: Option<&str>,
    repo: Option<&str>,
) -> Result<Vec<crate::ExtractorGap>> {
    let mut gaps = Vec::new();
    for info in describe_in(rt, org, repo)? {
        // Only a registered extractor produces symbols from source; the
        // fallback never does. So a language with symbols stored and no
        // extractor now is the only candidate (O(repos) from the catalog).
        let suspects: Vec<(&String, usize)> = info
            .languages
            .iter()
            .filter(|(l, li)| li.symbols > 0 && !registry.has(l))
            .map(|(l, li)| (l, li.symbols))
            .collect();
        if suspects.is_empty() {
            continue;
        }
        let names = rt.open_table(NAMES)?;
        let nodes = rt.open_table(NODES)?;
        let children = rt.open_multimap_table(CHILDREN)?;
        let find = |parent: Option<NodeId>, kind: NodeKind, name: &str| -> Result<Option<NodeId>> {
            Ok(names
                .get(name_key(parent, kind, name).as_str())?
                .map(|v| v.value()))
        };
        let Some(repo_id) = find(None, NodeKind::Org, &info.org)?
            .map(|o| find(Some(o), NodeKind::Repo, &info.repo))
            .transpose()?
            .flatten()
        else {
            continue;
        };
        // One pass over the repo's files, stopping once every suspect has a
        // fingerprinted file (pre-extracted ingests carry none and never
        // count: their symbols did not come from a registered extractor).
        let mut found: BTreeMap<&str, String> = BTreeMap::new();
        for c in children.get(repo_id)? {
            let Some(raw) = nodes.get(c?.value())? else {
                continue;
            };
            let n = dec(raw.value())?;
            let (Some(lang), Some(fp)) = (n.language.as_deref(), n.fingerprint.as_deref()) else {
                continue;
            };
            let Some(&(l, _)) = suspects.iter().find(|(l, _)| l.as_str() == lang) else {
                continue;
            };
            if found.contains_key(l.as_str()) {
                continue;
            }
            // Not redundant with the `!registry.has` filter above: that one
            // picks candidate languages from the catalog; this skips files
            // of the language indexed tokens-only (the fallback's version,
            // which is what `registry.version` returns without an
            // extractor), e.g. next to pre-extracted symbols.
            if let Some(v) = fingerprint_extractor_version(fp, lang) {
                if v != registry.version(l) {
                    found.insert(l.as_str(), v.to_string());
                }
            }
            if found.len() == suspects.len() {
                break;
            }
        }
        for (l, symbols) in suspects {
            if let Some(v) = found.remove(l.as_str()) {
                gaps.push(crate::ExtractorGap {
                    org: info.org.clone(),
                    repo: info.repo.clone(),
                    language: l.clone(),
                    stored_version: v,
                    symbols,
                });
            }
        }
    }
    Ok(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::{Extractor, Span, SymbolDecl, TokenDecl};

    fn file_node(encoding: Option<&str>, lossy: bool) -> Node {
        Node {
            id: 3,
            parent: Some(2),
            kind: NodeKind::File,
            name: "src/a.cs".into(),
            language: Some("csharp".into()),
            symbol_kind: None,
            lang_kind: None,
            token_class: None,
            has_errors: false,
            origin: Some("directory".into()),
            fingerprint: Some("sha256:ab|csharp|1|1".into()),
            encoding: encoding.map(Into::into),
            lossy,
            span: None,
        }
    }

    /// ADR 0007 C6 golden bytes: a UTF-8 File node serializes exactly as it
    /// did before schema 11 (no `encoding`, no `lossy`), and the new fields
    /// appear, in a fixed place, only for a non-UTF-8 or lossy decode.
    #[test]
    fn file_node_golden_bytes() {
        let utf8 = br#"{"id":3,"parent":2,"kind":"file","name":"src/a.cs","language":"csharp","symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"origin":"directory","fingerprint":"sha256:ab|csharp|1|1","span":null}"#;
        assert_eq!(enc(&file_node(None, false)), utf8.to_vec());
        let le = br#"{"id":3,"parent":2,"kind":"file","name":"src/a.cs","language":"csharp","symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"origin":"directory","fingerprint":"sha256:ab|csharp|1|1","encoding":"UTF-16LE","span":null}"#;
        assert_eq!(enc(&file_node(Some("UTF-16LE"), false)), le.to_vec());
        let lossy = br#"{"id":3,"parent":2,"kind":"file","name":"src/a.cs","language":"csharp","symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"origin":"directory","fingerprint":"sha256:ab|csharp|1|1","encoding":"windows-1252","lossy":true,"span":null}"#;
        assert_eq!(enc(&file_node(Some("windows-1252"), true)), lossy.to_vec());
        // Each reads back as itself; a v10 record (no fields) reads as UTF-8.
        for (bytes, want) in [
            (&utf8[..], file_node(None, false)),
            (&le[..], file_node(Some("UTF-16LE"), false)),
            (&lossy[..], file_node(Some("windows-1252"), true)),
        ] {
            assert_eq!(dec(bytes).unwrap(), want);
        }
    }

    /// An encoding name `encoding_rs` does not know (or a label that is not
    /// the canonical name, or `replacement`) in a stored File node is a
    /// corrupt record, never a panic.
    #[test]
    fn unknown_stored_encoding_is_corrupt() {
        for name in ["EBCDIC-9000", "latin1", "utf-16le", "replacement", ""] {
            let json = String::from_utf8(enc(&file_node(None, false)))
                .unwrap()
                .replace(
                    r#""span":null"#,
                    &format!(r#""encoding":"{name}","span":null"#),
                );
            assert!(
                matches!(dec(json.as_bytes()), Err(StoreError::Corrupt(m)) if m.contains("encoding")),
                "{name}"
            );
        }
        let json = String::from_utf8(enc(&file_node(None, false)))
            .unwrap()
            .replace(r#""span":null"#, r#""encoding":7,"span":null"#);
        assert!(matches!(dec(json.as_bytes()), Err(StoreError::Corrupt(_))));
    }

    /// ADR 0007 C7: a clean UTF-8 decode leaves the fingerprint exactly as
    /// before (no forced re-index); anything else gets the suffix.
    #[test]
    fn utf8_fingerprint_is_unchanged() {
        let r = Registry::default();
        let v = r.version("text");
        assert_eq!(
            fingerprint(&r, b"x", "text", encoding::UTF_8, false),
            format!(
                "sha256:2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881|text|{v}|{FINGERPRINT_FORMAT_VERSION}"
            )
        );
        assert_eq!(
            fingerprint(&r, b"x", "text", encoding::UTF_16BE, false),
            format!(
                "sha256:2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881|text|{v}|{FINGERPRINT_FORMAT_VERSION}|enc=UTF-16BE@{DECODER_VERSION}"
            )
        );
        assert!(fingerprint(&r, b"x", "text", encoding::WINDOWS_1252, true)
            .ends_with(&format!("|enc=windows-1252+lossy@{DECODER_VERSION}")));
    }
    #[test]
    fn fingerprint_extractor_version_parses_the_fingerprint_shape() {
        let r = Registry::default();
        let fp = fingerprint(&r, b"x", "Toy", encoding::UTF_8, false);
        assert_eq!(
            fingerprint_extractor_version(&fp, "toy"),
            Some(r.version("toy").as_str())
        );
        // The encoding suffix (ADR 0007 C7) is tolerated, lossy or not.
        for (e, lossy) in [
            (encoding::UTF_16LE, false),
            (encoding::SHIFT_JIS, true),
            (encoding::UTF_8, true),
        ] {
            let fp = fingerprint(&r, b"x", "Toy", e, lossy);
            assert!(fp.contains("|enc="), "{fp}");
            assert_eq!(
                fingerprint_extractor_version(&fp, "toy"),
                Some(r.version("toy").as_str()),
                "{fp}"
            );
        }
        let f = FINGERPRINT_FORMAT_VERSION;
        assert_eq!(
            fingerprint_extractor_version(&format!("sha256:ab|toy|v|{f}|enc=UTF-16BE@1"), "toy"),
            Some("v")
        );
        // A malformed suffix is not stripped, so the shape does not match.
        assert_eq!(
            fingerprint_extractor_version(&format!("sha256:ab|toy|v|{f}|enc=x@y"), "toy"),
            None
        );
        // A `|` in the language or the version does not shift the fields.
        let fp = fingerprint(&r, b"x", "a|b", encoding::UTF_8, false);
        assert_eq!(
            fingerprint_extractor_version(&fp, "a|b"),
            Some(r.version("a|b").as_str())
        );
        let f = FINGERPRINT_FORMAT_VERSION;
        assert_eq!(
            fingerprint_extractor_version(&format!("sha256:ab|a|b|v|2|{f}"), "a|b"),
            Some("v|2")
        );
        // Wrong language, wrong format version, or not a fingerprint.
        assert_eq!(fingerprint_extractor_version(&fp, "c"), None);
        assert_eq!(
            fingerprint_extractor_version("sha256:ab|toy|v|999", "toy"),
            None
        );
        assert_eq!(fingerprint_extractor_version("nope", "toy"), None);
    }

    fn sp(s: u32, e: u32) -> Span {
        Span {
            start: s,
            end: e,
            start_line: 1,
            start_col: s + 1,
            end_line: 1,
            end_col: e + 1,
        }
    }

    fn sym(name: &str, s: u32, e: u32) -> SymbolDecl {
        SymbolDecl {
            owner: None,
            name: name.into(),
            kind: SymbolKind::Type,
            lang_kind: None,
            span: sp(s, e),
        }
    }

    fn tok(t: &str, s: u32, e: u32) -> TokenDecl {
        TokenDecl {
            text: t.into(),
            class: graph_core::TokenClass::Identifier,
            span: sp(s, e),
        }
    }

    #[test]
    fn validate_spans_accepts_nesting_and_rejects_partial_overlap_and_inversion() {
        let ok = Extraction {
            has_errors: false,
            symbols: vec![sym("A", 0, 10), sym("B", 2, 6), sym("Z", 4, 4)],
            tokens: vec![tok("a", 0, 1), tok("b", 3, 4), tok("c", 8, 10)],
        };
        validate_spans(&ok).unwrap();
        let partial = Extraction {
            has_errors: false,
            symbols: vec![sym("A", 0, 9), sym("B", 5, 14)],
            tokens: vec![],
        };
        assert!(matches!(
            validate_spans(&partial),
            Err(StoreError::InvalidSpan(m)) if m.contains("partially overlap")
        ));
        let token_escapes = Extraction {
            has_errors: false,
            symbols: vec![sym("A", 0, 4)],
            tokens: vec![tok("t", 2, 6)],
        };
        assert!(matches!(
            validate_spans(&token_escapes),
            Err(StoreError::InvalidSpan(_))
        ));
        let inverted = Extraction {
            has_errors: false,
            symbols: vec![sym("A", 3, 0)],
            tokens: vec![],
        };
        assert!(matches!(
            validate_spans(&inverted),
            Err(StoreError::InvalidSpan(m)) if m.contains("start 3 > end 0")
        ));
    }

    #[test]
    fn nodes_without_origin_or_fingerprint_fields_still_deserialize() {
        let old = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"span":null}"#;
        assert_eq!(dec(old).unwrap().origin, None);
        let older = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"span":null}"#;
        assert_eq!(dec(older).unwrap().origin, None);
        let no_fp = br#"{"id":1,"parent":null,"kind":"file","name":"a","language":null,"symbol_kind":null,"lang_kind":null,"token_class":null,"has_errors":false,"origin":"directory","span":null}"#;
        assert_eq!(dec(no_fp).unwrap().fingerprint, None);
        assert!(matches!(dec(b"{not json"), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn open_failed_hint_only_for_permission_errors() {
        let path = std::path::Path::new("x.redb");
        let io = |k: std::io::ErrorKind| {
            DatabaseError::Storage(redb::StorageError::Io(std::io::Error::from(k)))
        };
        let denied = open_failed(path, &io(std::io::ErrorKind::PermissionDenied)).to_string();
        assert!(denied.contains("cannot open database") && denied.contains("must be writable"));
        let ro = open_failed(path, &io(std::io::ErrorKind::ReadOnlyFilesystem)).to_string();
        assert!(ro.contains("must be writable"), "{ro}");
        let other = open_failed(path, &io(std::io::ErrorKind::NotFound)).to_string();
        assert!(!other.contains("writable"), "{other}");
    }

    struct Versioned(&'static str);
    impl Extractor for Versioned {
        fn language(&self) -> &str {
            "vx"
        }
        fn version(&self) -> String {
            self.0.into()
        }
        fn extract(&self, _: &str) -> Extraction {
            Extraction {
                has_errors: false,
                symbols: vec![],
                tokens: vec![],
            }
        }
    }

    #[test]
    fn fallback_and_rust_versions_are_distinct() {
        let mut r = Registry::default();
        r.register(Box::new(Versioned("v")));
        assert_eq!(r.version("vx"), "v");
        assert!(r
            .version("zig")
            .starts_with(graph_core::FALLBACK_EXTRACTOR_VERSION));
        assert_ne!(graph_lang_rust::RustExtractor.version(), r.version("zig"));
        assert_ne!(graph_lang_rust::RustExtractor.version(), "1");
    }

    #[test]
    fn extractor_and_tokenizer_versions_participate_in_fingerprint() {
        let bare = Registry::default();
        let mut rust = Registry::default();
        rust.register(Box::new(graph_lang_rust::RustExtractor));
        let fp_fallback = fingerprint(&bare, b"fn f() {}\n", "rust", encoding::UTF_8, false);
        let fp_rust = fingerprint(&rust, b"fn f() {}\n", "rust", encoding::UTF_8, false);
        assert_ne!(fp_fallback, fp_rust);
        assert!(fp_rust.contains(&graph_lang_rust::RustExtractor.version()));
        assert!(fp_fallback.contains(graph_core::FALLBACK_EXTRACTOR_VERSION));
        let tok = format!("tok{}", graph_core::tokenizer::TOKENIZER_VERSION);
        assert!(fp_rust.contains(&tok) && fp_fallback.contains(&tok));
        // Content, language (case-folded) and the scheme version all count.
        assert_ne!(
            fp_rust,
            fingerprint(&rust, b"fn g() {}\n", "rust", encoding::UTF_8, false)
        );
        assert_eq!(
            fp_rust,
            fingerprint(&rust, b"fn f() {}\n", "RUST", encoding::UTF_8, false)
        );
        assert!(fp_rust.ends_with(&format!("|{FINGERPRINT_FORMAT_VERSION}")));
    }

    #[test]
    fn kind_label_and_kind_matches() {
        let mut n = Node {
            id: 1,
            parent: None,
            kind: NodeKind::Symbol,
            name: "S".into(),
            language: None,
            symbol_kind: Some(SymbolKind::Type),
            lang_kind: Some("struct".into()),
            token_class: None,
            has_errors: false,
            origin: None,
            fingerprint: None,
            encoding: None,
            lossy: false,
            span: None,
        };
        assert_eq!(kind_label(&n), "type/struct");
        assert!(kind_matches(&n, "TYPE") && kind_matches(&n, "Struct"));
        assert!(!kind_matches(&n, "method"));
        n.lang_kind = Some("type".into());
        assert_eq!(kind_label(&n), "type");
        n.symbol_kind = None;
        n.lang_kind = None;
        assert_eq!(kind_label(&n), "other");
        assert!(kind_matches(&n, "other"));
    }

    #[test]
    fn name_key_separates_fields_with_nul() {
        assert_eq!(name_key(None, NodeKind::Org, "o"), "0\0Org\0o");
        assert_eq!(
            name_key(Some(7), NodeKind::File, "a/b.rs"),
            "7\0File\0a/b.rs"
        );
    }
}
