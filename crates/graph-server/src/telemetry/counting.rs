//! Export accounting (ADR 0009 D8, #250): `mg_otel_export_failures_total`
//! and `mg_otel_dropped_total`, per signal.
//!
//! SDK 0.33's batch processors drop on a full queue without telling
//! anyone. So the queue bound is enforced here, in front of them: a
//! [`CountingSpanProcessor`] / [`CountingLogProcessor`] admits an item only
//! while fewer than `max_queue_size` items are pending (admitted and not
//! yet handed to the exporter), and counts the rest as dropped. The SDK's
//! own channel holds at most the pending items, so it never fills and
//! never drops silently. The exporter decorators release pending items
//! when a batch is handed over and count the batch's outcome: a failed or
//! timed-out export is one failure and its items are dropped.
//!
//! So once nothing is pending: `queued = exported + dropped`.
use opentelemetry_sdk::error::OTelSdkResult;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The three signals, in `/metrics` order.
pub const SIGNALS: [&str; 3] = ["traces", "metrics", "logs"];

/// `# HELP` of `mg_otel_export_failures_total`.
pub const EXPORT_FAILURES_HELP: &str =
    "OTLP export calls that failed or timed out, by signal (traces, metrics, logs).";
/// `# HELP` of `mg_otel_dropped_total`.
pub const DROPPED_HELP: &str = "Spans, log records or metric data points never exported, by signal: those in failed or timed-out exports, and those refused by a full batch queue.";

/// The least time between two export-error log lines of one signal.
const ERROR_LOG_EVERY: Duration = Duration::from_secs(60);

struct Global {
    failures: AtomicU64,
    dropped: AtomicU64,
}

static GLOBAL: [Global; 3] = [
    Global {
        failures: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    },
    Global {
        failures: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    },
    Global {
        failures: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    },
];

/// One signal's process-wide counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtelCounter {
    pub signal: &'static str,
    pub failures: u64,
    pub dropped: u64,
}

/// The process-wide counters behind the two `/metrics` families (0 while
/// OTLP is off).
pub fn otel_counters() -> [OtelCounter; 3] {
    std::array::from_fn(|i| OtelCounter {
        signal: SIGNALS[i],
        failures: GLOBAL[i].failures.load(Ordering::Relaxed),
        dropped: GLOBAL[i].dropped.load(Ordering::Relaxed),
    })
}

/// One pipeline's counts, for tests (`queued = exported + dropped` once
/// nothing is pending).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineCounts {
    /// Items offered to the pipeline (sampled spans, log records, metric
    /// data points collected).
    pub queued: u64,
    /// Items in exports that succeeded.
    pub exported: u64,
    /// Failed or timed-out export calls.
    pub failures: u64,
    /// Items refused by the queue or lost with a failed export.
    pub dropped: u64,
    /// Items admitted and not yet handed to the exporter.
    pub pending: u64,
}

/// Apply `f` atomically (a compare-and-swap loop); `false` when `f`
/// declines.
fn update(a: &AtomicU64, f: impl Fn(u64) -> Option<u64>) -> bool {
    let mut current = a.load(Ordering::SeqCst);
    loop {
        let Some(next) = f(current) else { return false };
        match a.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => current = seen,
        }
    }
}

/// One signal pipeline's accounting, shared by its processor and exporter.
#[derive(Debug)]
pub(crate) struct Accounting {
    signal: usize,
    /// Whether the process-wide counters follow this pipeline (not in
    /// unit tests).
    global: bool,
    cap: u64,
    queued: AtomicU64,
    exported: AtomicU64,
    failures: AtomicU64,
    dropped: AtomicU64,
    pending: AtomicU64,
    closed: AtomicBool,
    last_error_log: Mutex<Option<Instant>>,
}

impl Accounting {
    /// `signal`: an index into [`SIGNALS`]; `cap`: the queue bound.
    pub(crate) fn new(signal: usize, cap: usize) -> Arc<Self> {
        Arc::new(Self {
            signal,
            global: true,
            cap: cap.max(1) as u64,
            queued: AtomicU64::new(0),
            exported: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            last_error_log: Mutex::new(None),
        })
    }

    pub(crate) fn counts(&self) -> PipelineCounts {
        PipelineCounts {
            queued: self.queued.load(Ordering::SeqCst),
            exported: self.exported.load(Ordering::SeqCst),
            failures: self.failures.load(Ordering::SeqCst),
            dropped: self.dropped.load(Ordering::SeqCst),
            pending: self.pending.load(Ordering::SeqCst),
        }
    }

    fn drop_items(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::SeqCst);
        if self.global {
            GLOBAL[self.signal].dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// One item offered: whether it may go on to the batch processor.
    fn admit(&self) -> bool {
        self.queued.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.drop_items(1);
            return false;
        }
        let admitted = update(&self.pending, |p| (p < self.cap).then_some(p + 1));
        if !admitted {
            self.drop_items(1);
        }
        admitted
    }

