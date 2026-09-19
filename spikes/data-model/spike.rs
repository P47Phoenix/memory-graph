//! THROWAWAY spike for the data-model review (not committed). See docs/spikes/data-model.md.
use graph_core::schema::*;
use graph_core::{detect_language_from_content, Extraction, Registry};
use graph_store::{BatchFile, Grain, Query, Store, SymbolQuery};
use redb::{Database, MultimapTableDefinition, ReadableMultimapTable, ReadableTable, TableDefinition, ReadableTableMetadata};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

// ---------- corpus loading ----------
struct Src { repo: String, path: String, lang: String, src: String }
fn walk(d: &std::path::Path, root: &std::path::Path, repo: &str, out: &mut Vec<Src>) {
    let mut es: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().path()).collect();
    es.sort();
    for p in es {
        if p.is_dir() { if p.file_name().unwrap() != ".git" { walk(&p, root, repo, out) } }
        else if let Ok(s) = std::fs::read_to_string(&p) {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let lang = detect_language_from_content(&rel, &s);
            out.push(Src { repo: repo.into(), path: rel, lang, src: s });
        }
    }
}
fn load(dir: &str) -> Vec<Src> {
    let mut out = vec![];
    let mut rs: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    rs.sort();
    for r in rs { walk(&r, &r, r.file_name().unwrap().to_str().unwrap(), &mut out); }
    out
}
fn words(s: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = s.as_bytes(); let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() && !(b[i].is_ascii_alphabetic() || b[i] == b'_') { i += 1 }
        if i >= b.len() { return None }
        let st = i; while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') { i += 1 }
        Some((st, i))
    })
}
/// scaled corpus: copy k>0 renames rare identifiers (corpus freq<=50) with a suffix; exact copies when mutate=false
fn scaled(base: &[Src], copies: usize, mutate: bool) -> Vec<(String, Vec<&Src>, Vec<String>)> { let _ = (base, copies, mutate); vec![] }
fn build_set(base: &[Src], copies: usize, mutate: bool) -> Vec<Src> {
    let mut freq: HashMap<&str, u32> = HashMap::new();
    for f in base { for (a, b) in words(&f.src) { *freq.entry(&f.src[a..b]).or_default() += 1 } }
    let mut out = vec![];
    for k in 0..copies {
        for f in base {
            let src = if k == 0 || !mutate { f.src.clone() } else {
                let mut s = String::with_capacity(f.src.len() + 64); let mut last = 0;
                for (a, b) in words(&f.src) {
                    let w = &f.src[a..b];
                    if w.len() >= 4 && freq[w] <= 50 { s.push_str(&f.src[last..b]); s.push_str(&format!("{k}")); last = b }
                }
                s.push_str(&f.src[last..]); s
            };
            out.push(Src { repo: format!("{}-{k}", f.repo), path: f.path.clone(), lang: f.lang.clone(), src });
        }
    }
    out
}

fn registry() -> Registry { let mut r = Registry::default(); r.register(Box::new(graph_lang_rust::RustExtractor)); r }

// ---------- varint ----------
fn put(v: &mut Vec<u8>, mut x: u64) { while x >= 0x80 { v.push((x as u8) | 0x80); x >>= 7 } v.push(x as u8) }
fn zz(x: i64) -> u64 { ((x << 1) ^ (x >> 63)) as u64 }
fn unzz(x: u64) -> i64 { ((x >> 1) as i64) ^ -((x & 1) as i64) }
struct Cur<'a>(&'a [u8], usize);
impl<'a> Cur<'a> {
    fn get(&mut self) -> u64 { let mut r = 0u64; let mut sh = 0; loop { let b = self.0[self.1]; self.1 += 1; r |= ((b & 0x7f) as u64) << sh; if b < 0x80 { return r } sh += 7 } }
    fn str(&mut self) -> &'a str { let n = self.get() as usize; let s = std::str::from_utf8(&self.0[self.1..self.1 + n]).unwrap(); self.1 += n; s }
    fn done(&self) -> bool { self.1 >= self.0.len() }
}
fn put_str(v: &mut Vec<u8>, s: &str) { put(v, s.len() as u64); v.extend_from_slice(s.as_bytes()) }
fn class_u(c: TokenClass) -> u64 { match c { TokenClass::Identifier => 0, TokenClass::Keyword => 1, TokenClass::Literal => 2, TokenClass::Operator => 3, TokenClass::Punctuation => 4, TokenClass::Comment => 5, TokenClass::Other => 6 } }
fn u_class(u: u64) -> TokenClass { [TokenClass::Identifier, TokenClass::Keyword, TokenClass::Literal, TokenClass::Operator, TokenClass::Punctuation, TokenClass::Comment, TokenClass::Other][u as usize] }
fn sk_u(k: SymbolKind) -> u8 { match k { SymbolKind::Module => 0, SymbolKind::Type => 1, SymbolKind::Function => 2, SymbolKind::Method => 3, SymbolKind::Variable => 4, SymbolKind::Constant => 5, SymbolKind::Other => 6 } }
fn u_sk(u: u8) -> SymbolKind { [SymbolKind::Module, SymbolKind::Type, SymbolKind::Function, SymbolKind::Method, SymbolKind::Variable, SymbolKind::Constant, SymbolKind::Other][u as usize] }

