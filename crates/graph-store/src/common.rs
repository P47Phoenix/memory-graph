//! Shared, backend-independent pieces of the store: the entity tables every
//! layout keeps (`meta`, `nodes`, `names`, `children`, `symbols_by_name`,
//! `catalog`), node encoding, fingerprints, the pure half of indexing one
//! file (`prepare_file`) and its committing half (`commit_prepared`), span
//! validation and the catalog bookkeeping behind `describe`. Nothing here
//! knows how tokens are stored: that is `v2.rs`.
use crate::api::{Prepared, PreparedFile};
use crate::{BatchFile, IndexOptions, IngestStats, RepoInfo, StoreError};
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

/// Map an open failure; the read-only hint is given only for permission errors.
pub(crate) fn open_failed(path: &Path, e: &DatabaseError) -> StoreError {
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

/// Fingerprint of `bytes` indexed as `lang` with `registry`'s extractor.
/// Pure, so it can run on any thread.
pub(crate) fn fingerprint(registry: &Registry, bytes: &[u8], lang: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "sha256:{hash}|{}|{}|{FINGERPRINT_FORMAT_VERSION}",
        lang.to_ascii_lowercase(),
        registry.version(lang)
    )
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
        ..IngestStats::default()
    };
    if dirty {
        f.origin = origin.map(Into::into);
        nodes.insert(file_id, enc(&f).as_slice())?;
    }
    Ok(Some((stats, dirty)))
}

/// The pure half of indexing one file (see [`crate::Store::prepare`]): size
/// and UTF-8 checks, path normalization, language detection, fingerprint,
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
    };
    if f.bytes.len() > MAX_SOURCE_BYTES {
        p.work = Prepared::Rejected(StoreError::TooLarge(format!("`{}`", f.path)));
        return Ok(p);
    }
    let Ok(src) = std::str::from_utf8(f.bytes) else {
        p.work = Prepared::Rejected(StoreError::NotUtf8(format!("`{}`", f.path)));
        return Ok(p);
    };
    p.language = f.language.map_or_else(
        || registry.detect_language(&p.path, src),
        str::to_ascii_lowercase,
    );
    p.fingerprint = fingerprint(registry, f.bytes, &p.language);
    p.work = if !opts.reindex && unchanged(&p.path, &p.language, &p.fingerprint)? {
        // Keep the source: if the file changed by commit time, the commit
        // extracts it after all (as `index_batch` would have).
        Prepared::Unchanged(src.to_owned())
    } else {
        extract_checked(registry, &p.path, &p.language, src)
    };
    Ok(p)
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
    }
}

/// A symbol matches a kind name if it is its generic kind or its
/// language-specific kind string (ASCII case-insensitive, like `--language`).
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
            });
        match (f[0], f.len()) {
            ("r", 3) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::{Extractor, Span, SymbolDecl, TokenDecl};

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
        let fp_fallback = fingerprint(&bare, b"fn f() {}\n", "rust");
        let fp_rust = fingerprint(&rust, b"fn f() {}\n", "rust");
        assert_ne!(fp_fallback, fp_rust);
        assert!(fp_rust.contains(&graph_lang_rust::RustExtractor.version()));
        assert!(fp_fallback.contains(graph_core::FALLBACK_EXTRACTOR_VERSION));
        let tok = format!("tok{}", graph_core::tokenizer::TOKENIZER_VERSION);
        assert!(fp_rust.contains(&tok) && fp_fallback.contains(&tok));
        // Content, language (case-folded) and the scheme version all count.
        assert_ne!(fp_rust, fingerprint(&rust, b"fn g() {}\n", "rust"));
        assert_eq!(fp_rust, fingerprint(&rust, b"fn f() {}\n", "RUST"));
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
