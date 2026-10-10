//! Traces (ADR 0009 D5, D8, D9; epic story 51).
//!
//! * [`tracing_layer`]: the `tracing-opentelemetry` layer `serve` installs
//!   when traces are on, behind its own per-layer filter: spans only (events
//!   are never exported as span events, D9), from this workspace's crates
//!   only, and never the Raft service's `rpc` spans ([`RAFT_RPC_TARGET`]),
//!   so heartbeats and AppendEntries do not reach the collector.
//! * [`TraceExporter`]: wraps the OTLP span exporter. It counts failed and
//!   timed-out exports ([`export_failures`], [`dropped`]), bounds every export
//!   by the export timeout on the exporter's own runtime, and keeps the
//!   resource's `service.instance.id` and `memory_graph.cluster` current
//!   ([`set_node_identity`]): a node's first start learns them only inside
//!   `graph_server::start` (#249), after the provider was built.
//! * [`ApplyLinks`]: joins the leader's `apply` span to the `rpc` span that
//!   proposed the entry. The apply runs on the state machine's task, not on
//!   the request's, so it is a span link carrying the log index (D5), not a
//!   child; nothing is added to the Raft log.
use opentelemetry::trace::SpanContext;
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// The target of the Raft service's `rpc` spans: logged like every other
/// rpc span, never exported (an idle cluster would flood the collector).
pub const RAFT_RPC_TARGET: &str = "memory_graph::raft_rpc";

/// The signals the failure counters are kept for (the `signal` label).
pub const SIGNALS: [&str; 3] = ["traces", "metrics", "logs"];

static TRACES_ON: AtomicBool = AtomicBool::new(false);
static FAILURES: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static DROPPED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static LAST_WARNED: Mutex<[Option<Instant>; 3]> = Mutex::new([None, None, None]);
/// Export errors are logged at most once per this per signal (D8).
const WARN_EVERY: Duration = Duration::from_secs(60);

/// Whether this process installed the OpenTelemetry tracing layer: the
/// cheap check before any trace-only work (hashing a proposal for
/// [`ApplyLinks`]).
pub fn traces_on() -> bool {
    TRACES_ON.load(Ordering::Relaxed)
}

/// Export calls for `signal` (`traces`, `metrics`, `logs`) that failed or
/// timed out since the process started (`mg_otel_export_failures_total`).
pub fn export_failures(signal: &str) -> u64 {
    index_of(signal).map_or(0, |i| FAILURES[i].load(Ordering::Relaxed))
}

/// Items carried by those failed exports (`mg_otel_dropped_total`). Items
/// the SDK's batch queue drops when full are not seen here (#250).
pub fn dropped(signal: &str) -> u64 {
    index_of(signal).map_or(0, |i| DROPPED[i].load(Ordering::Relaxed))
}

fn index_of(signal: &str) -> Option<usize> {
    SIGNALS.iter().position(|s| *s == signal)
}

fn count_export(signal: usize, items: usize, r: &OTelSdkResult) {
    let Err(e) = r else { return };
    FAILURES[signal].fetch_add(1, Ordering::Relaxed);
    DROPPED[signal].fetch_add(items as u64, Ordering::Relaxed);
    let mut last = LAST_WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if last[signal].is_none_or(|t| now.duration_since(t) >= WARN_EVERY) {
        last[signal] = Some(now);
        drop(last);
        tracing::warn!(
            signal = SIGNALS[signal],
            items,
            error = %e,
            "OpenTelemetry export failed; dropped (logged at most once a minute)"
        );
    }
}

/// Whether the OpenTelemetry layer exports this span: spans only, from the
/// workspace's own crates (`graph_*`, `memory_graph::*`), at `debug` or
/// above, and not the Raft service's rpc spans.
pub fn exported(m: &Metadata<'_>) -> bool {
    exports(m.is_span(), m.target(), *m.level())
}

fn exports(is_span: bool, target: &str, level: Level) -> bool {
    is_span
        && target != RAFT_RPC_TARGET
        && (target.starts_with("graph_") || target.starts_with("memory_graph"))
        && level <= Level::DEBUG
}