// ---------- token stream codec (model B/C/D) ----------
/// per token: varint(term<<4 | class<<1 | irregular), varint(gap<<1 | newline_before) [, varint(line_delta) if newline, varint(col-1) if newline], irregular => explicit end_line delta, end_col, len, start_col
fn encode_stream(toks: &[&TokenDecl], term_ids: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(toks.len() * 4 + 4);
    put(&mut v, toks.len() as u64);
    let (mut pend, mut pline, mut pcol) = (0u32, 1u32, 1u32);
    for (t, &id) in toks.iter().zip(term_ids) {
        let s = &t.span;
        let gap = s.start - pend;
        let nl = s.start_line > pline;
        let pred_col = if nl { 1 } else { pcol + gap };
        let regular = s.end_line == s.start_line && (s.end - s.start) as usize == t.text.len()
            && s.end_col == s.start_col + t.text.chars().count() as u32
            && (nl || s.start_col == pred_col);
        put(&mut v, ((id as u64) << 4) | (class_u(t.class) << 1) | (!regular) as u64);
        put(&mut v, ((gap as u64) << 1) | nl as u64);
        if nl { put(&mut v, (s.start_line - pline) as u64); put(&mut v, (s.start_col - 1) as u64) }
        if !regular {
            put(&mut v, zz(if nl { 0 } else { s.start_col as i64 - pred_col as i64 })); put(&mut v, (s.end_line - s.start_line) as u64);
            put(&mut v, s.end_col as u64); put(&mut v, (s.end - s.start) as u64);
        }
        pend = s.end; pline = s.end_line; pcol = s.end_col;
    }
    v
}
/// returns (term, class, span)
fn decode_stream(b: &[u8], text_of: &mut dyn FnMut(u32) -> (usize, usize)) -> Vec<(u32, TokenClass, Span)> {
    let mut c = Cur(b, 0); let n = c.get() as usize; let mut out = Vec::with_capacity(n);
    let (mut pend, mut pline, mut pcol) = (0u32, 1u32, 1u32);
    for _ in 0..n {
        let v = c.get(); let term = (v >> 4) as u32; let class = u_class((v >> 1) & 7); let irr = v & 1 == 1;
        let g = c.get(); let gap = (g >> 1) as u32; let nl = g & 1 == 1;
        let (mut line, mut col) = (pline, pcol + gap);
        if nl { line = pline + c.get() as u32; col = c.get() as u32 + 1 }
        let start = pend + gap;
        let s;
        if !irr { let (len, chars) = text_of(term); s = Span { start, end: start + len as u32, start_line: line, start_col: col, end_line: line, end_col: col + chars as u32 } }
        else {
            let col2 = (col as i64 + unzz(c.get())) as u32; let el = line + c.get() as u32; let ec = c.get() as u32; let len = c.get() as u32;
            s = Span { start, end: start + len, start_line: line, start_col: col2, end_line: el, end_col: ec };
        }
        pend = s.end; pline = s.end_line; pcol = s.end_col;
        out.push((term, class, s));
    }
    out
}

// ---------- prototype store P ----------
const P_META: TableDefinition<&str, u64> = TableDefinition::new("p_meta");
const P_NAMES: TableDefinition<&str, u64> = TableDefinition::new("p_names");
const P_ENT: TableDefinition<u64, &[u8]> = TableDefinition::new("p_ent");
const P_TERMS: TableDefinition<&str, u32> = TableDefinition::new("p_terms");
const P_TERMTXT: TableDefinition<u32, &str> = TableDefinition::new("p_termtxt");
const P_STREAM: TableDefinition<u64, &[u8]> = TableDefinition::new("p_stream");
const P_POST: TableDefinition<(u32, u64), &[u8]> = TableDefinition::new("p_post");
const P_SYMS: MultimapTableDefinition<&str, u64> = MultimapTableDefinition::new("p_syms");
// model E (per-token binary nodes)
const E_TOKS: MultimapTableDefinition<u32, u64> = MultimapTableDefinition::new("e_toks");
const E_CHILD: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("e_children");

fn ent_container(kind: u8, parent: u64, name: &str) -> Vec<u8> { let mut v = vec![kind]; put(&mut v, parent); put_str(&mut v, name); v }
fn ent_file(parent: u64, name: &str, lang: &str, err: bool, fp: &[u8], first_sym: u64, nsym: u64, ntok: u64) -> Vec<u8> {
    let mut v = ent_container(2, parent, name); put_str(&mut v, lang); v.push(err as u8); v.extend_from_slice(fp); put(&mut v, first_sym); put(&mut v, nsym); put(&mut v, ntok); v
}
fn ent_sym(parent: u64, s: &graph_core::SymbolDecl) -> Vec<u8> {
    let mut v = ent_container(3, parent, &s.name); v.push(sk_u(s.kind)); put_str(&mut v, s.lang_kind.as_deref().unwrap_or(""));
    let p = &s.span; for x in [p.start, p.end - p.start, p.start_line, p.start_col, p.end_line - p.start_line, p.end_col] { put(&mut v, x as u64) } v
}
struct Ent<'a> { kind: u8, parent: u64, name: &'a str, rest: Cur<'a> }
fn ent_dec(b: &[u8]) -> Ent<'_> { let mut c = Cur(b, 1); let parent = c.get(); let name = c.str(); Ent { kind: b[0], parent, name, rest: c } }
struct FileRec<'a> { lang: &'a str, fp: &'a [u8], first_sym: u64, nsym: u64, ntok: u64 }
fn file_rec<'a>(e: &mut Ent<'a>) -> FileRec<'a> {
    let lang = e.rest.str(); e.rest.1 += 1; let fp = &e.rest.0[e.rest.1..e.rest.1 + 32]; e.rest.1 += 32;
    FileRec { lang, fp, first_sym: e.rest.get(), nsym: e.rest.get(), ntok: e.rest.get() }
}
#[derive(Clone)] struct SymRec { id: u64, name: String, kind: SymbolKind, lang_kind: String, span: Span }
fn sym_rec(id: u64, e: &mut Ent<'_>) -> SymRec {
    let kind = u_sk(e.rest.0[e.rest.1]); e.rest.1 += 1; let lk = e.rest.str().to_string();
    let (a, l, sl, sc, el, ec) = (e.rest.get() as u32, e.rest.get() as u32, e.rest.get() as u32, e.rest.get() as u32, e.rest.get() as u32, e.rest.get() as u32);
    SymRec { id, name: e.name.to_string(), kind, lang_kind: lk, span: Span { start: a, end: a + l, start_line: sl, start_col: sc, end_line: sl + el, end_col: ec } }
}

