//! Metrics over OTLP (ADR 0009 D6, story 52).
//!
//! Every `/metrics` family but the two duration histograms is an observable
//! instrument whose callback copies points out of one cached
//! [`MetricsSnapshot`]. The snapshot is built once per collection, before
//! it, by the reader's own thread ([`OtlpReader`]), so no callback ever
//! builds one: building reads the Raft log store (a redb read transaction),
//! and callbacks only take a short lock on the cached `Arc`.
//!
//! The reader is a `ManualReader` driven by a thread of ours at
//! `--otlp-metrics-interval` rather than the SDK's `PeriodicReader`, whose
//! loop offers no hook before its collection. Each tick: refresh the
//! snapshot, collect, export through the OTLP exporter (on the telemetry
//! runtime), count the outcome ([`super::counting`]).
//!
//! The two histograms are synchronous instruments ([`OtlpRecorders`])
//! recorded where `/metrics` records them, with the [`DURATION_BUCKETS`]
//! bounds.
use super::counting::Accounting;
use super::{MetricMapping, OtelInstrument, METRIC_MAPPING};
use crate::observe::{MetricsSnapshot, SampleValue, DURATION_BUCKETS};
use opentelemetry::metrics::{Histogram, Meter, MeterProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, Pipeline, SdkMeterProvider, Temporality,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

/// Builds the node's snapshot; `None` once the node is gone (the last
/// snapshot is kept for the final export).
pub type SnapshotSource = Box<dyn Fn() -> Option<MetricsSnapshot> + Send + Sync>;

/// The snapshot the callbacks read, and how often it was built.
#[derive(Default)]
pub(crate) struct SnapshotCache {
    source: Mutex<Option<SnapshotSource>>,
    current: Mutex<Option<Arc<MetricsSnapshot>>>,
    builds: AtomicU64,
    collections: AtomicU64,
}

impl SnapshotCache {
    /// Before a collection: build the snapshot once.
    fn refresh(&self) {
        let built = {
            let source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
            // Collections count from the moment a node is attached.
            if source.is_some() {
                self.collections.fetch_add(1, Ordering::SeqCst);
            }
            source.as_ref().and_then(|f| f())
        };
        if let Some(snapshot) = built {
            self.builds.fetch_add(1, Ordering::SeqCst);
            *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(snapshot));
        }
    }

    fn current(&self) -> Option<Arc<MetricsSnapshot>> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Snapshot builds and collections so far (equal while a node is
/// attached: one build per collection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionCounts {
    pub collections: u64,
    pub builds: u64,
}

struct ReaderShared {
    manual: ManualReader,
    exporter: opentelemetry_otlp::MetricExporter,
    cache: Arc<SnapshotCache>,
    acct: Arc<Accounting>,
    runtime: tokio::runtime::Handle,
    /// `true` once shut down; the ticking thread waits on `wake`.
    stopped: Mutex<bool>,
    wake: Condvar,
    /// One collect-and-export at a time.
    exporting: Mutex<()>,
}

impl ReaderShared {
    fn collect_and_export(&self) -> OTelSdkResult {
        let _one = self
            .exporting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.cache.refresh();
        let mut rm = ResourceMetrics::default();
        self.manual.collect(&mut rm)?;
        let points = data_points(&rm);
        if points == 0 {
            return Ok(());
        }
        self.acct.offered(points);
        let result = self.runtime.block_on(self.exporter.export(&rm));
        self.acct.finished(points, &result);
        result
    }
}

/// The metric reader: a `ManualReader` plus a ticking thread.
#[derive(Clone)]
pub(crate) struct OtlpReader(Arc<ReaderShared>);

impl std::fmt::Debug for OtlpReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtlpReader")
    }
}

