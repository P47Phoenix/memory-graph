//! Storage format v2 on redb (ADR 0003 stories 2-4, first slice): an interned
//! dictionary, one compact stream per file (tokens and symbols, see
//! [`crate::codec`]) and count postings `(term, file) -> count`. Tokens are not
//! rows. Orgs, repos and files are entity rows (the same JSON `Node` as v1);
//! symbols and tokens are addressed by ids that encode `(file, index)`.
//!
//! This backend sits behind [`Store`] as `Backend::RedbV2` and is checked
//! against v1 by the conformance suite and `run_differential`. It is a first
//! slice, not the final layout: see the ADR story notes for what remains
//! (packed dictionary, block postings, sparse checkpoints, `--limit`
//! push-down, versioned components, sharding).
//!
//! Ids: `tag(2) | file_local(30) | index(32)`. Tag 0 is an entity (org, repo,
//! file; `file_local` is the allocation counter and `index` is 0), tag 1 a
//! symbol (`index` = position in the file's symbol section) and tag 2 a token
//! (`index` = ordinal). Ids of symbols and tokens are stable only until the
//! file is re-indexed (ADR Q1).
//!
//! The on-disk file is stamped `schema_version` 3, so v1 builds refuse it and
//! this backend refuses a v1 file, in both cases before writing anything.
use crate::codec::{self, Stream, SymRec, TokRec, STREAM_FORMAT};
use crate::{dec, enc, RedbStore, Store, StoreRead};
use crate::{
    kind_label, kind_matches, name_key, open_failed, validate_spans, BatchFile, Grain, Hit,
    IndexOptions, IngestStats, Query, RepoInfo, Scope, StoreError, SymbolHit, SymbolQuery, Tally,
    CATALOG, CATALOG_VERSION, CHILDREN, FINGERPRINT_FORMAT_VERSION, MAX_SOURCE_BYTES, META, NAMES,
    NODES, ORIGIN_DIRECTORY, SYMBOLS,
};
use graph_core::{
    detect_language_from_content, normalize_path, Extraction, Extractor, Node, NodeId, NodeKind,
    Registry, SymbolKind,
};
use redb::{
    Database, DatabaseError, ReadTransaction, ReadableMultimapTable, ReadableTable, TableDefinition,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

type Result<T> = std::result::Result<T, StoreError>;

/// Layout version of a v2 file (v1 is 1 and 2).
pub const V2_SCHEMA_VERSION: u64 = 3;

/// term text -> term id.
const DICT: TableDefinition<&str, u64> = TableDefinition::new("dict");
/// term id -> term text.
const DICT_REV: TableDefinition<u64, &str> = TableDefinition::new("dict_rev");
/// file id -> encoded stream.
const STREAMS: TableDefinition<u64, &[u8]> = TableDefinition::new("stream");
/// (term id, file id) -> number of occurrences of the term in the file.
const POST: TableDefinition<(u64, u64), u64> = TableDefinition::new("post");

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
        span: None,
    }
}

/// The v2 backend: one redb file.
pub struct V2Store {
    db: Database,
    registry: Registry,
}

pub struct V2Snapshot {
    rt: ReadTransaction,
}

/// Read side over one read transaction.
struct R {
    nodes: redb::ReadOnlyTable<u64, &'static [u8]>,
    names: redb::ReadOnlyTable<&'static str, u64>,
    streams: redb::ReadOnlyTable<u64, &'static [u8]>,
    dict: redb::ReadOnlyTable<&'static str, u64>,
    rev: redb::ReadOnlyTable<u64, &'static str>,
    post: redb::ReadOnlyTable<(u64, u64), u64>,
    sym_idx: redb::ReadOnlyMultimapTable<&'static str, u64>,
    cat: redb::ReadOnlyTable<&'static str, u64>,
}

/// One file with its containment path and decoded stream.
struct FileCtx {
    org: Node,
    repo: Node,
    file: Node,
}