/// The OpenTelemetry layer for the guard's tracer, behind [`exported`];
/// `None` when traces are not among the signals. Install it once, in the
/// process's global subscriber.
pub fn tracing_layer<S>(
    guard: &super::TelemetryGuard,
) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    use opentelemetry::trace::TracerProvider as _;
    let tracer = guard.tracer_provider()?.tracer("memory-graph");
    TRACES_ON.store(true, Ordering::Relaxed);
    Some(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_location(false)
            .with_threads(false)
            .with_tracked_inactivity(false)
            .with_filter(tracing_subscriber::filter::filter_fn(exported))
            .boxed(),
    )
}

type ClusterFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;
static IDENTITY: Mutex<Option<(u64, ClusterFn)>> = Mutex::new(None);

/// This node's identity once `start` resolved it: the node id, and how to
/// read the cluster id (it may still be adopted later, from the first
/// leader that reaches an uninitialized node). Every trace export from then
/// on carries both in its resource (#249). One node per process: `serve`.
pub fn set_node_identity(
    node_id: u64,
    cluster: impl Fn() -> Option<String> + Send + Sync + 'static,
) {
    *IDENTITY.lock().unwrap_or_else(PoisonError::into_inner) = Some((node_id, Arc::new(cluster)));
}

/// Set by `serve` before it starts: hold exported spans until
/// [`set_node_identity`] (or a shutdown), so even the first spans of a first
/// start (the bootstrap's own applies, a join) carry the node and cluster
/// ids in their resource (#249). Without it (tests, embedded use) spans go
/// out at once.
static HOLD_FOR_IDENTITY: AtomicBool = AtomicBool::new(false);
/// Spans held at most (beyond: dropped and counted, like a full queue).
const HELD_MAX: usize = 8192;

/// See [`HOLD_FOR_IDENTITY`].
pub fn hold_until_identity() {
    HOLD_FOR_IDENTITY.store(true, Ordering::SeqCst);
}

/// Stop holding (a shutdown before the identity was known: export what
/// was held rather than lose it).
pub(super) fn release_hold() {
    HOLD_FOR_IDENTITY.store(false, Ordering::SeqCst);
}

/// Whether spans are held now. The flag and the identity are process
/// globals (one `serve` per process): only `serve` sets the flag, so tests
/// and embedded users never hold; the exporter's own `force_hold` is the
/// test seam that does not touch them.
fn held(force_hold: bool) -> bool {
    (force_hold || HOLD_FOR_IDENTITY.load(Ordering::SeqCst))
        && IDENTITY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
}

fn current_identity() -> Option<(String, Option<String>)> {
    let id = IDENTITY.lock().unwrap_or_else(PoisonError::into_inner);
    let (node, cluster) = id.as_ref()?;
    Some((node.to_string(), cluster()))
}

/// The OTLP span exporter, counted, bounded and with a live identity.
#[derive(Debug)]
pub(super) struct TraceExporter<E> {
    inner: tokio::sync::Mutex<E>,
    /// The resource the provider gave; identity keys are replaced in it.
    base: Mutex<Option<Resource>>,
    /// The identity last written into the inner exporter's resource.
    applied: Mutex<Option<(String, Option<String>)>>,
    timeout: Duration,
    runtime: tokio::runtime::Handle,
    /// Spans held while [`held`] (the node's identity is not known yet).
    pending: Mutex<Vec<SpanData>>,
    /// Tests: hold as if `serve` had asked to, until [`release`].
    force_hold: AtomicBool,
}