struct P { db: Database, stop: std::collections::HashSet<String>, all_postings: bool }
impl P {
    fn open(path: &str, stop: std::collections::HashSet<String>, all_postings: bool) -> P { P { db: Database::create(path).unwrap(), stop, all_postings } }
    fn ingest_batch(&self, org: &str, repo: &str, files: &[&Src], reg: &Registry) -> (usize, usize) {
        let wt = self.db.begin_write().unwrap(); let (mut ntok, mut nfile) = (0, 0);
        {
            let mut meta = wt.open_table(P_META).unwrap(); let mut names = wt.open_table(P_NAMES).unwrap();
            let mut ent = wt.open_table(P_ENT).unwrap(); let mut terms = wt.open_table(P_TERMS).unwrap();
            let mut termtxt = wt.open_table(P_TERMTXT).unwrap(); let mut stream = wt.open_table(P_STREAM).unwrap();
            let mut post = wt.open_table(P_POST).unwrap(); let mut syms = wt.open_multimap_table(P_SYMS).unwrap();
            let mut next = meta.get("next").unwrap().map(|v| v.value()).unwrap_or(1);
            let mut next_term = meta.get("next_term").unwrap().map(|v| v.value()).unwrap_or(0) as u32;
            let ensure = |next: &mut u64, names: &mut redb::Table<&str, u64>, ent: &mut redb::Table<u64, &[u8]>, parent: u64, kind: u8, name: &str| -> u64 {
                let key = format!("{parent}\0{kind}\0{name}");
                if let Some(v) = names.get(key.as_str()).unwrap() { return v.value() }
                let id = *next; *next += 1; names.insert(key.as_str(), id).unwrap(); ent.insert(id, ent_container(kind, parent, name).as_slice()).unwrap(); id
            };
            let oid = ensure(&mut next, &mut names, &mut ent, 0, 0, org); let rid = ensure(&mut next, &mut names, &mut ent, oid, 1, repo);
            for f in files {
                let ex = reg.extract(&f.lang, &f.src);
                let fp: [u8; 32] = Sha256::digest(f.src.as_bytes()).into();
                let fid = ensure(&mut next, &mut names, &mut ent, rid, 2, &f.path);
                self.remove_file_body(&mut ent, &mut stream, &mut post, &mut syms, &termtxt, fid);
                let mut sy: Vec<_> = ex.symbols.iter().collect(); sy.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
                let mut toks: Vec<_> = ex.tokens.iter().collect(); toks.sort_by_key(|t| t.span.start);
                let first_sym = next;
                // symbols: parent by containment stack
                let mut open: Vec<(u64, u32)> = vec![];
                for s in &sy {
                    while open.last().is_some_and(|&(_, e)| e <= s.span.start) { open.pop(); }
                    let parent = open.last().map_or(fid, |x| x.0); let id = next; next += 1;
                    ent.insert(id, ent_sym(parent, s).as_slice()).unwrap(); syms.insert(s.name.as_str(), id).unwrap(); open.push((id, s.span.end));
                }
                let mut ids = Vec::with_capacity(toks.len());
                for t in &toks {
                    let found = terms.get(t.text.as_str()).unwrap().map(|v| v.value()); let id = match found { Some(v) => v, None => { let i = next_term; next_term += 1; terms.insert(t.text.as_str(), i).unwrap(); termtxt.insert(i, t.text.as_str()).unwrap(); i } };
                    ids.push(id);
                }
                stream.insert(fid, encode_stream(&toks, &ids).as_slice()).unwrap();
                let mut per: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
                for (i, &id) in ids.iter().enumerate() { per.entry(id).or_default().push(i as u64) }
                for (tid, ords) in per {
                    if !self.all_postings { let txt = termtxt.get(tid).unwrap().unwrap().value().to_string(); if self.stop.contains(&txt) { continue } }
                    let mut v = vec![]; put(&mut v, ords.len() as u64); let mut p = 0; for o in ords { put(&mut v, o - p); p = o }
                    post.insert((tid, fid), v.as_slice()).unwrap();
                }
                ent.insert(fid, ent_file(rid, &f.path, &f.lang, ex.has_errors, &fp, first_sym, next - first_sym, toks.len() as u64).as_slice()).unwrap();
                ntok += toks.len(); nfile += 1;
            }
            meta.insert("next", next).unwrap(); meta.insert("next_term", next_term as u64).unwrap();
        }
        wt.commit().unwrap(); (nfile, ntok)
    }
    fn remove_file_body(&self, ent: &mut redb::Table<u64, &[u8]>, stream: &mut redb::Table<u64, &[u8]>, post: &mut redb::Table<(u32, u64), &[u8]>, syms: &mut redb::MultimapTable<&str, u64>, termtxt: &redb::Table<u32, &str>, fid: u64) {
        let old = ent.get(fid).unwrap().map(|v| v.value().to_vec()); let Some(old) = old else { return };
        let mut e = ent_dec(&old); if e.rest.done() { return } let fr = file_rec(&mut e);
        for id in fr.first_sym..fr.first_sym + fr.nsym { let nm = ent.remove(id).unwrap().map(|v| ent_dec(v.value()).name.to_string()); if let Some(nm) = nm { syms.remove(nm.as_str(), id).unwrap(); } }
        if let Some(s) = stream.remove(fid).unwrap() {
            let bytes = s.value().to_vec(); let mut lens: HashMap<u32, (usize, usize)> = HashMap::new();
            let d = decode_stream(&bytes, &mut |t| *lens.entry(t).or_insert_with(|| { let v = termtxt.get(t).unwrap().unwrap(); let x = v.value(); (x.len(), x.chars().count()) }));
            let mut seen = std::collections::HashSet::new();
            for (t, _, _) in d { if seen.insert(t) { post.remove((t, fid)).unwrap(); } }
        }
    }
    fn delete_file(&self, org: &str, repo: &str, path: &str) -> bool {
        let wt = self.db.begin_write().unwrap(); let mut ok = false;
        {
            let mut names = wt.open_table(P_NAMES).unwrap(); let mut ent = wt.open_table(P_ENT).unwrap(); let termtxt = wt.open_table(P_TERMTXT).unwrap();
            let mut stream = wt.open_table(P_STREAM).unwrap(); let mut post = wt.open_table(P_POST).unwrap(); let mut syms = wt.open_multimap_table(P_SYMS).unwrap();
            let oid = names.get(format!("0\00\0{org}").as_str()).unwrap().map(|v| v.value());
            if let Some(oid) = oid { let rid = names.get(format!("{oid}\01\0{repo}").as_str()).unwrap().map(|v| v.value());
                if let Some(rid) = rid { let key = format!("{rid}\02\0{path}"); let fid = names.get(key.as_str()).unwrap().map(|v| v.value());
                    if let Some(fid) = fid { self.remove_file_body(&mut ent, &mut stream, &mut post, &mut syms, &termtxt, fid); ent.remove(fid).unwrap(); names.remove(key.as_str()).unwrap(); ok = true } } }
        }
        wt.commit().unwrap(); ok
    }
    /// unchanged? (hash + lookup)
    fn unchanged(&self, org: &str, repo: &str, path: &str, src: &str) -> bool {
        let rt = self.db.begin_read().unwrap(); let names = rt.open_table(P_NAMES).unwrap(); let ent = rt.open_table(P_ENT).unwrap();
        let fp: [u8; 32] = Sha256::digest(src.as_bytes()).into();
        let g = |k: String| names.get(k.as_str()).unwrap().map(|v| v.value());
        let Some(o) = g(format!("0\00\0{org}")) else { return false }; let Some(r) = g(format!("{o}\01\0{repo}")) else { return false };
        let Some(f) = g(format!("{r}\02\0{path}")) else { return false };
        let b = ent.get(f).unwrap().unwrap(); let mut e = ent_dec(b.value()); file_rec(&mut e).fp == fp
    }
    /// search -> (rows, total count)
    fn search(&self, q: &Query) -> (usize, usize) {
        let rt = self.db.begin_read().unwrap();
        let terms = rt.open_table(P_TERMS).unwrap(); let termtxt = rt.open_table(P_TERMTXT).unwrap(); let ent = rt.open_table(P_ENT).unwrap();
        let stream = rt.open_table(P_STREAM).unwrap(); let post = rt.open_table(P_POST).unwrap();
        let Some(tid) = terms.get(q.text.as_str()).unwrap().map(|v| v.value()) else { return (0, 0) };
        let indexed = self.all_postings || !self.stop.contains(&q.text);
        let mut cache: HashMap<u64, (String, String)> = HashMap::new(); // repo id -> (org, repo)
        let mut lens: HashMap<u32, (usize, usize)> = HashMap::new();
        let mut rows: BTreeMap<(String, String, String, u32, u64), usize> = BTreeMap::new();
        let mut files: Vec<(u64, Option<usize>)> = vec![];
        if indexed { for r in post.range((tid, 0u64)..=(tid, u64::MAX)).unwrap() { let (k, v) = r.unwrap(); let mut c = Cur(v.value(), 0); files.push((k.value().1, Some(c.get() as usize))) } }
        else { for r in stream.iter().unwrap() { files.push((r.unwrap().0.value(), None)) } }
        for (fid, cnt) in files {
            let fb = ent.get(fid).unwrap().unwrap().value().to_vec(); let mut fe = ent_dec(&fb); let fr = file_rec(&mut fe);
            if let Some(l) = &q.language { if !fr.lang.eq_ignore_ascii_case(l) { continue } }
            let (org, repo) = cache.entry(fe.parent).or_insert_with(|| { let rb = ent.get(fe.parent).unwrap().unwrap().value().to_vec(); let re = ent_dec(&rb); let ob = ent.get(re.parent).unwrap().unwrap().value().to_vec(); (ent_dec(&ob).name.to_string(), re.name.to_string()) }).clone();
            if q.org.as_ref().is_some_and(|o| *o != org) || q.repo.as_ref().is_some_and(|r| *r != repo) { continue }
            let need_stream = q.class.is_some() || matches!(q.grain, Grain::Token | Grain::Symbol) || cnt.is_none();
            if !need_stream { let c = cnt.unwrap(); let key = match q.grain { Grain::File => (org, repo, fe.name.to_string(), 0, 0), Grain::Repo => (org, repo, String::new(), 0, 0), _ => (org, String::new(), String::new(), 0, 0) }; *rows.entry(key).or_default() += c; continue }
            let sb = stream.get(fid).unwrap().unwrap().value().to_vec();
            let dec = decode_stream(&sb, &mut |t| *lens.entry(t).or_insert_with(|| { let v = termtxt.get(t).unwrap().unwrap(); let x = v.value(); (x.len(), x.chars().count()) }));
            let syms: Vec<SymRec> = if q.grain == Grain::Symbol { (fr.first_sym..fr.first_sym + fr.nsym).map(|id| { let b = ent.get(id).unwrap().unwrap().value().to_vec(); let mut e = ent_dec(&b); sym_rec(id, &mut e) }).collect() } else { vec![] };
            for (t, class, span) in dec {
                if t != tid || q.class.is_some_and(|c| c != class) { continue }
                let key = match q.grain {
                    Grain::Token => (org.clone(), repo.clone(), fe.name.to_string(), span.start, 0),
                    Grain::Symbol => { let s = syms.iter().rev().find(|s| s.span.start <= span.start && span.start < s.span.end && q.symbol_kind.as_ref().is_none_or(|k| s.kind.as_str() == k)); match s { Some(s) => (org.clone(), repo.clone(), fe.name.to_string(), s.span.start, s.id), None => (org.clone(), repo.clone(), fe.name.to_string(), 0, 0) } }
                    Grain::File => (org.clone(), repo.clone(), fe.name.to_string(), 0, 0), Grain::Repo => (org.clone(), repo.clone(), String::new(), 0, 0), Grain::Org => (org.clone(), String::new(), String::new(), 0, 0) };
                *rows.entry(key).or_default() += 1;
            }
        }
        (rows.len(), rows.values().sum())
    }
    fn symbols(&self, name: &str) -> usize {
        let rt = self.db.begin_read().unwrap(); let ent = rt.open_table(P_ENT).unwrap(); let syms = rt.open_multimap_table(P_SYMS).unwrap();
        let mut n = 0; for v in syms.get(name).unwrap() { let id = v.unwrap().value(); let b = ent.get(id).unwrap().unwrap().value().to_vec(); let e = ent_dec(&b); let _ = e.name; // ancestors
            let mut p = e.parent; loop { let b = ent.get(p).unwrap().unwrap().value().to_vec(); let pe = ent_dec(&b); if pe.kind == 0 { break } p = pe.parent }
            n += 1 } n
    }
    fn describe(&self) -> usize {
        let rt = self.db.begin_read().unwrap(); let ent = rt.open_table(P_ENT).unwrap(); let mut m: BTreeMap<(u64, String), (usize, u64, u64)> = BTreeMap::new();
        for r in ent.iter().unwrap() { let (_, v) = r.unwrap(); let b = v.value(); if b[0] == 2 { let mut e = ent_dec(b); let fr = file_rec(&mut e); let x = m.entry((e.parent, fr.lang.to_string())).or_default(); x.0 += 1; x.1 += fr.nsym; x.2 += fr.ntok } }
        m.len()
    }
}