impl OtlpReader {
    /// The reader, and its thread ticking every `interval`.
    pub(crate) fn start(
        exporter: opentelemetry_otlp::MetricExporter,
        interval: Duration,
        cache: Arc<SnapshotCache>,
        acct: Arc<Accounting>,
        runtime: tokio::runtime::Handle,
    ) -> Result<OtlpReader, std::io::Error> {
        let shared = Arc::new(ReaderShared {
            manual: ManualReader::builder()
                .with_temporality(Temporality::Cumulative)
                .build(),
            exporter,
            cache,
            acct,
            runtime,
            stopped: Mutex::new(false),
            wake: Condvar::new(),
            exporting: Mutex::new(()),
        });
        let weak: Weak<ReaderShared> = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("mg-otel-metrics".into())
            .spawn(move || tick(&weak, interval))?;
        Ok(OtlpReader(shared))
    }
}

fn tick(weak: &Weak<ReaderShared>, interval: Duration) {
    loop {
        let Some(shared) = weak.upgrade() else { return };
        let stopped = shared
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (stopped, _) = shared
            .wake
            .wait_timeout_while(stopped, interval, |s| !*s)
            .unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            return;
        }
        drop(stopped);
        // Failures are counted (and logged, throttled) by the accounting.
        let _ = shared.collect_and_export();
    }
}

impl MetricReader for OtlpReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.manual.register_pipeline(pipeline);
    }

    fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.manual.collect(rm)
    }

    /// Blocks until the export ends: not for async code.
    fn force_flush(&self) -> OTelSdkResult {
        if *self
            .0
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(OTelSdkError::AlreadyShutdown);
        }
        self.0.collect_and_export()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        {
            let mut stopped = self
                .0
                .stopped
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *stopped {
                return Err(OTelSdkError::AlreadyShutdown);
            }
            *stopped = true;
        }
        self.0.wake.notify_all();
        // The final export (bounded by the exporter's own timeout).
        let last = self.0.collect_and_export();
        let _ = self.0.exporter.shutdown_with_timeout(timeout);
        let _ = self.0.manual.shutdown_with_timeout(timeout);
        last
    }

    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.manual.temporality(kind)
    }
}

/// Data points in one collection.
fn data_points(rm: &ResourceMetrics) -> u64 {
    fn of<T>(d: &MetricData<T>) -> usize {
        match d {
            MetricData::Gauge(g) => g.data_points().count(),
            MetricData::Sum(s) => s.data_points().count(),
            MetricData::Histogram(h) => h.data_points().count(),
            MetricData::ExponentialHistogram(h) => h.data_points().count(),
        }
    }
    rm.scope_metrics()
        .flat_map(|s| s.metrics())
        .map(|m| match m.data() {
            AggregatedMetrics::F64(d) => of(d),
            AggregatedMetrics::U64(d) => of(d),
            AggregatedMetrics::I64(d) => of(d),
        } as u64)
        .sum()
}

/// OTLP metrics, handed to the server so it can attach its node
/// ([`OtlpMetrics::attach`]). Cheap to clone.
#[derive(Clone)]
pub struct OtlpMetrics {
    pub(crate) provider: SdkMeterProvider,
    pub(crate) cache: Arc<SnapshotCache>,
    recorders: Arc<OnceLock<OtlpRecorders>>,
}

impl std::fmt::Debug for OtlpMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtlpMetrics")
    }
}

impl OtlpMetrics {
    pub(crate) fn new(provider: SdkMeterProvider, cache: Arc<SnapshotCache>) -> Self {
        Self {
            provider,
            cache,
            recorders: Arc::new(OnceLock::new()),
        }
    }

    /// Export `source`'s snapshots from now on, replacing any earlier
    /// source. The instruments are registered on the first call; every
    /// call returns the same histogram recorders.
    pub fn attach(&self, source: SnapshotSource) -> OtlpRecorders {
        *self
            .cache
            .source
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(source);
        self.recorders
            .get_or_init(|| {
                let meter = self.provider.meter("memory-graph");
                register_observables(&meter, &self.cache);
                OtlpRecorders::new(&meter)
            })
            .clone()
    }

    /// Collect and export now (tests; the reader also does it on its own).
    /// Blocks on the telemetry runtime: never call it (or the provider's
    /// `force_flush`) from async code; use `spawn_blocking`.
    #[doc(hidden)]
    pub fn collect_now(&self) -> OTelSdkResult {
        self.provider.force_flush()
    }