impl<E: SpanExporter> TraceExporter<E> {
    /// `runtime` is the exporter's own; the timeout's timer lives there.
    pub(super) fn new(inner: E, timeout: Duration, runtime: tokio::runtime::Handle) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(inner),
            base: Mutex::new(None),
            applied: Mutex::new(None),
            timeout,
            runtime,
            pending: Mutex::new(Vec::new()),
            force_hold: AtomicBool::new(false),
        }
    }

    fn held(&self) -> bool {
        held(self.force_hold.load(Ordering::SeqCst))
    }

    /// Export what is still held (a shutdown or a flush after the hold was
    /// released, when the batch processor has nothing new to send, so it
    /// never calls `export`: a start that failed before the identity was
    /// set, say). Runs on the exporter's runtime, bounded by `timeout`;
    /// whatever cannot be sent is counted as dropped, never lost silently.
    fn drain_pending(&self, timeout: Duration) {
        let batch =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        if batch.is_empty() {
            return;
        }
        let items = batch.len();
        // Blocking on our runtime is only allowed off its workers.
        let inner = match (tokio::runtime::Handle::try_current(), self.inner.try_lock()) {
            (Err(_), Ok(inner)) => inner,
            _ => {
                count_export(
                    0,
                    items,
                    &Err(OTelSdkError::InternalFailure(
                        "held spans not sent at shutdown".into(),
                    )),
                );
                return;
            }
        };
        let resource = self.refreshed();
        let mut inner = inner;
        if let Some(r) = &resource {
            inner.set_resource(r);
        }
        let r = self.runtime.block_on(async {
            match tokio::time::timeout(timeout, inner.export(batch)).await {
                Ok(r) => r,
                Err(_) => Err(OTelSdkError::Timeout(timeout)),
            }
        });
        count_export(0, items, &r);
    }

    /// The resource to switch to, when the identity changed since the last
    /// export.
    fn refreshed(&self) -> Option<Resource> {
        let id = current_identity()?;
        let mut applied = self.applied.lock().unwrap_or_else(PoisonError::into_inner);
        if applied.as_ref() == Some(&id) {
            return None;
        }
        let base = self
            .base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let mut attrs: Vec<KeyValue> = base
            .iter()
            .filter(|(k, _)| {
                k.as_str() != "service.instance.id" && k.as_str() != "memory_graph.cluster"
            })
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
            .collect();
        attrs.push(KeyValue::new("service.instance.id", id.0.clone()));
        if let Some(c) = &id.1 {
            attrs.push(KeyValue::new("memory_graph.cluster", c.clone()));
        }
        *applied = Some(id);
        Some(Resource::builder_empty().with_attributes(attrs).build())
    }
}

impl<E: SpanExporter> SpanExporter for TraceExporter<E> {
    async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
        {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if self.held() {
                let room = HELD_MAX.saturating_sub(pending.len());
                if batch.len() > room {
                    DROPPED[0].fetch_add((batch.len() - room) as u64, Ordering::Relaxed);
                    batch.truncate(room);
                }
                pending.append(&mut batch);
                return Ok(());
            }
            if !pending.is_empty() {
                pending.append(&mut batch);
                batch = std::mem::take(&mut *pending);
            }
        }
        let items = batch.len();
        let resource = self.refreshed();
        // Created on the exporter's runtime (the batch processor polls this
        // on a thread of its own, outside any runtime).
        let deadline = {
            let _entered = self.runtime.enter();
            tokio::time::sleep(self.timeout)
        };
        let r = {
            let mut inner = self.inner.lock().await;
            if let Some(r) = &resource {
                inner.set_resource(r);
            }
            tokio::select! {
                r = inner.export(batch) => r,
                () = deadline => Err(OTelSdkError::Timeout(self.timeout)),
            }
        };
        count_export(0, items, &r);
        r
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        // A shutdown sends what was held, identity or not.
        self.force_hold.store(false, Ordering::SeqCst);
        release_hold();
        self.drain_pending(timeout);
        match self.inner.try_lock() {
            Ok(inner) => inner.shutdown_with_timeout(timeout),
            // An export is still running (a stalled collector): it ends at
            // its own deadline; nothing to wait for here.
            Err(_) => Ok(()),
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        if !self.held() {
            self.drain_pending(self.timeout);
        }
        match self.inner.try_lock() {
            Ok(inner) => inner.force_flush(),
            Err(_) => Ok(()),
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        *self.base.get_mut().unwrap_or_else(PoisonError::into_inner) = Some(resource.clone());
        *self
            .applied
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.inner.get_mut().set_resource(resource);
    }
}

/// Proposals in flight on this node that came from a traced request: the
/// hash of the proposed bytes -> the request's `rpc` span. The leader's
/// apply of those bytes links to it ([`link`](Self::link)). Empty, and
/// never hashed, unless traces are on.
///
/// Two proposals of identical bytes in flight at once share a key: both
/// requests are kept, and an apply of those bytes links to every one of
/// them (it cannot tell which proposal it is; they are the same write).
#[derive(Debug, Default)]
pub struct ApplyLinks {
    map: Mutex<HashMap<u64, Vec<SpanContext>>>,
    live: AtomicUsize,
}

/// Removes its proposal from [`ApplyLinks`] when the proposal is answered.
pub struct LinkGuard<'a> {
    links: &'a ApplyLinks,
    key: u64,
    rpc: SpanContext,
}