    /// After the processor's shutdown returned: items never handed to the
    /// exporter (a shutdown that timed out) are dropped. Should the
    /// abandoned worker still export them later, they are counted again
    /// (as exported or dropped): a known, shutdown-only over-count.
    fn abandon_pending(&self) {
        let n = self.pending.swap(0, Ordering::SeqCst);
        if n > 0 {
            self.drop_items(n);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// `n` admitted items were handed to the exporter.
    fn handed(&self, n: u64) {
        update(&self.pending, |p| Some(p.saturating_sub(n)));
    }

    /// `n` items were collected straight into an export (metrics: no
    /// queue).
    pub(crate) fn offered(&self, n: u64) {
        self.queued.fetch_add(n, Ordering::SeqCst);
    }

    /// The outcome of an export of `n` items.
    pub(crate) fn finished(&self, n: u64, result: &OTelSdkResult) {
        match result {
            Ok(()) => {
                self.exported.fetch_add(n, Ordering::SeqCst);
            }
            Err(e) => {
                self.failures.fetch_add(1, Ordering::SeqCst);
                if self.global {
                    GLOBAL[self.signal].failures.fetch_add(1, Ordering::Relaxed);
                }
                self.drop_items(n);
                self.log_error(e);
            }
        }
    }

    /// At most one line per signal per [`ERROR_LOG_EVERY`] (D8).
    fn log_error(&self, e: &opentelemetry_sdk::error::OTelSdkError) {
        let mut last = self
            .last_error_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < ERROR_LOG_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        tracing::warn!(
            signal = SIGNALS[self.signal],
            error = %e,
            "OpenTelemetry export failed (logged at most once a minute per signal)"
        );
    }
}

/// A span processor that enforces the queue bound and counts drops.
#[derive(Debug)]
pub(crate) struct CountingSpanProcessor<P> {
    pub(crate) inner: P,
    pub(crate) acct: Arc<Accounting>,
}

impl<P: opentelemetry_sdk::trace::SpanProcessor> opentelemetry_sdk::trace::SpanProcessor
    for CountingSpanProcessor<P>
{
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
        // The batch processor ignores unsampled spans: so do the counts.
        if span.span_context.is_sampled() && self.acct.admit() {
            self.inner.on_end(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.acct.close();
        let r = self.inner.shutdown_with_timeout(timeout);
        self.acct.abandon_pending();
        r
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

/// A span exporter that counts each export's outcome.
#[derive(Debug)]
pub(crate) struct CountingSpanExporter<E> {
    pub(crate) inner: E,
    pub(crate) acct: Arc<Accounting>,
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for CountingSpanExporter<E>
{
    fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let n = batch.len() as u64;
        self.acct.handed(n);
        let fut = self.inner.export(batch);
        async move {
            let r = fut.await;
            self.acct.finished(n, &r);
            r
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

/// A log processor that enforces the queue bound and counts drops.
#[derive(Debug)]
pub(crate) struct CountingLogProcessor<P> {
    pub(crate) inner: P,
    pub(crate) acct: Arc<Accounting>,
}

impl<P: opentelemetry_sdk::logs::LogProcessor> opentelemetry_sdk::logs::LogProcessor
    for CountingLogProcessor<P>
{
    fn emit(
        &self,
        data: &mut opentelemetry_sdk::logs::SdkLogRecord,
        instrumentation: &opentelemetry::InstrumentationScope,
    ) {
        if self.acct.admit() {
            self.inner.emit(data, instrumentation);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.acct.close();
        let r = self.inner.shutdown_with_timeout(timeout);
        self.acct.abandon_pending();
        r
    }

    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

/// A log exporter that counts each export's outcome.
#[derive(Debug)]
pub(crate) struct CountingLogExporter<E> {
    pub(crate) inner: E,
    pub(crate) acct: Arc<Accounting>,
}

impl<E: opentelemetry_sdk::logs::LogExporter> opentelemetry_sdk::logs::LogExporter
    for CountingLogExporter<E>
{
    fn export(
        &self,
        batch: opentelemetry_sdk::logs::LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let n = batch.iter().count() as u64;
        self.acct.handed(n);
        let fut = self.inner.export(batch);
        async move {
            let r = fut.await;
            self.acct.finished(n, &r);
            r
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Many threads at once: the bound holds exactly and every item is
    /// either admitted or dropped.
    #[test]
    fn admit_holds_the_bound_under_contention() {
        let mut acct = Accounting::new(0, 64);
        Arc::get_mut(&mut acct).expect("unshared").global = false;
        let admitted = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..2_000 {
                        if acct.admit() {
                            admitted.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        let c = acct.counts();
        assert_eq!(c.queued, 16_000);
        assert_eq!(admitted.load(Ordering::SeqCst), 64, "exactly the bound");
        assert_eq!(c.pending, 64);
        assert_eq!(c.dropped, 16_000 - 64);
        acct.abandon_pending();
        assert_eq!(acct.counts().dropped, 16_000);
    }

    #[test]
    fn the_queue_bound_drops_and_counts() {
        let mut acct = Accounting::new(0, 2);
        Arc::get_mut(&mut acct).expect("unshared").global = false;
        assert!(acct.admit());
        assert!(acct.admit());
        assert!(!acct.admit(), "the third waits for room: dropped");
        acct.handed(2);
        acct.finished(2, &Ok(()));
        assert!(acct.admit());
        acct.handed(1);
        acct.finished(
            1,
            &Err(opentelemetry_sdk::error::OTelSdkError::Timeout(
                Duration::from_secs(1),
            )),
        );
        acct.close();
        assert!(!acct.admit(), "closed");
        let c = acct.counts();
        assert_eq!(
            c,
            PipelineCounts {
                queued: 5,
                exported: 2,
                failures: 1,
                dropped: 3,
                pending: 0
            }
        );
        assert_eq!(c.queued, c.exported + c.dropped);
    }
}