    /// Collections and snapshot builds so far.
    #[doc(hidden)]
    pub fn collection_counts(&self) -> CollectionCounts {
        CollectionCounts {
            collections: self.cache.collections.load(Ordering::SeqCst),
            builds: self.cache.builds.load(Ordering::SeqCst),
        }
    }
}

/// The points of the family `prometheus` in the cached snapshot, with the
/// labels as attributes.
fn points(cache: &SnapshotCache, prometheus: &str) -> Vec<(SampleValue, Vec<KeyValue>)> {
    let Some(snapshot) = cache.current() else {
        return Vec::new();
    };
    let Some(family) = snapshot.family(prometheus) else {
        return Vec::new();
    };
    family
        .samples
        .iter()
        .map(|s| {
            let attrs = s
                .labels
                .iter()
                .map(|(k, v)| KeyValue::new(*k, v.clone()))
                .collect();
            (s.value.clone(), attrs)
        })
        .collect()
}

fn as_u64(v: &SampleValue) -> Option<u64> {
    match v {
        SampleValue::Int(n) => Some(*n),
        SampleValue::Float(f) => Some(*f as u64),
        SampleValue::Histogram(_) => None,
    }
}

fn as_f64(v: &SampleValue) -> Option<f64> {
    match v {
        SampleValue::Int(n) => Some(*n as f64),
        SampleValue::Float(f) => Some(*f),
        SampleValue::Histogram(_) => None,
    }
}

/// Whether the instrument carries floats: seconds do (`s`), the rest are
/// whole numbers.
fn is_float(m: &MetricMapping) -> bool {
    m.unit == "s"
}

/// One observable instrument per snapshot family in [`METRIC_MAPPING`].
/// The SDK keeps each callback registered for the provider's life.
fn register_observables(meter: &Meter, cache: &Arc<SnapshotCache>) {
    for m in METRIC_MAPPING.iter() {
        let Some(otel) = m.otel else { continue };
        let prom = m.prometheus;
        let cache = Arc::clone(cache);
        match (m.instrument, is_float(m)) {
            (OtelInstrument::Histogram, _) => {}
            (OtelInstrument::Gauge, false) => {
                meter
                    .u64_observable_gauge(otel)
                    .with_unit(m.unit)
                    .with_callback(move |o| {
                        for (v, a) in points(&cache, prom) {
                            if let Some(v) = as_u64(&v) {
                                o.observe(v, &a);
                            }
                        }
                    })
                    .build();
            }
            (OtelInstrument::Gauge, true) => {
                meter
                    .f64_observable_gauge(otel)
                    .with_unit(m.unit)
                    .with_callback(move |o| {
                        for (v, a) in points(&cache, prom) {
                            if let Some(v) = as_f64(&v) {
                                o.observe(v, &a);
                            }
                        }
                    })
                    .build();
            }
            (OtelInstrument::UpDownCounter, _) => {
                meter
                    .i64_observable_up_down_counter(otel)
                    .with_unit(m.unit)
                    .with_callback(move |o| {
                        for (v, a) in points(&cache, prom) {
                            if let Some(v) = as_u64(&v) {
                                o.observe(i64::try_from(v).unwrap_or(i64::MAX), &a);
                            }
                        }
                    })
                    .build();
            }
            (OtelInstrument::Counter, false) => {
                meter
                    .u64_observable_counter(otel)
                    .with_unit(m.unit)
                    .with_callback(move |o| {
                        for (v, a) in points(&cache, prom) {
                            if let Some(v) = as_u64(&v) {
                                o.observe(v, &a);
                            }
                        }
                    })
                    .build();
            }
            (OtelInstrument::Counter, true) => {
                meter
                    .f64_observable_counter(otel)
                    .with_unit(m.unit)
                    .with_callback(move |o| {
                        for (v, a) in points(&cache, prom) {
                            if let Some(v) = as_f64(&v) {
                                o.observe(v, &a);
                            }
                        }
                    })
                    .build();
            }
        }
    }
}