impl Drop for LinkGuard<'_> {
    fn drop(&mut self) {
        let mut m = self
            .links
            .map
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(v) = m.get_mut(&self.key) {
            if let Some(i) = v.iter().position(|c| *c == self.rpc) {
                v.swap_remove(i);
                self.links.live.fetch_sub(1, Ordering::Relaxed);
            }
            if v.is_empty() {
                m.remove(&self.key);
            }
        }
    }
}

fn key_of(payload: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut h);
    h.finish()
}

impl ApplyLinks {
    /// Note that `payload` is being proposed for the request whose `rpc`
    /// span has context `rpc`; `None` (nothing to link) when traces are off
    /// or the context is not valid.
    pub fn register(&self, payload: &[u8], rpc: SpanContext) -> Option<LinkGuard<'_>> {
        if !traces_on() || !rpc.is_valid() {
            return None;
        }
        let key = key_of(payload);
        let mut m = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        m.entry(key).or_default().push(rpc.clone());
        self.live.fetch_add(1, Ordering::Relaxed);
        Some(LinkGuard {
            links: self,
            key,
            rpc,
        })
    }

    /// Link `span` (an `apply`) to the request that proposed `payload` at
    /// log index `index`, when one is waiting on this node.
    pub fn link(&self, span: &tracing::Span, payload: &[u8], index: u64) {
        if self.live.load(Ordering::Relaxed) == 0 {
            return;
        }
        let found = self
            .map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key_of(payload))
            .cloned()
            .unwrap_or_default();
        for rpc in found {
            use tracing_opentelemetry::OpenTelemetrySpanExt;
            span.add_link_with_attributes(
                rpc,
                vec![KeyValue::new("memory_graph.log_index", index as i64)],
            );
        }
    }
}

tokio::task_local! {
    /// The `rpc` span of the request this task serves (set by
    /// `observe::RpcService`): what a proposal made on its behalf links to.
    pub static RPC_SPAN: tracing::Span;
}

