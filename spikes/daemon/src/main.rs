//! THROWAWAY spike for docs/spikes/daemon-and-locking.md. Not part of the workspace.
//! Commands: hold | try | retry | storm-worker | search-once | serve | bench
use graph_store::{Grain, Query, Store, StoreError, SymbolQuery};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}
fn rng() -> impl FnMut() -> u64 {
    let mut s = now_ns() as u64 ^ ((std::process::id() as u64) << 32) | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

// ---------------- wire model ----------------
#[derive(Serialize, Deserialize, Debug, Clone)]
enum Req {
    Ping,
    Search {
        text: String,
        limit: u32,
        symbol_grain: bool,
    },
    Symbols {
        pattern: String,
        limit: u32,
    },
    Describe,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct WHit {
    grain: String,
    org: String,
    repo: Option<String>,
    file: Option<String>,
    language: Option<String>,
    symbol: Option<String>,
    kind: Option<String>,
    span: Option<[u32; 6]>,
    count: u32,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
enum Resp {
    Pong,
    Hits(Vec<WHit>),
    Describe(String),
    Err(String),
}

fn span_arr(s: &graph_core::schema::Span) -> [u32; 6] {
    [
        s.start,
        s.end,
        s.start_line,
        s.start_col,
        s.end_line,
        s.end_col,
    ]
}
fn handle(store: &Store, r: &Req) -> Resp {
    match r {
        Req::Ping => Resp::Pong,
        Req::Search {
            text,
            limit,
            symbol_grain,
        } => {
            let mut q = Query::new(text.clone());
            q.limit = Some(*limit as usize);
            if *symbol_grain {
                q.grain = Grain::Symbol;
            }
            match store.search(&q) {
                Ok(h) => Resp::Hits(
                    h.iter()
                        .map(|h| WHit {
                            grain: format!("{:?}", h.grain),
                            org: h.org.clone(),
                            repo: h.repo.clone(),
                            file: h.file.clone(),
                            language: h.language.clone(),
                            symbol: h.symbol.clone(),
                            kind: h.symbol_kind.map(|k| format!("{k:?}")),
                            span: h.span.as_ref().map(span_arr),
                            count: h.count as u32,
                        })
                        .collect(),
                ),
                Err(e) => Resp::Err(e.to_string()),
            }
        }
        Req::Symbols { pattern, limit } => {
            let mut q = SymbolQuery::new(pattern.clone());
            q.limit = Some(*limit as usize);
            match store.search_symbols(&q) {
                Ok(h) => Resp::Hits(
                    h.iter()
                        .map(|h| WHit {
                            grain: "Symbol".into(),
                            org: h.org.clone(),
                            repo: Some(h.repo.clone()),
                            file: Some(h.file.clone()),
                            language: h.language.clone(),
                            symbol: Some(h.qualified.clone()),
                            kind: Some(format!("{:?}", h.kind)),
                            span: h.span.as_ref().map(span_arr),
                            count: 1,
                        })
                        .collect(),
                ),
                Err(e) => Resp::Err(e.to_string()),
            }
        }
        Req::Describe => match store.describe(None, None) {
            Ok(d) => Resp::Describe(serde_json::to_string(&d).unwrap()),
            Err(e) => Resp::Err(e.to_string()),
        },
    }
}

// ---------------- compact binary codec (hand-rolled, varints + length-prefixed strings) ----------------
fn pv(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push((v as u8) | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}
fn ps(b: &mut Vec<u8>, s: &str) {
    pv(b, s.len() as u64);
    b.extend_from_slice(s.as_bytes());
}
fn pos(b: &mut Vec<u8>, s: &Option<String>) {
    match s {
        None => b.push(0),
        Some(s) => {
            b.push(1);
            ps(b, s)
        }
    }
}
struct Rd<'a>(&'a [u8], usize);
impl<'a> Rd<'a> {
    fn v(&mut self) -> u64 {
        let (mut r, mut sh) = (0u64, 0);
        loop {
            let x = self.0[self.1];
            self.1 += 1;
            r |= ((x & 0x7f) as u64) << sh;
            if x < 0x80 {
                return r;
            }
            sh += 7;
        }
    }
    fn s(&mut self) -> String {
        let n = self.v() as usize;
        let s = std::str::from_utf8(&self.0[self.1..self.1 + n])
            .unwrap()
            .to_string();
        self.1 += n;
        s
    }
    fn os(&mut self) -> Option<String> {
        let t = self.0[self.1];
        self.1 += 1;
        if t == 0 {
            None
        } else {
            Some(self.s())
        }
    }
}
fn enc_req(r: &Req) -> Vec<u8> {
    let mut b = vec![];
    match r {
        Req::Ping => b.push(0),
        Req::Search {
            text,
            limit,
            symbol_grain,
        } => {
            b.push(1);
            ps(&mut b, text);
            pv(&mut b, *limit as u64);
            b.push(*symbol_grain as u8);
        }
        Req::Symbols { pattern, limit } => {
            b.push(2);
            ps(&mut b, pattern);
            pv(&mut b, *limit as u64);
        }
        Req::Describe => b.push(3),
    }
    b
}
fn dec_req(b: &[u8]) -> Req {
    let mut r = Rd(b, 1);
    match b[0] {
        0 => Req::Ping,
        1 => {
            let text = r.s();
            let limit = r.v() as u32;
            let symbol_grain = b[r.1] != 0;
            Req::Search {
                text,
                limit,
                symbol_grain,
            }
        }
        2 => Req::Symbols {
            pattern: r.s(),
            limit: r.v() as u32,
        },
        _ => Req::Describe,
    }
}
fn enc_resp(r: &Resp) -> Vec<u8> {
    let mut b = vec![];
    match r {
        Resp::Pong => b.push(0),
        Resp::Hits(h) => {
            b.push(1);
            pv(&mut b, h.len() as u64);
            for x in h {
                ps(&mut b, &x.grain);
                ps(&mut b, &x.org);
                pos(&mut b, &x.repo);
                pos(&mut b, &x.file);
                pos(&mut b, &x.language);
                pos(&mut b, &x.symbol);
                pos(&mut b, &x.kind);
                match x.span {
                    None => b.push(0),
                    Some(s) => {
                        b.push(1);
                        for v in s {
                            pv(&mut b, v as u64)
                        }
                    }
                }
                pv(&mut b, x.count as u64);
            }
        }
        Resp::Describe(s) => {
            b.push(2);
            ps(&mut b, s)
        }
        Resp::Err(s) => {
            b.push(3);
            ps(&mut b, s)
        }
    }
    b
}
fn dec_resp(b: &[u8]) -> Resp {
    let mut r = Rd(b, 1);
    match b[0] {
        0 => Resp::Pong,
        1 => {
            let n = r.v() as usize;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                let grain = r.s();
                let org = r.s();
                let repo = r.os();
                let file = r.os();
                let language = r.os();
                let symbol = r.os();
                let kind = r.os();
                let t = b[r.1];
                r.1 += 1;
                let span = if t == 0 {
                    None
                } else {
                    let mut s = [0u32; 6];
                    for v in s.iter_mut() {
                        *v = r.v() as u32;
                    }
                    Some(s)
                };
                let count = r.v() as u32;
                out.push(WHit {
                    grain,
                    org,
                    repo,
                    file,
                    language,
                    symbol,
                    kind,
                    span,
                    count,
                });
            }
            Resp::Hits(out)
        }
        2 => Resp::Describe(r.s()),
        _ => Resp::Err(r.s()),
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Json,
    Bin,
}
fn enc_r(e: Enc, r: &Req) -> Vec<u8> {
    match e {
        Enc::Json => serde_json::to_vec(r).unwrap(),
        Enc::Bin => enc_req(r),
    }
}
fn dec_r(e: Enc, b: &[u8]) -> Req {
    match e {
        Enc::Json => serde_json::from_slice(b).unwrap(),
        Enc::Bin => dec_req(b),
    }
}
fn enc_p(e: Enc, r: &Resp) -> Vec<u8> {
    match e {
        Enc::Json => serde_json::to_vec(r).unwrap(),
        Enc::Bin => enc_resp(r),
    }
}
fn dec_p(e: Enc, b: &[u8]) -> Resp {
    match e {
        Enc::Json => serde_json::from_slice(b).unwrap(),
        Enc::Bin => dec_resp(b),
    }
}
fn write_frame(s: &mut UnixStream, p: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4 + p.len());
    buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
    buf.extend_from_slice(p);
    s.write_all(&buf)
}
fn read_frame(s: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut l = [0u8; 4];
    s.read_exact(&mut l)?;
    let mut b = vec![0u8; u32::from_le_bytes(l) as usize];
    s.read_exact(&mut b)?;
    Ok(b)
}

// ---------------- commands ----------------
fn serve(db: &str, sock: &str, enc: Enc) {
    let mut store = Store::open(db).expect("open");
    store.register(Box::new(graph_lang_rust::RustExtractor));
    // NOTE: graph_store::Store is not Send/Sync today (Registry holds Box<dyn Extractor>), so this
    // spike serves one connection at a time on the accept thread. A real daemon needs Send+Sync.
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock).unwrap();
    eprintln!("serving {db} on {sock}");
    for c in l.incoming() {
        let mut c = c.unwrap();
        // server-side handle() time per request, grouped by request, so client RTT - handle = overhead on the SAME store
        let mut hm: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        while let Ok(f) = read_frame(&mut c) {
            let r = dec_r(enc, &f);
            let t = Instant::now();
            let resp = handle(&store, &r);
            hm.entry(format!("{r:?}"))
                .or_default()
                .push(ms(t.elapsed()));
            let p = enc_p(enc, &resp);
            if write_frame(&mut c, &p).is_err() {
                break;
            }
        }
        for (k, v) in hm.iter_mut() {
            let n = v.len();
            let (p50, p95, _) = summarize(std::mem::take(v));
            eprintln!(
                "server_handle enc={} n={n} p50={p50:.4}ms p95={p95:.4}ms req={k}",
                if enc == Enc::Json { "json" } else { "bin" }
            );
        }
    }
}

fn ops() -> Vec<(&'static str, Req)> {
    vec![
        ("ping", Req::Ping),
        (
            "search_rare_tok",
            Req::Search {
                text: "Backtrace".into(),
                limit: 100,
                symbol_grain: false,
            },
        ),
        (
            "search_common_tok_lim100",
            Req::Search {
                text: "self".into(),
                limit: 100,
                symbol_grain: false,
            },
        ),
        (
            "search_common_sym_lim100",
            Req::Search {
                text: "self".into(),
                limit: 100,
                symbol_grain: true,
            },
        ),
        (
            "search_common_tok_lim2000",
            Req::Search {
                text: "self".into(),
                limit: 2000,
                symbol_grain: false,
            },
        ),
        (
            "symbols_exact",
            Req::Symbols {
                pattern: "new".into(),
                limit: 100,
            },
        ),
        (
            "symbols_prefix_lim100",
            Req::Symbols {
                pattern: "f*".into(),
                limit: 100,
            },
        ),
        ("describe", Req::Describe),
    ]
}
fn summarize(mut v: Vec<f64>) -> (f64, f64, f64) {
    let p50 = pct(&mut v, 0.5);
    let p95 = pct(&mut v, 0.95);
    let p99 = pct(&mut v, 0.99);
    (p50, p95, p99)
}
fn iters_for(name: &str, tokens_hint: usize) -> usize {
    match name {
        "describe" => {
            if tokens_hint > 5_000_000 {
                15
            } else {
                100
            }
        }
        _ if tokens_hint > 5_000_000 => 100,
        _ => 1000,
    }
}

fn bench(db: &str, label: &str, sock_json: Option<&str>, sock_bin: Option<&str>) {
    // in-process
    let store =
        Store::open(db).expect("open (is the daemon holding it? bench inproc needs its own copy)");
    let mut inproc = std::collections::BTreeMap::new();
    for (name, req) in ops() {
        let n = iters_for(name, if label.contains("10m") { 10_000_000 } else { 0 });
        let _ = handle(&store, &req);
        let mut v = vec![];
        let mut rows = 0;
        for _ in 0..n {
            let t = Instant::now();
            let r = handle(&store, &req);
            v.push(ms(t.elapsed()));
            if let Resp::Hits(h) = &r {
                rows = h.len();
            }
        }
        inproc.insert(name, (summarize(v), rows));
    }
    drop(store);
    println!("dataset={label}");
    println!(
        "{:<28} {:>5} {:>9} {:>9} | {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9} | {:>7} {:>7}",
        "op",
        "rows",
        "inp_p50",
        "inp_p95",
        "json_p50",
        "json_p95",
        "json_d50",
        "bin_p50",
        "bin_p95",
        "bin_d50",
        "jsonB",
        "binB"
    );
    let mut cj = sock_json.map(|s| UnixStream::connect(s).unwrap());
    let mut cb = sock_bin.map(|s| UnixStream::connect(s).unwrap());
    for (name, req) in ops() {
        let n = iters_for(name, if label.contains("10m") { 10_000_000 } else { 0 });
        let ((i50, i95, _), rows) = inproc[name];
        let mut cols = vec![];
        for (c, e) in [(cj.as_mut(), Enc::Json), (cb.as_mut(), Enc::Bin)] {
            let Some(c) = c else {
                cols.push((f64::NAN, f64::NAN, 0));
                continue;
            };
            let mut v = vec![];
            let mut bytes = 0;
            for k in 0..n + 5 {
                let t = Instant::now();
                write_frame(c, &enc_r(e, &req)).unwrap();
                let f = read_frame(c).unwrap();
                let r = dec_p(e, &f);
                let d = ms(t.elapsed());
                bytes = f.len();
                if let Resp::Err(m) = r {
                    panic!("{m}")
                }
                if k >= 5 {
                    v.push(d)
                }
            }
            let (p50, p95, _) = summarize(v);
            cols.push((p50, p95, bytes));
        }
        println!("{:<28} {:>5} {:>9.4} {:>9.4} | {:>9.4} {:>9.4} {:>9.4} | {:>9.4} {:>9.4} {:>9.4} | {:>7} {:>7}", name, rows, i50, i95, cols[0].0, cols[0].1, cols[0].0 - i50, cols[1].0, cols[1].1, cols[1].0 - i50, cols[0].2, cols[1].2);
    }
}

fn try_open(db: &str) -> Result<Store, StoreError> {
    Store::open(db)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("hold") => {
            // hold <db> <secs>: open, print open ts, sleep, print release ts, drop
            let s = try_open(&a[2]).expect("open");
            println!("held_open_ns={}", now_ns());
            std::thread::sleep(Duration::from_secs_f64(a[3].parse().unwrap()));
            println!("held_release_ns={}", now_ns());
            drop(s);
        }
        Some("try") => {
            let t = Instant::now();
            match try_open(&a[2]) {
                Ok(_) => println!("try: opened in {:.3} ms", ms(t.elapsed())),
                Err(e) => println!(
                    "try: FAILED after {:.3} ms: {:?} | display: {}",
                    ms(t.elapsed()),
                    e,
                    e
                ),
            }
        }
        Some("retry") => {
            // retry <db> <base_ms> <cap_ms> <timeout_s> <mode: jitter|fixed>
            let (base, cap, to): (f64, f64, f64) = (
                a[3].parse().unwrap(),
                a[4].parse().unwrap(),
                a[5].parse().unwrap(),
            );
            let jitter = a.get(6).map(String::as_str) != Some("fixed");
            let mut r = rng();
            let t = Instant::now();
            let (mut attempts, mut delay) = (0, base);
            loop {
                attempts += 1;
                match try_open(&a[2]) {
                    Ok(s) => {
                        println!(
                            "retry_ok_ns={} attempts={} waited_ms={:.1}",
                            now_ns(),
                            attempts,
                            ms(t.elapsed())
                        );
                        drop(s);
                        break;
                    }
                    Err(StoreError::Locked(_)) => {
                        if t.elapsed().as_secs_f64() > to {
                            println!("retry_TIMEOUT attempts={attempts}");
                            std::process::exit(2)
                        }
                        let sleep = if jitter {
                            (r() % 1000) as f64 / 1000.0 * delay
                        } else {
                            delay
                        };
                        std::thread::sleep(Duration::from_secs_f64(sleep / 1000.0));
                        if jitter {
                            delay = (delay * 2.0).min(cap)
                        }
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
        Some("storm-worker") => {
            // storm-worker <db> <iters> <base_ms> <cap_ms>: each iter = retry-open, search, close
            let iters: usize = a[3].parse().unwrap();
            let (base, cap): (f64, f64) = (a[4].parse().unwrap(), a[5].parse().unwrap());
            let mut r = rng();
            let (mut lat, mut tot_attempts, mut maxa) = (vec![], 0u64, 0u64);
            for _ in 0..iters {
                let t = Instant::now();
                let (mut attempts, mut delay) = (0u64, base);
                let s = loop {
                    attempts += 1;
                    match try_open(&a[2]) {
                        Ok(s) => break s,
                        Err(StoreError::Locked(_)) => {
                            std::thread::sleep(Duration::from_secs_f64(
                                (r() % 1000) as f64 / 1000.0 * delay / 1000.0,
                            ));
                            delay = (delay * 2.0).min(cap);
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                let mut q = Query::new("self");
                q.limit = Some(100);
                let _ = s.search(&q).unwrap();
                drop(s);
                lat.push(ms(t.elapsed()));
                tot_attempts += attempts;
                maxa = maxa.max(attempts);
            }
            let n = lat.len();
            let mx = lat.iter().cloned().fold(0.0, f64::max);
            let (p50, p95, _) = summarize(lat);
            println!("worker pid={} n={n} p50={p50:.1}ms p95={p95:.1}ms max={mx:.1}ms attempts_total={tot_attempts} attempts_max={maxa}", std::process::id());
        }
        Some("search-once") => {
            // process-level: open + search + close timing, one process
            let t0 = Instant::now();
            let s = try_open(&a[2]).expect("open");
            let t1 = Instant::now();
            let mut q = Query::new("self");
            q.limit = Some(100);
            let h = s.search(&q).unwrap();
            let t2 = Instant::now();
            drop(s);
            println!(
                "open={:.1}ms search={:.2}ms close={:.1}ms hits={} (lock held {:.1}ms)",
                ms(t1 - t0),
                ms(t2 - t1),
                ms(t2.elapsed()),
                h.len(),
                ms(t0.elapsed())
            );
        }
        Some("serve") => serve(
            &a[2],
            &a[3],
            if a[4] == "bin" { Enc::Bin } else { Enc::Json },
        ),
        Some("bench") => bench(
            &a[2],
            &a[3],
            a.get(4).map(String::as_str).filter(|s| *s != "-"),
            a.get(5).map(String::as_str),
        ),
        _ => eprintln!("usage: hold|try|retry|storm-worker|search-once|serve|bench"),
    }
}