impl R {
    fn new(rt: &ReadTransaction) -> Result<Self> {
        Ok(Self {
            nodes: rt.open_table(NODES)?,
            names: rt.open_table(NAMES)?,
            streams: rt.open_table(STREAMS)?,
            dict: rt.open_table(DICT)?,
            rev: rt.open_table(DICT_REV)?,
            post: rt.open_table(POST)?,
            sym_idx: rt.open_multimap_table(SYMBOLS)?,
            cat: rt.open_table(CATALOG)?,
        })
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

    fn text(&self, term: u64) -> Result<String> {
        Ok(self
            .rev
            .get(term)?
            .ok_or_else(|| StoreError::Corrupt(format!("dangling term {term}")))?
            .value()
            .to_string())
    }

    fn sym_node(&self, file: u64, i: usize, s: &Stream) -> Result<Node> {
        let r = &s.symbols[i];
        let parent = r.parent.map_or(file, |p| sub_id(TAG_SYM, file, p as usize));
        let mut n = blank(
            sub_id(TAG_SYM, file, i),
            Some(parent),
            NodeKind::Symbol,
            self.text(r.name)?,
        );
        n.symbol_kind = Some(r.kind);
        n.lang_kind = r.lang_kind.map(|k| self.text(k)).transpose()?;
        n.span = Some(r.span);
        Ok(n)
    }

    fn tok_node(&self, file: u64, i: usize, s: &Stream) -> Result<Node> {
        let r = &s.tokens[i];
        let parent = r.parent.map_or(file, |p| sub_id(TAG_SYM, file, p as usize));
        let mut n = blank(
            sub_id(TAG_TOK, file, i),
            Some(parent),
            NodeKind::Token,
            self.text(r.term)?,
        );
        n.token_class = Some(r.class);
        n.span = Some(r.span);
        Ok(n)
    }

    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        let (tag, file, i) = split_id(id);
        if tag == 0 {
            return self.node(id);
        }
        let Some(s) = self.stream(file)? else {
            return Ok(None);
        };
        if tag == TAG_SYM && i < s.symbols.len() {
            Ok(Some(self.sym_node(file, i, &s)?))
        } else if tag == TAG_TOK && i < s.tokens.len() {
            Ok(Some(self.tok_node(file, i, &s)?))
        } else {
            Ok(None)
        }
    }

    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        match self.get(id)?.and_then(|n| n.parent) {
            Some(p) => self.get(p),
            None => Ok(None),
        }
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