/// The OpenTelemetry context of the current request's `rpc` span; invalid
/// outside a request or with traces off.
pub fn current_rpc_context() -> SpanContext {
    if !traces_on() {
        return SpanContext::empty_context();
    }
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    RPC_SPAN
        .try_with(|s| s.context().span().span_context().clone())
        .unwrap_or_else(|_| SpanContext::empty_context())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_workspace_spans_outside_the_raft_service_are_exported() {
        assert!(exports(true, "graph_server::observe", Level::INFO));
        assert!(exports(
            true,
            "graph_server::raft::state_machine",
            Level::DEBUG
        ));
        assert!(exports(true, "graph_client::conn", Level::INFO));
        assert!(!exports(true, RAFT_RPC_TARGET, Level::INFO));
        assert!(!exports(false, "graph_server::observe", Level::INFO));
        assert!(!exports(true, "graph_server::observe", Level::TRACE));
        assert!(!exports(true, "openraft::core", Level::INFO));
        assert!(!exports(true, "h2::proto", Level::DEBUG));
    }

    #[test]
    fn failed_exports_are_counted_by_signal() {
        let (f, d) = (export_failures("traces"), dropped("traces"));
        count_export(0, 7, &Ok(()));
        assert_eq!((export_failures("traces"), dropped("traces")), (f, d));
        count_export(0, 7, &Err(OTelSdkError::Timeout(Duration::from_secs(1))));
        assert!(export_failures("traces") > f && dropped("traces") >= d + 7);
        assert_eq!(export_failures("nonsense"), 0);
    }

    #[test]
    fn links_need_traces_on_and_are_dropped_with_their_guard() {
        let links = ApplyLinks::default();
        assert!(links.register(b"x", SpanContext::empty_context()).is_none());
        TRACES_ON.store(true, Ordering::Relaxed);
        use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState};
        let sc = SpanContext::new(
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        );
        let other = SpanContext::new(
            TraceId::from_bytes([3; 16]),
            SpanId::from_bytes([4; 8]),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        );
        {
            let _g = links.register(b"payload", sc.clone()).expect("registered");
            assert_eq!(links.live.load(Ordering::Relaxed), 1);
            {
                // The same bytes proposed twice at once: both kept.
                let _h = links.register(b"payload", other).expect("registered");
                assert_eq!(links.map.lock().unwrap()[&key_of(b"payload")].len(), 2);
            }
            assert_eq!(links.map.lock().unwrap()[&key_of(b"payload")], vec![sc]);
        }
        assert_eq!(links.live.load(Ordering::Relaxed), 0);
        assert!(links.map.lock().unwrap().is_empty());
    }

    /// Records what reaches the wire.
    #[derive(Debug, Default)]
    struct Sink(Arc<Mutex<Vec<String>>>);

    impl SpanExporter for Sink {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0
                .lock()
                .unwrap()
                .extend(batch.into_iter().map(|s| s.name.into_owned()));
            Ok(())
        }
    }

    fn span(name: &'static str) -> SpanData {
        use opentelemetry::trace::{SpanId, SpanKind, Status};
        SpanData {
            span_context: SpanContext::empty_context(),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: name.into(),
            start_time: std::time::SystemTime::now(),
            end_time: std::time::SystemTime::now(),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            events: Default::default(),
            links: Default::default(),
            status: Status::Unset,
            instrumentation_scope: Default::default(),
        }
    }

    /// More than [`HELD_MAX`] spans before the identity is known: the rest
    /// are dropped and counted.
    #[test]
    fn a_hold_overflow_is_counted_as_dropped() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let wire = Arc::new(Mutex::new(Vec::new()));
        let e = TraceExporter::new(
            Sink(Arc::clone(&wire)),
            Duration::from_secs(5),
            rt.handle().clone(),
        );
        e.force_hold.store(true, Ordering::SeqCst);
        let before = dropped("traces");
        for _ in 0..3 {
            let batch = (0..3000).map(|_| span("x")).collect();
            assert!(rt.block_on(e.export(batch)).is_ok());
        }
        assert!(dropped("traces") >= before + (9000 - HELD_MAX) as u64);
        e.shutdown_with_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            wire.lock().unwrap().len(),
            HELD_MAX,
            "what was held is sent"
        );
    }

    /// Review finding: spans held for the identity reach the collector at
    /// shutdown even when nothing else is exported after the hold ends (a
    /// start that failed before its on-ready hook).
    #[test]
    fn held_spans_are_sent_at_shutdown() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let wire = Arc::new(Mutex::new(Vec::new()));
        let e = TraceExporter::new(
            Sink(Arc::clone(&wire)),
            Duration::from_secs(5),
            rt.handle().clone(),
        );
        e.force_hold.store(true, Ordering::SeqCst);
        let r = rt.block_on(e.export(vec![span("a"), span("b")]));
        assert!(r.is_ok());
        assert!(wire.lock().unwrap().is_empty(), "held");
        // A flush while still held sends nothing.
        e.force_flush().unwrap();
        assert!(wire.lock().unwrap().is_empty());
        let failures = export_failures("traces");
        e.shutdown_with_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(*wire.lock().unwrap(), ["a", "b"]);
        assert_eq!(export_failures("traces"), failures, "sent, not dropped");
    }
}