// ---------- model E: per-token binary nodes ----------
fn ingest_e(db: &Database, org: &str, repo: &str, files: &[&Src], reg: &Registry) -> (usize, usize) {
    let wt = db.begin_write().unwrap(); let (mut nt, mut nf) = (0, 0);
    {
        let mut meta = wt.open_table(P_META).unwrap(); let mut names = wt.open_table(P_NAMES).unwrap(); let mut ent = wt.open_table(P_ENT).unwrap();
        let mut terms = wt.open_table(P_TERMS).unwrap(); let mut termtxt = wt.open_table(P_TERMTXT).unwrap(); let mut tk = wt.open_multimap_table(E_TOKS).unwrap(); let mut ch = wt.open_multimap_table(E_CHILD).unwrap();
        let mut next = meta.get("next").unwrap().map(|v| v.value()).unwrap_or(1); let mut next_term = meta.get("next_term").unwrap().map(|v| v.value()).unwrap_or(0) as u32;
        let ensure = |next: &mut u64, names: &mut redb::Table<&str, u64>, ent: &mut redb::Table<u64, &[u8]>, ch: &mut redb::MultimapTable<u64, u64>, parent: u64, kind: u8, name: &str| -> u64 {
            let key = format!("{parent}\0{kind}\0{name}"); if let Some(v) = names.get(key.as_str()).unwrap() { return v.value() }
            let id = *next; *next += 1; names.insert(key.as_str(), id).unwrap(); ent.insert(id, ent_container(kind, parent, name).as_slice()).unwrap(); if parent != 0 { ch.insert(parent, id).unwrap(); } id };
        let oid = ensure(&mut next, &mut names, &mut ent, &mut ch, 0, 0, org); let rid = ensure(&mut next, &mut names, &mut ent, &mut ch, oid, 1, repo);
        for f in files {
            let ex = reg.extract(&f.lang, &f.src); let fid = ensure(&mut next, &mut names, &mut ent, &mut ch, rid, 2, &f.path);
            let fp: [u8; 32] = Sha256::digest(f.src.as_bytes()).into();
            ent.insert(fid, ent_file(rid, &f.path, &f.lang, ex.has_errors, &fp, 0, 0, ex.tokens.len() as u64).as_slice()).unwrap();
            let mut sy: Vec<_> = ex.symbols.iter().collect(); sy.sort_by_key(|s| (s.span.start, std::cmp::Reverse(s.span.end)));
            let mut toks: Vec<_> = ex.tokens.iter().collect(); toks.sort_by_key(|t| t.span.start);
            let mut open: Vec<(u64, u32)> = vec![]; let (mut si, mut ti) = (0, 0);
            while si < sy.len() || ti < toks.len() {
                let take = si < sy.len() && (ti >= toks.len() || sy[si].span.start <= toks[ti].span.start);
                let pos = if take { sy[si].span.start } else { toks[ti].span.start };
                while open.last().is_some_and(|&(_, e)| e <= pos) { open.pop(); }
                let parent = open.last().map_or(fid, |x| x.0); let id = next; next += 1;
                if take { let s = sy[si]; si += 1; ent.insert(id, ent_sym(parent, s).as_slice()).unwrap(); open.push((id, s.span.end)); }
                else { let t = toks[ti]; ti += 1;
                    let found = terms.get(t.text.as_str()).unwrap().map(|v| v.value()); let tid = match found { Some(v) => v, None => { let i = next_term; next_term += 1; terms.insert(t.text.as_str(), i).unwrap(); termtxt.insert(i, t.text.as_str()).unwrap(); i } };
                    let mut v = vec![4u8]; put(&mut v, parent); put(&mut v, tid as u64); v.push(class_u(t.class) as u8); let p = &t.span;
                    for x in [p.start, p.end - p.start, p.start_line, p.start_col, p.end_line - p.start_line, p.end_col] { put(&mut v, x as u64) }
                    ent.insert(id, v.as_slice()).unwrap(); tk.insert(tid, id).unwrap(); nt += 1; }
                ch.insert(parent, id).unwrap();
            }
            nf += 1;
        }
        meta.insert("next", next).unwrap(); meta.insert("next_term", next_term as u64).unwrap();
    }
    wt.commit().unwrap(); (nf, nt)
}