    fn ctx(&self, file_id: u64, cache: &mut HashMap<u64, FileCtx>) -> Result<()> {
        if cache.contains_key(&file_id) {
            return Ok(());
        }
        let file = self.need(file_id)?;
        let repo = self.need(
            file.parent
                .ok_or_else(|| StoreError::Corrupt("file without repo".into()))?,
        )?;
        let org = self.need(
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
        for i in 0..s.tokens.len() {
            out.push(self.tok_node(f, i, &s)?);
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
                    let l = info.languages.entry(lang).or_default();
                    l.files += 1;
                    let s = self
                        .stream(n.id)?
                        .ok_or_else(|| StoreError::Corrupt(format!("file {} no stream", n.id)))?;
                    for i in 0..s.symbols.len() {
                        l.symbols += 1;
                        *l.symbol_kinds
                            .entry(kind_label(&self.sym_node(n.id, i, &s)?))
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
        let mut streams: HashMap<u64, Option<Stream>> = HashMap::new();
        let mut files: HashMap<u64, FileCtx> = HashMap::new();
        let mut out: Vec<(u64, SymbolHit)> = Vec::new();
        for id in ids {
            let (tag, file, i) = split_id(id);
            if tag != TAG_SYM {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = streams.entry(file) {
                e.insert(self.stream(file)?);
            }
            // A dangling index entry (stale index) is skipped, not an error.
            let Some(s) = streams[&file].as_ref().filter(|s| i < s.symbols.len()) else {
                continue;
            };
            let sym = self.sym_node(file, i, s)?;
            if q.kind.as_deref().is_some_and(|k| !kind_matches(&sym, k)) {
                continue;
            }
            self.ctx(file, &mut files)?;
            let c = &files[&file];
            if want_lang
                .as_ref()
                .is_some_and(|l| c.file.language.as_ref() != Some(l))
                || q.org.as_ref().is_some_and(|o| &c.org.name != o)
                || q.repo.as_ref().is_some_and(|r| &c.repo.name != r)
                || want_file.as_ref().is_some_and(|f| &c.file.name != f)
            {
                continue;
            }
            let mut quals = vec![sym.name.clone()];
            let mut cur = s.symbols[i].parent;
            while let Some(p) = cur {
                // In range: `codec::decode` checks a parent is an earlier symbol.
                let r = &s.symbols[p as usize];
                quals.push(self.text(r.name)?);
                cur = r.parent;
            }
            quals.reverse();
            out.push((
                id,
                SymbolHit {
                    org: c.org.name.clone(),
                    repo: c.repo.name.clone(),
                    file: c.file.name.clone(),
                    language: c.file.language.clone(),
                    name: sym.name,
                    qualified: quals.join("::"),
                    kind: sym.symbol_kind.unwrap_or(SymbolKind::Other),
                    lang_kind: sym.lang_kind,
                    span: sym.span,
                },
            ));
        }
        out.sort_by(|(ia, a), (ib, b)| {
            (
                &a.org,
                &a.repo,
                &a.file,
                a.span.map(|s| s.start),
                &a.qualified,
                ia,
            )
                .cmp(&(
                    &b.org,
                    &b.repo,
                    &b.file,
                    b.span.map(|s| s.start),
                    &b.qualified,
                    ib,
                ))
        });
        let mut out: Vec<SymbolHit> = out.into_iter().map(|(_, h)| h).collect();
        if let Some(n) = q.limit {
            out.truncate(n);
        }
        Ok(out)
    }

    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        let Some(term) = self.dict.get(q.text.as_str())?.map(|v| v.value()) else {
            return Ok(Vec::new());
        };
        // Candidate files come from the postings; token, symbol and class
        // reads decode the stream, file/repo/org roll-ups without a class
        // filter use the posting counts alone.
        let mut cands: Vec<(u64, u64)> = Vec::new();
        for r in self.post.range((term, 0)..=(term, u64::MAX))? {
            let (k, v) = r?;
            cands.push((k.value().1, v.value()));
        }
        let counts_only =
            q.class.is_none() && matches!(q.grain, Grain::File | Grain::Repo | Grain::Org);
        let want_lang = q.language.as_deref().map(str::to_ascii_lowercase);
        let mut files: HashMap<u64, FileCtx> = HashMap::new();
        type Key = (String, String, String, u32, u64);
        let mut rows: BTreeMap<Key, Hit> = BTreeMap::new();
        for (fid, n_post) in cands {
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
                }
                Grain::Org => {
                    hit.repo = None;
                    hit.file = None;
                    hit.language = None;
                }
                _ => {}
            };
            if counts_only {
                let mut hit = base_hit(n_post as usize);
                roll(&mut hit);
                rows.entry(file_key(q.grain))
                    .and_modify(|h| h.count += n_post as usize)
                    .or_insert(hit);
                continue;
            }
            let s = self
                .stream(fid)?
                .ok_or_else(|| StoreError::Corrupt(format!("file {fid} without stream")))?;
            for (ord, t) in s.tokens.iter().enumerate() {
                if t.term != term || q.class.is_some_and(|c| c != t.class) {
                    continue;
                }
                // Enclosing symbols, innermost first.
                let mut chain: Vec<usize> = Vec::new();
                let mut cur = t.parent;
                while let Some(p) = cur {
                    chain.push(p as usize);
                    // In range: `codec::decode` checks a parent is an earlier symbol.
                    cur = s.symbols[p as usize].parent;
                }
                let syms: Vec<Node> = chain
                    .iter()
                    .map(|&i| self.sym_node(fid, i, &s))
                    .collect::<Result<_>>()?;
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
                        hit.symbol = qual(&syms);
                        hit.symbol_kind = syms.first().and_then(|s| s.symbol_kind);
                        hit.lang_kind = syms.first().and_then(|s| s.lang_kind.clone());
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
                    Grain::Symbol => {
                        let pick = syms.iter().position(|s| {
                            q.symbol_kind.as_deref().is_none_or(|k| kind_matches(s, k))
                        });
                        match pick {
                            Some(i) => {
                                let s = &syms[i];
                                hit.symbol = qual(&syms[i..]);
                                hit.symbol_kind = s.symbol_kind;
                                hit.lang_kind = s.lang_kind.clone();
                                hit.span = s.span;
                                key = (
                                    base.0.clone(),
                                    base.1.clone(),
                                    base.2.clone(),
                                    s.span.map_or(0, |x| x.start),
                                    s.id,
                                );
                            }
                            None => {
                                if s.symbols.is_empty() {
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
        let n = q.limit.unwrap_or(usize::MAX);
        Ok(rows.into_values().take(n).collect())
    }
}

/// Write side: every table one ingest touches.
struct W<'t> {
    meta: redb::Table<'t, &'static str, u64>,
    nodes: redb::Table<'t, u64, &'static [u8]>,
    names: redb::Table<'t, &'static str, u64>,
    children: redb::MultimapTable<'t, u64, u64>,
    dict: redb::Table<'t, &'static str, u64>,
    rev: redb::Table<'t, u64, &'static str>,
    streams: redb::Table<'t, u64, &'static [u8]>,
    post: redb::Table<'t, (u64, u64), u64>,
    sym_idx: redb::MultimapTable<'t, &'static str, u64>,
    cat: redb::Table<'t, &'static str, u64>,
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
        })
    }

    fn text(&self, term: u64) -> Result<String> {
        Ok(self
            .rev
            .get(term)?
            .ok_or_else(|| StoreError::Corrupt(format!("dangling term {term}")))?
            .value()
            .to_string())
    }

    fn intern(&mut self, text: &str, next_term: &mut u64) -> Result<u64> {
        if let Some(v) = self.dict.get(text)? {
            return Ok(v.value());
        }
        let id = *next_term;
        *next_term += 1;
        self.dict.insert(text, id)?;
        self.rev.insert(id, text)?;
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

    /// Delete a file's stream, postings and symbol index entries (not its
    /// entity row, name or catalog file count).
    fn remove_content(&mut self, file: u64, scope: &Scope, tally: &mut Tally) -> Result<()> {
        let Some(raw) = self.streams.remove(file)? else {
            return Ok(());
        };
        let s = codec::decode(raw.value())?;
        drop(raw);
        self.tally_stream(tally, scope, file, &s, -1)?;
        let mut terms: HashSet<u64> = HashSet::new();
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

impl V2Store {
    /// Open or create a v2 database file. Refuses (without writing) a file
    /// that is not v2: a v1 file must be re-indexed or migrated (ADR story 12).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path.as_ref()).map_err(|e| match e {
            DatabaseError::DatabaseAlreadyOpen => {
                StoreError::Locked(path.as_ref().display().to_string())
            }
            e => open_failed(path.as_ref(), &e),
        })?;
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
            Some(v) => {
                return Err(StoreError::Rejected(format!(
                    "{} has schema version {v}, not the v2 layout ({V2_SCHEMA_VERSION}); \
                     use the v1 backend, or re-index into a new file; database left unmodified",
                    path.as_ref().display()
                )))
            }
            None => {
                let wt = db.begin_write()?;
                {
                    let mut m = wt.open_table(META)?;
                    m.insert("schema_version", V2_SCHEMA_VERSION)?;
                    m.insert("next_id", 1)?;
                    m.insert("next_term", 0)?;
                    m.insert("stream_format", u64::from(STREAM_FORMAT))?;
                    m.insert("catalog_version", CATALOG_VERSION)?;
                    wt.open_table(CATALOG)?;
                    wt.open_table(NODES)?;
                    wt.open_table(NAMES)?;
                    wt.open_table(DICT)?;
                    wt.open_table(DICT_REV)?;
                    wt.open_table(STREAMS)?;
                    wt.open_table(POST)?;
                    wt.open_multimap_table(CHILDREN)?;
                    wt.open_multimap_table(SYMBOLS)?;
                }
                wt.commit()?;
            }
        }
        Ok(Self {
            db,
            registry: Registry::default(),
        })
    }

    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.registry.register(e);
    }

    fn fingerprint(&self, bytes: &[u8], lang: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!(
            "sha256:{hash}|{}|{}|{FINGERPRINT_FORMAT_VERSION}",
            lang.to_ascii_lowercase(),
            self.registry.version(lang)
        )
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
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(StoreError::TooLarge(format!("`{path}`")));
        }
        let src =
            std::str::from_utf8(bytes).map_err(|_| StoreError::NotUtf8(format!("`{path}`")))?;
        let path = normalize_path(path);
        let lang = language.map_or_else(
            || detect_language_from_content(&path, src),
            str::to_ascii_lowercase,
        );
        let fp = self.fingerprint(bytes, &lang);
        let wt = self.db.begin_write()?;
        if !opts.reindex {
            if let Some((stats, dirty)) =
                RedbStore::check_unchanged(&wt, org, repo, &path, &lang, &fp, origin)?
            {
                if dirty {
                    wt.commit()?;
                }
                return Ok(stats);
            }
        }
        let ex = self.registry.extract(&lang, src);
        validate_spans(&ex)?;
        let stats = Self::ingest_validated(&wt, org, repo, &path, &lang, &ex, (origin, Some(&fp)))?;
        wt.commit()?;
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
        let stats = Self::ingest_validated(&wt, org, repo, path, language, ex, (origin, None))?;
        wt.commit()?;
        Ok(stats)
    }

    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        let wt = self.db.begin_write()?;
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            if f.bytes.len() > MAX_SOURCE_BYTES {
                out.push(Err(StoreError::TooLarge(format!("`{}`", f.path))));
                continue;
            }
            let Ok(src) = std::str::from_utf8(f.bytes) else {
                out.push(Err(StoreError::NotUtf8(format!("`{}`", f.path))));
                continue;
            };
            let path = normalize_path(f.path);
            let lang = f.language.map_or_else(
                || detect_language_from_content(&path, src),
                str::to_ascii_lowercase,
            );
            let fp = self.fingerprint(f.bytes, &lang);
            if !opts.reindex {
                if let Some((stats, _)) =
                    RedbStore::check_unchanged(&wt, org, repo, &path, &lang, &fp, f.origin)?
                {
                    out.push(Ok(stats));
                    continue;
                }
            }
            let ex = self.registry.extract(&lang, src);
            if let Err(StoreError::InvalidSpan(why)) = validate_spans(&ex) {
                out.push(Err(StoreError::InvalidSpan(format!("`{path}`: {why}"))));
                continue;
            }
            out.push(Ok(Self::ingest_validated(
                &wt,
                org,
                repo,
                &path,
                &lang,
                &ex,
                (f.origin, Some(&fp)),
            )?));
        }
        wt.commit()?;
        Ok(out)
    }

    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        let wt = self.db.begin_write()?;
        let mut removed = Vec::new();
        {
            let mut w = W::new(&wt)?;
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
        if dry_run {
            wt.abort()?;
        } else {
            wt.commit()?;
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
        (origin, fingerprint): (Option<&str>, Option<&str>),
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
        }
        let language = language.to_ascii_lowercase();
        let language = language.as_str();
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
            };
            tally.file(&old_scope, -1);
            w.remove_content(file_id, &old_scope, &mut tally)?;
        }
        let mut f = blank(file_id, Some(repo_id), NodeKind::File, path.into());
        f.language = Some(language.into());
        f.has_errors = ex.has_errors;
        f.origin = origin.map(Into::into);
        f.fingerprint = fingerprint.map(Into::into);
        w.nodes.insert(file_id, enc(&f).as_slice())?;

        let mut syms: Vec<_> = ex.symbols.iter().collect();
        syms.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
        let mut toks: Vec<_> = ex.tokens.iter().collect();
        toks.sort_by_key(|t| t.span.start);

        let mut stream = Stream::default();
        // Open-symbol stack: (symbol index, end).
        let mut open: Vec<(u32, u32)> = Vec::new();
        let (mut si, mut ti) = (0, 0);
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
                let name = w.intern(&s.name, &mut next_term)?;
                let lang_kind = match &s.lang_kind {
                    Some(k) => Some(w.intern(k, &mut next_term)?),
                    None => None,
                };
                let idx = stream.symbols.len();
                stream.symbols.push(SymRec {
                    name,
                    kind: s.kind,
                    lang_kind,
                    parent,
                    span: s.span,
                });
                w.sym_idx
                    .insert(s.name.as_str(), sub_id(TAG_SYM, file_id, idx))?;
                open.push((idx as u32, s.span.end));
            } else {
                let t = toks[ti];
                ti += 1;
                let term = w.intern(&t.text, &mut next_term)?;
                stream.tokens.push(TokRec {
                    term,
                    class: t.class,
                    parent,
                    span: t.span,
                });
            }
        }
        let mut counts: HashMap<u64, u64> = HashMap::new();
        for t in &stream.tokens {
            *counts.entry(t.term).or_default() += 1;
        }
        for (term, n) in counts {
            w.post.insert((term, file_id), n)?;
        }
        w.streams
            .insert(file_id, codec::encode(&stream).as_slice())?;
        w.tally_stream(&mut tally, &scope, file_id, &stream, 1)?;
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
        })
    }
}