/// The two synchronous duration histograms (ADR 0009 D6).
#[derive(Clone)]
pub struct OtlpRecorders {
    rpc: Histogram<f64>,
    apply: Histogram<f64>,
}

impl std::fmt::Debug for OtlpRecorders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtlpRecorders")
    }
}

/// `rpc.server.call.duration`.
pub const RPC_DURATION: &str = "rpc.server.call.duration";
/// `memory_graph.raft.apply.duration`.
pub const APPLY_DURATION: &str = "memory_graph.raft.apply.duration";

impl OtlpRecorders {
    fn new(meter: &Meter) -> Self {
        let bounds = DURATION_BUCKETS.to_vec();
        Self {
            rpc: meter
                .f64_histogram(RPC_DURATION)
                .with_unit("s")
                .with_boundaries(bounds.clone())
                .build(),
            apply: meter
                .f64_histogram(APPLY_DURATION)
                .with_unit("s")
                .with_boundaries(bounds)
                .build(),
        }
    }

    /// One call of `rpc` (the `/metrics` label, `Store/Search`) that ended
    /// with `outcome` (`ok` or a snake-case gRPC code).
    pub fn record_rpc(&self, rpc: &str, outcome: &str, secs: f64) {
        self.rpc.record(clamp(secs), &rpc_attributes(rpc, outcome));
    }

    pub fn record_apply(&self, secs: f64) {
        self.apply.record(clamp(secs), &[]);
    }
}

/// As `/metrics` records it: a NaN or negative duration is 0.
fn clamp(secs: f64) -> f64 {
    if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        0.0
    }
}

/// The semantic-convention attributes of an rpc: `rpc.system`,
/// `rpc.service` (fully qualified), `rpc.method`, `rpc.grpc.status_code`.
pub fn rpc_attributes(rpc: &str, outcome: &str) -> [KeyValue; 4] {
    let (service, method) = match rpc.split_once('/') {
        Some(("Health", m)) => ("grpc.health.v1.Health".to_string(), m),
        Some((s, m)) => (format!("memory_graph.v1.{s}"), m),
        None => (rpc.to_string(), rpc),
    };
    [
        KeyValue::new("rpc.system", "grpc"),
        KeyValue::new("rpc.service", service),
        KeyValue::new("rpc.method", method.to_string()),
        KeyValue::new("rpc.grpc.status_code", status_code(outcome)),
    ]
}

/// The numeric gRPC code of an outcome label; a transport error (no
/// status at all) is `UNKNOWN` (2).
pub fn status_code(outcome: &str) -> i64 {
    const CODES: [&str; 17] = [
        "ok",
        "cancelled",
        "unknown",
        "invalid_argument",
        "deadline_exceeded",
        "not_found",
        "already_exists",
        "permission_denied",
        "resource_exhausted",
        "failed_precondition",
        "aborted",
        "out_of_range",
        "unimplemented",
        "internal",
        "unavailable",
        "data_loss",
        "unauthenticated",
    ];
    CODES
        .iter()
        .position(|c| *c == outcome)
        .map_or(2, |i| i as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_map_to_grpc_codes() {
        for code in 0..17 {
            let c = tonic::Code::from_i32(code);
            let label = crate::observe::outcome_label(c);
            assert_eq!(status_code(&label), i64::from(code), "{label}");
        }
        assert_eq!(status_code("transport_error"), 2);
    }

    #[test]
    fn rpc_labels_split_into_service_and_method() {
        let a = rpc_attributes("Store/Search", "ok");
        assert_eq!(a[1].value.as_str(), "memory_graph.v1.Store");
        assert_eq!(a[2].value.as_str(), "Search");
        let a = rpc_attributes("Health/Check", "not_found");
        assert_eq!(a[1].value.as_str(), "grpc.health.v1.Health");
        assert_eq!(a[3].value, opentelemetry::Value::I64(5));
        let a = rpc_attributes("unknown", "unimplemented");
        assert_eq!(a[1].value.as_str(), "unknown");
        assert_eq!(a[2].value.as_str(), "unknown");
    }
}