// ---------- helpers ----------
fn rss_kb() -> u64 { std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse().ok()).unwrap_or(0) }
fn io() -> (u64, u64, u64) { let s = std::fs::read_to_string("/proc/self/io").unwrap(); let g = |k: &str| s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse().ok()).unwrap_or(0); (g("wchar"), g("write_bytes"), g("read_bytes")) }
fn pct(v: &mut Vec<f64>, p: f64) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[((v.len() - 1) as f64 * p).round() as usize] }
fn fsz(p: &str) -> u64 { std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) }
fn timeit<F: FnMut()>(reps: usize, mut f: F) -> (f64, f64, f64) { let mut v = vec![]; for _ in 0..reps { let t = Instant::now(); f(); v.push(t.elapsed().as_secs_f64() * 1e3) } (pct(&mut v.clone(), 0.5), pct(&mut v.clone(), 0.95), v.iter().sum::<f64>() / v.len() as f64) }

fn stop_set(set: &[Src], reg: &Registry, k: usize) -> std::collections::HashSet<String> {
    let mut c: HashMap<String, u64> = HashMap::new();
    for f in set { for t in reg.extract(&f.lang, &f.src).tokens { *c.entry(t.text).or_default() += 1 } }
    let mut v: Vec<_> = c.into_iter().collect(); v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0))); v.into_iter().take(k).map(|x| x.0).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let cmd = a[1].as_str(); let reg = registry();
    match cmd {
        // freq <corpus>
        "freq" => {
            let base = load(&a[2]); let mut c: HashMap<String, (u64, [u64; 7])> = HashMap::new(); let (mut ntok, mut bytes, mut sym, mut chars) = (0u64, 0u64, 0u64, 0u64);
            let mut cls = [0u64; 7]; let mut tb = 0u64; let mut irregular = 0u64;
            for f in &base { let ex = reg.extract(&f.lang, &f.src); bytes += f.src.len() as u64; sym += ex.symbols.len() as u64;
                for t in &ex.tokens { let e = c.entry(t.text.clone()).or_default(); e.0 += 1; e.1[class_u(t.class) as usize] += 1; ntok += 1; cls[class_u(t.class) as usize] += 1; tb += t.text.len() as u64; chars += 1;
                    if t.span.end_line != t.span.start_line { irregular += 1 } } }
            let mut v: Vec<_> = c.iter().collect(); v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
            println!("files={} src_bytes={} tokens={} symbols={} distinct_texts={} avg_token_text_bytes={:.2} multiline_tokens={}", base.len(), bytes, ntok, sym, v.len(), tb as f64 / chars as f64, irregular);
            println!("class occurrences: {:?}", ["identifier","keyword","literal","operator","punctuation","comment","other"].iter().zip(cls).collect::<Vec<_>>());
            let mut dc = [0u64; 7]; for (_, (_, cl)) in &c { let m = cl.iter().enumerate().max_by_key(|x| x.1).unwrap().0; dc[m] += 1 } println!("class distinct(by majority): {:?}", dc);
            let mut cum = 0u64; let marks = [10usize, 100, 1000, 10000]; let mut mi = 0;
            for (i, (_, (n, _))) in v.iter().enumerate() { cum += n; if mi < marks.len() && i + 1 == marks[mi] { println!("top {} texts cover {:.1}% of occurrences", marks[mi], 100.0 * cum as f64 / ntok as f64); mi += 1 } }
            let once = v.iter().filter(|x| x.1 .0 == 1).count(); println!("texts occurring once: {} ({:.1}% of distinct, {:.1}% of occurrences)", once, 100.0 * once as f64 / v.len() as f64, 100.0 * once as f64 / ntok as f64);
            for (i, (t, (n, _))) in v.iter().take(30).enumerate() { println!("  #{:<2} {:?} {} ({:.2}%)", i + 1, t, n, 100.0 * *n as f64 / ntok as f64) }
            for r in [100usize, 500, 2000] { if let Some((t, (n, _))) = v.get(r) { println!("  rank {r}: {:?} {}", t, n) } }
            let dict_bytes: usize = v.iter().map(|x| x.0.len()).sum(); println!("dictionary text bytes: {}", dict_bytes);
            // dup content
            let mut h: HashMap<[u8; 32], usize> = HashMap::new(); let mut dupb = 0u64; for f in &base { let x: [u8; 32] = Sha256::digest(f.src.as_bytes()).into(); let e = h.entry(x).or_default(); if *e > 0 { dupb += f.src.len() as u64 } *e += 1 }
            println!("identical-content files: {} of {} distinct hashes, duplicate bytes {}", base.len() - h.len(), h.len(), dupb);
        }
        // ingest <model a|e|p|pall|pnone> <db> <copies> <mutate 0|1> <corpus> [stopK]
        "ingest" => {
            let (model, dbp, copies, mutate, corpus) = (a[2].as_str(), &a[3], a[4].parse::<usize>().unwrap(), a[5] == "1", &a[6]);
            let stopk: usize = a.get(7).map(|x| x.parse().unwrap()).unwrap_or(0);
            let base = load(corpus); let set = build_set(&base, copies, mutate); let srcbytes: usize = set.iter().map(|f| f.src.len()).sum();
            let stop = if model == "p" && stopk > 0 { stop_set(&set, &reg, stopk) } else { Default::default() };
            let mut by: BTreeMap<&str, Vec<&Src>> = BTreeMap::new(); for f in &set { by.entry(&f.repo).or_default().push(f) }
            let (w0, wb0, _) = io(); let t0 = Instant::now(); let (mut nf, mut nt) = (0, 0); let mut txs = 0; let mut growth: Vec<u64> = vec![];
            match model {
                "a" => { let st = { let mut s = Store::open(dbp).unwrap(); s.register(Box::new(graph_lang_rust::RustExtractor)); s };
                    for (repo, fs) in &by { let org = format!("org{}", repo.rsplit('-').next().unwrap().parse::<usize>().unwrap_or(0) % 10);
                        let bf: Vec<BatchFile> = fs.iter().map(|f| BatchFile { path: &f.path, bytes: f.src.as_bytes(), language: None, origin: Some("directory") }).collect();
                        let r = st.index_batch(&org, repo, &bf).unwrap(); for x in r { let s = x.unwrap(); nt += s.tokens; nf += 1 } txs += 1; growth.push(fsz(dbp)) } }
                "e" => { let db = Database::create(dbp).unwrap(); for (repo, fs) in &by { let org = format!("org{}", repo.rsplit('-').next().unwrap().parse::<usize>().unwrap_or(0) % 10); let (f, t) = ingest_e(&db, &org, repo, fs, &reg); nf += f; nt += t; txs += 1; growth.push(fsz(dbp)) } }
                _ => { let p = P::open(dbp, stop, model == "pall" || model == "p" && stopk == 0);
                    for (repo, fs) in &by { let org = format!("org{}", repo.rsplit('-').next().unwrap().parse::<usize>().unwrap_or(0) % 10); let (f, t) = p.ingest_batch(&org, repo, fs, &reg); nf += f; nt += t; txs += 1; growth.push(fsz(dbp)) } }
            }
            let el = t0.elapsed().as_secs_f64(); let (w1, wb1, _) = io();
            println!("model={model} stopK={stopk} copies={copies} mutate={mutate} files={nf} tokens={nt} src_MB={:.2} txs={txs} ingest_s={el:.2} files/s={:.0} tokens/s={:.0} MB/s={:.2} peakRSS_MB={:.0} db_MB={:.2} B/token={:.1} B/file={:.0} wchar_MB={:.1} write_bytes_MB={:.1} write_amp(wchar/db)={:.2}",
                srcbytes as f64 / 1e6, nf as f64 / el, nt as f64 / el, srcbytes as f64 / 1e6 / el, rss_kb() as f64 / 1024.0, fsz(dbp) as f64 / 1e6, fsz(dbp) as f64 / nt as f64, fsz(dbp) as f64 / nf as f64, (w1 - w0) as f64 / 1e6, (wb1 - wb0) as f64 / 1e6, (w1 - w0) as f64 / fsz(dbp) as f64);
            if txs > 0 { let mx = growth.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(growth[0]); println!("  largest single-commit file growth: {:.1} MB (of {} commits)", mx as f64 / 1e6, txs) }
        }
        // stats <db> : per-table stats (redb) for any of our tables + compaction
        "stats" => {
            let mut db = Database::open(&a[2]).unwrap(); let before = fsz(&a[2]);
            { let rt = db.begin_read().unwrap();
              macro_rules! t { ($n:expr, $def:expr) => { if let Ok(t) = rt.open_table($def) { let s = t.stats().unwrap(); println!("{:<16} entries={:<9} stored={:>10} meta={:>9} frag={:>9} leaf_pages={:<7} branch_pages={:<6} height={}", $n, t.len().unwrap(), s.stored_bytes(), s.metadata_bytes(), s.fragmented_bytes(), s.leaf_pages(), s.branch_pages(), s.tree_height()) } } }
              macro_rules! m { ($n:expr, $def:expr) => { if let Ok(t) = rt.open_multimap_table($def) { let s = t.stats().unwrap(); println!("{:<16} entries={:<9} stored={:>10} meta={:>9} frag={:>9} leaf_pages={:<7} branch_pages={:<6} height={}", $n, t.len().unwrap(), s.stored_bytes(), s.metadata_bytes(), s.fragmented_bytes(), s.leaf_pages(), s.branch_pages(), s.tree_height()) } } }
              // A tables
              t!("meta", TableDefinition::<&str, u64>::new("meta")); t!("nodes", TableDefinition::<u64, &[u8]>::new("nodes")); t!("names", TableDefinition::<&str, u64>::new("names"));
              m!("children", MultimapTableDefinition::<u64, u64>::new("children")); m!("tokens_by_text", MultimapTableDefinition::<&str, u64>::new("tokens_by_text")); m!("symbols_by_name", MultimapTableDefinition::<&str, u64>::new("symbols_by_name"));
              t!("p_names", P_NAMES); t!("p_ent", P_ENT); t!("p_terms", P_TERMS); t!("p_termtxt", P_TERMTXT); t!("p_stream", P_STREAM); t!("p_post", P_POST); m!("p_syms", P_SYMS); m!("e_toks", E_TOKS); m!("e_children", E_CHILD);
              if let Ok(t) = rt.open_table(TableDefinition::<u64, &[u8]>::new("nodes")) { // JSON analysis
                  let (mut n, mut b) = ([0u64; 5], [0u64; 5]); let mut fields = 0u64; let mut tokjson = 0u64;
                  for r in t.iter().unwrap() { let (_, v) = r.unwrap(); let node: Node = serde_json::from_slice(v.value()).unwrap(); let k = node.kind as usize; n[k] += 1; b[k] += v.value().len() as u64; if node.kind == NodeKind::Token { tokjson += serde_json::to_string(&node.name).unwrap().len() as u64; fields += 1 } }
                  for (i, nm) in ["org", "repo", "file", "symbol", "token"].iter().enumerate() { if n[i] > 0 { println!("nodes[{nm}]: count={} json_bytes={} avg={:.1}", n[i], b[i], b[i] as f64 / n[i] as f64) } }
                  if fields > 0 { println!("token name text (JSON-quoted) share = {:.1}% of token json bytes", 100.0 * tokjson as f64 / b[4] as f64) }
              } }
            let mut rounds = 0; while db.compact().unwrap() && rounds < 20 { rounds += 1 } println!("compact rounds that changed something: {rounds}"); println!("file bytes: before compact {} after compact {}", before, fsz(&a[2]));
        }
        // bench <model> <db> [stopK] : open time, search/symbols/describe latency; correctness vs A given <adb>
        "bench" => {
            let (model, dbp) = (a[2].as_str(), &a[3]); let reps: usize = a.get(4).map(|x| x.parse().unwrap()).unwrap_or(30); let stopk: usize = a.get(5).map(|x| x.parse().unwrap()).unwrap_or(0);
            let corpus = std::env::var("SPIKE_CORPUS").unwrap(); let base = load(&corpus);
            let stop = if model == "p" && stopk > 0 { stop_set(&base, &reg, stopk) } else { Default::default() };
            // pick query terms from base corpus frequency
            let mut c: HashMap<String, u64> = HashMap::new(); for f in &base { for t in reg.extract(&f.lang, &f.src).tokens { *c.entry(t.text).or_default() += 1 } }
            let mut v: Vec<_> = c.into_iter().collect(); v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let idents: Vec<_> = v.iter().filter(|x| x.0.chars().all(|c| c.is_alphanumeric() || c == '_') && x.0.len() > 3).collect();
            let qs: Vec<(String, String)> = vec![("very common punct".into(), v[0].0.clone()), ("very common 2".into(), v[1].0.clone()), ("common ident".into(), idents[0].0.clone()), ("mid (rank~500)".into(), v[500].0.clone()), ("rare (count 2)".into(), v.iter().find(|x| x.1 == 2 && x.0.len() > 5).unwrap().0.clone()), ("absent".into(), "zzz_nope_zzz".into())];
            let t0 = Instant::now();
            enum S { A(Store), P(P) }
            let s = match model { "a" => { let mut st = Store::open(dbp).unwrap(); st.register(Box::new(graph_lang_rust::RustExtractor)); S::A(st) } _ => S::P(P::open(dbp, stop, model == "pall" || (model == "p" && stopk == 0))) };
            println!("model={model} stopK={stopk} open_ms={:.2} (first open, includes any rebuild) ", t0.elapsed().as_secs_f64() * 1e3);
            let mut tot = 0;
            for (label, text) in &qs {
                for (gname, grain) in [("token", Grain::Token), ("symbol", Grain::Symbol), ("file", Grain::File), ("repo", Grain::Repo), ("org", Grain::Org)] {
                    for (filt, lang, class) in [("none", None, None), ("lang=rust", Some("rust".to_string()), None), ("class=keyword", None, Some(TokenClass::Keyword))] {
                        if filt != "none" && !(label.starts_with("very common 2") || label.starts_with("common ident") ) && gname != "file" && gname != "token" { continue }
                        let mut q = Query::new(text.clone()); q.grain = grain; q.language = lang; q.class = class;
                        let mut res = (0, 0);
                        let (p50, p95, mean) = match &s { S::A(st) => timeit(reps, || { let h = st.search(&q).unwrap(); res = (h.len(), h.iter().map(|x| x.count).sum()) }), S::P(p) => timeit(reps, || { res = p.search(&q) }) };
                        tot += 1; println!("search | {label:<18} {:?} | {gname:<6} | {filt:<13} | rows={:<7} count={:<8} p50={p50:.3}ms p95={p95:.3}ms mean={mean:.3}", text.chars().take(14).collect::<String>(), res.0, res.1);
                    }
                }
            }
            let sym_name = "new"; let (p50, p95, _) = match &s { S::A(st) => { let mut n = 0; let r = timeit(reps, || { let q = SymbolQuery::new(sym_name); n = st.search_symbols(&q).unwrap().len() }); println!("symbols exact `{sym_name}` -> {n} hits"); r } S::P(p) => { let mut n = 0; let r = timeit(reps, || n = p.symbols(sym_name)); println!("symbols exact `{sym_name}` -> {n} hits"); r } };
            println!("symbols | p50={p50:.3}ms p95={p95:.3}ms");
            let (p50, p95, _) = match &s { S::A(st) => timeit(reps.min(10), || { st.describe(None, None).unwrap(); }), S::P(p) => timeit(reps.min(10), || { p.describe(); }) };
            println!("describe | p50={p50:.3}ms p95={p95:.3}ms ({} search cases)", tot); let _ = model;
        }
        // mutate <model> <db> <corpus> [stopK] : reindex/delete/skip latency
        "mutate" => {
            let (model, dbp, corpus) = (a[2].as_str(), &a[3], &a[4]); let stopk: usize = a.get(5).map(|x| x.parse().unwrap()).unwrap_or(0);
            let base = load(corpus); let big = base.iter().filter(|f| f.lang == "csharp").max_by_key(|f| f.src.len()).unwrap(); let small = base.iter().filter(|f| f.lang == "rust").min_by_key(|f| f.src.len()).unwrap();
            let stop = if model == "p" && stopk > 0 { stop_set(&base, &reg, stopk) } else { Default::default() };
            let reps = 20;
            for (label, f) in [("largest csharp file", big), ("smallest rust file", small)] {
                let toks = reg.extract(&f.lang, &f.src).tokens.len(); let org = "org0"; let repo = format!("{}-0", f.repo);
                match model {
                    "a" => { let mut st = Store::open(dbp).unwrap(); st.register(Box::new(graph_lang_rust::RustExtractor));
                        let bf = [BatchFile { path: &f.path, bytes: f.src.as_bytes(), language: None, origin: Some("directory") }];
                        let (p50, p95, _) = timeit(reps, || { st.index_batch(org, &repo, &bf).unwrap(); }); println!("A reindex {label} ({} B, {toks} tok): p50={p50:.2}ms p95={p95:.2}ms", f.src.len());
                        // delete via prune (keep everything but this file), then restore
                        let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new(); for g in base.iter().filter(|g| g.repo == f.repo && g.path != f.path) { keep.insert(g.path.clone()); }
                        let mut v = vec![]; for _ in 0..5 { let t = Instant::now(); let r = st.prune_files(org, &repo, &keep, false).unwrap(); v.push(t.elapsed().as_secs_f64() * 1e3); assert_eq!(r.len(), 1); st.index_batch(org, &repo, &bf).unwrap(); }
                        println!("A delete {label} (via prune_files, scans repo files): p50={:.2}ms", pct(&mut v, 0.5)); }
                    _ => { let p = P::open(dbp, stop.clone(), model == "pall" || (model == "p" && stopk == 0)); let reg2 = registry();
                        let (p50, p95, _) = timeit(reps, || { p.ingest_batch(org, &repo, &[f], &reg2); }); println!("P reindex {label} ({} B, {toks} tok): p50={p50:.2}ms p95={p95:.2}ms", f.src.len());
                        let mut v = vec![]; for _ in 0..5 { let t = Instant::now(); assert!(p.delete_file(org, &repo, &f.path)); v.push(t.elapsed().as_secs_f64() * 1e3); p.ingest_batch(org, &repo, &[f], &reg2); } println!("P delete {label}: p50={:.2}ms", pct(&mut v, 0.5));
                        let (p50, p95, _) = timeit(200, || { assert!(p.unchanged(org, &repo, &f.path, &f.src)); }); println!("P skip-unchanged check {label}: p50={p50:.3}ms p95={p95:.3}ms"); }
                }
            }
        }
        // search1 <model> <db> <text> <grain> [stopK]: one query in a fresh process (cold/warm start, RSS)
        "search1" => {
            let (model, dbp, text) = (a[2].as_str(), &a[3], &a[4]); let grain: Grain = a[5].parse().unwrap(); let stopk: usize = a.get(6).map(|x| x.parse().unwrap()).unwrap_or(0);
            let corpus = std::env::var("SPIKE_CORPUS").unwrap(); let stop = if stopk > 0 { stop_set(&load(&corpus), &reg, stopk) } else { Default::default() };
            let (_, _, rb0) = io(); let t0 = Instant::now(); let mut q = Query::new(text.clone()); q.grain = grain;
            let (rows, cnt);
            if model == "a" { let mut st = Store::open(dbp).unwrap(); st.register(Box::new(graph_lang_rust::RustExtractor)); let open = t0.elapsed(); let h = st.search(&q).unwrap(); rows = h.len(); cnt = h.iter().map(|x| x.count).sum::<usize>(); println!("open={:.1}ms", open.as_secs_f64() * 1e3) }
            else { let p = P::open(dbp, stop, stopk == 0); println!("open={:.1}ms", t0.elapsed().as_secs_f64() * 1e3); let r = p.search(&q); rows = r.0; cnt = r.1 }
            let (_, _, rb1) = io(); println!("model={model} {text:?} {grain:?} rows={rows} count={cnt} total={:.1}ms peakRSS_MB={:.0} read_bytes_MB={:.1}", t0.elapsed().as_secs_f64() * 1e3, rss_kb() as f64 / 1024.0, (rb1 - rb0) as f64 / 1e6);
        }
        // verify: roundtrip stream codec on corpus
        "verify" => {
            let base = load(&a[2]); let (mut n, mut bytes, mut irr) = (0usize, 0usize, 0usize); let mut comp = 0usize;
            for f in &base { let ex = reg.extract(&f.lang, &f.src); let mut toks: Vec<_> = ex.tokens.iter().collect(); toks.sort_by_key(|t| t.span.start);
                let mut dict: HashMap<&str, u32> = HashMap::new(); let mut txt: Vec<&str> = vec![]; let ids: Vec<u32> = toks.iter().map(|t| *dict.entry(&t.text).or_insert_with(|| { txt.push(&t.text); txt.len() as u32 - 1 })).collect();
                let s = encode_stream(&toks, &ids); bytes += s.len(); n += toks.len(); comp += miniz_oxide::deflate::compress_to_vec(&s, 6).len();
                let d = decode_stream(&s, &mut |t| (txt[t as usize].len(), txt[t as usize].chars().count()));
                assert_eq!(d.len(), toks.len());
                for (x, t) in d.iter().zip(&toks) { assert_eq!(x.2, t.span, "span mismatch in {} tok {:?}", f.path, t.text); assert_eq!(txt[x.0 as usize], t.text); assert_eq!(x.1, t.class); if x.2.end_line != x.2.start_line { irr += 1 } } }
            println!("stream codec roundtrip OK: {n} tokens in {} files; stream bytes {bytes} = {:.2} B/token; deflate(6) per-file {comp} = {:.2} B/token", base.len(), bytes as f64 / n as f64, comp as f64 / n as f64);
        }
        _ => panic!("cmd"),
    }
}