macro_rules! store_read {
    ($ty:ty, |$s:ident| $rt:expr) => {
        impl StoreRead for $ty {
            fn get(&self, id: NodeId) -> Result<Option<Node>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.get(id)
            }
            fn parent(&self, id: NodeId) -> Result<Option<Node>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.parent(id)
            }
            fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.count_nodes(kind)
            }
            fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.file_tokens(org, repo, path)
            }
            fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
                let $s = self;
                let g = $rt;
                RedbStore::describe_in(&g, org, repo)
            }
            fn describe_by_scan(
                &self,
                org: Option<&str>,
                repo: Option<&str>,
            ) -> Result<Vec<RepoInfo>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.describe_by_scan(org, repo)
            }
            fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.search_symbols(q)
            }
            fn search(&self, q: &Query) -> Result<Vec<Hit>> {
                let $s = self;
                let g = $rt;
                R::new(&g)?.search(q)
            }
        }
    };
}

store_read!(V2Store, |s| s.db.begin_read()?);
store_read!(V2Snapshot, |s| &s.rt);

impl Store for V2Store {
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>> {
        Ok(Box::new(V2Snapshot {
            rt: self.db.begin_read()?,
        }))
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
    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        V2Store::prune_files(self, org, repo, keep, dry_run)
    }
}
