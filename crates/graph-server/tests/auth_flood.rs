//! ADR 0012 D10 (issue #300), the refusal flood: 10,000 calls with a bad
//! token produce a bounded number of log lines (the per-peer rate limit)
//! and exactly 10,000 more `mg_auth_refusals_total`, and a valid caller's
//! p99 latency while the flood runs stays within 2x of its unloaded
//! baseline. Deviation from ADR 0012's strict 2x: the bound is
//! max(2 x baseline, baseline + 3 ms), because the loopback baseline is
//! under a millisecond, where scheduler noise alone can double a p99.
//!
//! Its own test binary: it installs the process-wide subscriber.
use graph_core::Extractor;
use graph_server::testing::auth::AuthFixture;
use graph_server::testing::TestServer;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FLOOD: u64 = 10_000;
/// Calls the flood keeps in flight at once.
const FLOOD_CONCURRENCY: usize = 8;
/// Absorbs scheduler noise at sub-millisecond baselines.
const SLACK: Duration = Duration::from_millis(3);

fn rust() -> Vec<Box<dyn Extractor>> {
    vec![Box::new(graph_lang_rust::RustExtractor)]
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type Chan = tonic::transport::Channel;

async fn channel(ep: &str) -> Chan {
    tonic::transport::Endpoint::from_shared(format!("http://{ep}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

/// One `Admin.Status` call with `token`; its status code.
async fn status(ch: Chan, token: &str) -> tonic::Code {
    let mut c =
        graph_proto::pb::admin_client::AdminClient::with_interceptor(ch, graph_proto::SendVersion);
    let mut req = tonic::Request::new(graph_proto::pb::StatusRequest::default());
    req.metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    match c.status(req).await {
        Ok(_) => tonic::Code::Ok,
        Err(st) => st.code(),
    }
}

fn p99(mut v: Vec<Duration>) -> Duration {
    assert!(!v.is_empty());
    v.sort();
    v[(v.len() * 99 / 100).min(v.len() - 1)]
}

#[test]
fn a_flood_of_bad_tokens_is_counted_logs_little_and_does_not_starve_valid_callers() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_ansi(false)
        .with_writer(move || w.clone())
        .init();
    let refusal_lines = || {
        String::from_utf8_lossy(&buf.0.lock().unwrap())
            .matches("auth: call refused")
            .count()
    };

    let f = AuthFixture::new();
    let d = tempfile::tempdir().unwrap();
    let ts = TestServer::start_with(&d.path().join("g.redb"), rust(), |c| c.auth = f.config());
    let ep = ts.endpoint();
    let auth = ts.running().unwrap().auth();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    // The valid caller's own connection, warmed up, then its baseline.
    let valid = rt.block_on(channel(&ep));
    let sample = |n: usize| -> Vec<Duration> {
        let ch = valid.clone();
        let token = f.read.clone();
        rt.block_on(async move {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                let t = Instant::now();
                assert_eq!(status(ch.clone(), &token).await, tonic::Code::Ok);
                out.push(t.elapsed());
            }
            out
        })
    };
    sample(100);
    let baseline = p99(sample(500));

    let before: u64 = auth.refusal_counts().iter().map(|(_, n)| n).sum();
    let lines_before = refusal_lines();
    let started = Instant::now();
    // The flood: its own connection, FLOOD_CONCURRENCY calls in flight.
    let flood = {
        let ep = ep.clone();
        let bad = f.expired.clone();
        rt.spawn(async move {
            let ch = channel(&ep).await;
            let next = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let mut tasks = Vec::new();
            for _ in 0..FLOOD_CONCURRENCY {
                let (ch, bad, next) = (ch.clone(), bad.clone(), Arc::clone(&next));
                tasks.push(tokio::spawn(async move {
                    while next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < FLOOD {
                        assert_eq!(status(ch.clone(), &bad).await, tonic::Code::Unauthenticated);
                    }
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
        })
    };
    // The valid caller while the flood runs.
    let mut during = Vec::new();
    while !flood.is_finished() {
        during.extend(sample(20));
    }
    rt.block_on(flood).unwrap();
    let elapsed = started.elapsed();
    let loaded = p99(during.clone());

    let after: u64 = auth.refusal_counts().iter().map(|(_, n)| n).sum();
    assert_eq!(after - before, FLOOD, "every refusal is counted");
    let logged = refusal_lines() - lines_before;
    let windows = (elapsed.as_secs() / graph_server::auth::REFUSAL_LOG_WINDOW.as_secs()) as usize;
    assert!(
        logged <= windows + 2,
        "{logged} refusal lines for {FLOOD} refusals in {elapsed:?} (rate limit: one per peer per \
         window)"
    );
    eprintln!(
        "flood of {FLOOD} in {elapsed:?}: valid p99 {baseline:?} unloaded, {loaded:?} during \
         ({} samples), {logged} log lines",
        during.len()
    );
    assert!(during.len() >= 20, "the valid caller ran during the flood");
    assert!(
        loaded <= (baseline * 2).max(baseline + SLACK),
        "valid p99 {loaded:?} during the flood vs {baseline:?} unloaded"
    );
}
