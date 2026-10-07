//! OpenTelemetry over OTLP/gRPC (ADR 0009, Proposed; plan item O1).
//!
//! Off by default: with no endpoint configured, [`init`] returns `None` and
//! no provider, exporter, thread or runtime exists. With one, it builds the
//! tracer, meter and logger providers (OTLP gRPC exporters, batch span and
//! log processors, a periodic metric reader) for the selected signals. No
//! instrument, `tracing` layer or global provider is registered here: O2-O4
//! attach those to the providers this guard holds.
//!
//! Configuration precedence, highest first: a `serve` flag, its key in the
//! `serve --config` file (graph-cli merges those two before calling here,
//! as [`TelemetryOptions`]), then the standard environment variables:
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`,
//! `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_SDK_DISABLED`. Auth headers come only
//! from `OTEL_EXPORTER_OTLP_HEADERS`, which the exporter reads itself (the
//! config file refuses secret-looking keys). Transport is plain-text gRPC
//! to a local or sidecar collector, which handles TLS onward (#104), so an
//! endpoint must be `http://`.
//!
//! The exporters run on a small runtime of their own (one worker thread),
//! so telemetry never competes with, or outlives, the serving runtime, and
//! can still flush after `serve` has stopped.
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// `service.name` when nothing else names the service.
pub const DEFAULT_SERVICE_NAME: &str = "memory-graph";
/// The periodic metric reader's interval when none is given.
pub const DEFAULT_METRICS_INTERVAL: Duration = Duration::from_secs(60);
/// How long [`TelemetryGuard::shutdown`] waits for the final flush.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const ENV_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const ENV_SIGNAL_ENDPOINTS: [&str; 3] = [
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
];
const ENV_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
const ENV_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
const ENV_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";
const ENV_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";

/// Live [`TelemetryGuard`]s in this process: the structural "is anything
/// exporting" check tests use ([`is_active`]).
static ACTIVE_GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Whether any [`TelemetryGuard`] (providers, exporters, the runtime) is
/// alive in this process.
pub fn is_active() -> bool {
    ACTIVE_GUARDS.load(Ordering::SeqCst) > 0
}

/// A configuration mistake, reported at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    /// The endpoint is not `http://host[:port]`.
    BadEndpoint { source: &'static str, why: String },
    /// `--otlp-signals` is empty or names an unknown signal.
    BadSignals(String),
    /// An environment variable this build cannot honour.
    UnsupportedEnv { var: &'static str, why: String },
    /// A setting out of range (a zero interval or queue).
    BadSetting(String),
    /// Building a provider or the exporter runtime failed.
    Build(String),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TelemetryError::BadEndpoint { source, why } => write!(f, "{source}: {why}"),
            TelemetryError::BadSignals(why) => write!(f, "--otlp-signals: {why}"),
            TelemetryError::UnsupportedEnv { var, why } => write!(f, "{var}: {why}"),
            TelemetryError::BadSetting(why) => write!(f, "{why}"),
            TelemetryError::Build(why) => write!(f, "OpenTelemetry: {why}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Which signals are exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals {
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
}

impl Signals {
    pub const ALL: Signals = Signals {
        traces: true,
        metrics: true,
        logs: true,
    };

    /// A comma list of `traces`, `metrics`, `logs` (case-insensitive,
    /// spaces ignored, repeats allowed); at least one.
    pub fn parse(list: &str) -> Result<Signals, TelemetryError> {
        let mut signals = Signals {
            traces: false,
            metrics: false,
            logs: false,
        };
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match item.to_ascii_lowercase().as_str() {
                "traces" => signals.traces = true,
                "metrics" => signals.metrics = true,
                "logs" => signals.logs = true,
                other => {
                    return Err(TelemetryError::BadSignals(format!(
                        "unknown signal `{other}` (expected traces, metrics, logs)"
                    )))
                }
            }
        }
        if signals.any() {
            Ok(signals)
        } else {
            Err(TelemetryError::BadSignals(
                "names no signal (expected a comma list of traces, metrics, logs)".into(),
            ))
        }
    }

    pub fn any(self) -> bool {
        self.traces || self.metrics || self.logs
    }
}

/// The explicit settings: `serve`'s flags, already merged with its config
/// file by graph-cli (flag over config key).
#[derive(Debug, Clone, Default)]
pub struct TelemetryOptions {
    /// `--otlp-endpoint`: enables OTLP.
    pub endpoint: Option<String>,
    /// `--otlp-signals`, raw (default: all three).
    pub signals: Option<String>,
    /// `--otel-service-name`.
    pub service_name: Option<String>,
    /// `--otlp-metrics-interval` (default [`DEFAULT_METRICS_INTERVAL`]).
    pub metrics_interval: Option<Duration>,
}

/// What this process knows about the node it runs (resource attributes).
#[derive(Debug, Clone, Default)]
pub struct NodeIdentity {
    /// `service.instance.id`, when known at start.
    pub node_id: Option<u64>,
    /// `memory_graph.cluster`, when known at start.
    pub cluster: Option<String>,
    /// `host.name` (`crate::paths::hostname`).
    pub host_name: Option<String>,
}

/// Batch processor and exporter tuning: `None` keeps the SDK's default
/// (including its `OTEL_BSP_*`/`OTEL_BLRP_*` variables). Tests set tiny
/// values so nothing waits seconds for a batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchTuning {
    /// How often a batch processor exports what it holds.
    pub schedule_delay: Option<Duration>,
    /// Spans or log records a batch processor buffers before dropping.
    pub max_queue_size: Option<usize>,
    /// Deadline of one export call.
    pub export_timeout: Option<Duration>,
}

/// The resolved configuration; [`init`] acts on it.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryConfig {
    /// The collector, `http://host:port`; `None`: OTLP is off.
    pub endpoint: Option<String>,
    pub signals: Signals,
    pub service_name: String,
    pub metrics_interval: Duration,
    /// Every resource attribute, `service.name` included, sorted by key.
    pub resource: Vec<(String, String)>,
    pub batch: BatchTuning,
}

impl TelemetryConfig {
    /// Resolve from the explicit options and the process environment.
    pub fn resolve(
        options: &TelemetryOptions,
        identity: &NodeIdentity,
    ) -> Result<TelemetryConfig, TelemetryError> {
        Self::resolve_with_env(options, identity, |var| std::env::var(var).ok())
    }

    /// [`resolve`](Self::resolve) with the environment given (tests).
    pub fn resolve_with_env(
        options: &TelemetryOptions,
        identity: &NodeIdentity,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<TelemetryConfig, TelemetryError> {
        let env = |var: &str| env(var).filter(|v| !v.trim().is_empty());
        let endpoint = resolve_endpoint(options, &env)?;
        let signals = match &options.signals {
            Some(list) => Signals::parse(list)?,
            None => Signals::ALL,
        };
        let metrics_interval = options.metrics_interval.unwrap_or(DEFAULT_METRICS_INTERVAL);
        if metrics_interval.is_zero() {
            return Err(TelemetryError::BadSetting(
                "--otlp-metrics-interval: must be more than zero".into(),
            ));
        }
        let mut resource = parse_resource_attributes(env(ENV_RESOURCE_ATTRIBUTES).as_deref());
        let service_name = options
            .service_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| env(ENV_SERVICE_NAME))
            .or_else(|| resource.get("service.name").cloned())
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());
        resource.insert("service.name".into(), service_name.clone());
        resource.insert("service.version".into(), crate::SERVER_VERSION.into());
        if let Some(id) = identity.node_id {
            resource.insert("service.instance.id".into(), id.to_string());
        }
        if let Some(cluster) = &identity.cluster {
            resource.insert("memory_graph.cluster".into(), cluster.clone());
        }
        if let Some(host) = &identity.host_name {
            // OTEL_RESOURCE_ATTRIBUTES may name the host better (a pod).
            resource
                .entry("host.name".into())
                .or_insert_with(|| host.clone());
        }
        Ok(TelemetryConfig {
            endpoint,
            signals,
            service_name,
            metrics_interval,
            resource: resource.into_iter().collect(),
            batch: BatchTuning::default(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// The endpoint, flag or config first, then the environment. Startup
/// refuses a bad explicit endpoint; a bad environment one (other than
/// `https://`, which only disables OTLP with an error log, since a shared
/// environment may target another SDK) is refused too. Settings this build
/// cannot honour are refused rather than silently ignored.
fn resolve_endpoint(
    options: &TelemetryOptions,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, TelemetryError> {
    if let Some(explicit) = options.endpoint.as_deref().filter(|e| !e.trim().is_empty()) {
        return validate_endpoint(explicit)
            .map(Some)
            .map_err(|why| TelemetryError::BadEndpoint {
                source: "--otlp-endpoint",
                why,
            });
    }
    if env(ENV_SDK_DISABLED).is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return Ok(None);
    }
    let Some(from_env) = env(ENV_ENDPOINT) else {
        return Ok(None);
    };
    check_unsupported_env(env)?;
    if from_env.trim().to_ascii_lowercase().starts_with("https://") {
        tracing::error!(
            endpoint = %from_env,
            "{ENV_ENDPOINT} is https://, but this build exports plain-text gRPC only \
             (TLS belongs to the collector, #104); OpenTelemetry export is off"
        );
        return Ok(None);
    }
    validate_endpoint(&from_env)
        .map(Some)
        .map_err(|why| TelemetryError::BadEndpoint {
            source: ENV_ENDPOINT,
            why,
        })
}

fn check_unsupported_env(env: &impl Fn(&str) -> Option<String>) -> Result<(), TelemetryError> {
    for var in ENV_SIGNAL_ENDPOINTS {
        if env(var).is_some() {
            return Err(TelemetryError::UnsupportedEnv {
                var,
                why: format!("per-signal endpoints are not supported; set {ENV_ENDPOINT} only"),
            });
        }
    }
    match env(ENV_PROTOCOL) {
        Some(p) if !p.trim().eq_ignore_ascii_case("grpc") => Err(TelemetryError::UnsupportedEnv {
            var: ENV_PROTOCOL,
            why: format!("`{p}` is not supported; only grpc"),
        }),
        _ => Ok(()),
    }
}

/// `http://host[:port][/]`, normalised without the trailing slash.
fn validate_endpoint(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let uri: tonic::codegen::http::Uri = trimmed
        .parse()
        .map_err(|e| format!("`{raw}` is not a URL ({e})"))?;
    match uri.scheme_str() {
        Some("http") => {}
        Some("https") => {
            return Err(format!(
                "`{raw}`: https is not supported; export plain-text gRPC to a local or \
                 sidecar collector (http://...) and let it handle TLS (#104)"
            ))
        }
        _ => return Err(format!("`{raw}` must start with http://")),
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(format!("`{raw}` names no host"));
    }
    if uri.path() != "/" && !uri.path().is_empty() {
        return Err(format!("`{raw}`: a gRPC endpoint takes no path"));
    }
    Ok(trimmed.to_string())
}

/// `OTEL_RESOURCE_ATTRIBUTES`: `k=v,k2=v2`, values percent-decoded; a
/// malformed pair is skipped (the spec says to ignore it).
fn parse_resource_attributes(raw: Option<&str>) -> std::collections::BTreeMap<String, String> {
    raw.unwrap_or_default()
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            let value = percent_encoding::percent_decode_str(value.trim())
                .decode_utf8()
                .ok()?;
            Some((key.to_string(), value.into_owned()))
        })
        .collect()
}

/// Start OTLP export as configured: `None` when it is off (nothing is
/// created), else the providers in a guard to keep for the process life.
pub fn init(cfg: &TelemetryConfig) -> Result<Option<TelemetryGuard>, TelemetryError> {
    let Some(endpoint) = cfg.endpoint.as_deref() else {
        return Ok(None);
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("mg-otel")
        .enable_all()
        .build()
        .map_err(|e| TelemetryError::Build(format!("exporter runtime: {e}")))?;
    // The tonic channels spawn their connection tasks on the current
    // runtime when built: make that ours.
    let providers = {
        let _entered = runtime.enter();
        Providers::build(cfg, endpoint)?
    };
    ACTIVE_GUARDS.fetch_add(1, Ordering::SeqCst);
    Ok(Some(TelemetryGuard {
        inner: Some(GuardInner { providers, runtime }),
    }))
}

struct Providers {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Providers {
    fn build(cfg: &TelemetryConfig, endpoint: &str) -> Result<Providers, TelemetryError> {
        let resource = Resource::builder_empty()
            .with_attributes(
                cfg.resource
                    .iter()
                    .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
            )
            .build();
        let built = |what: &str, e: opentelemetry_otlp::ExporterBuildError| {
            TelemetryError::Build(format!("{what} exporter: {e}"))
        };
        let timeout = cfg.batch.export_timeout;
        let tracer = if cfg.signals.traces {
            let mut b = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            let exporter = b.build().map_err(|e| built("span", e))?;
            let processor = opentelemetry_sdk::trace::BatchSpanProcessor::builder(exporter)
                .with_batch_config(span_batch_config(&cfg.batch))
                .build();
            Some(
                SdkTracerProvider::builder()
                    .with_span_processor(processor)
                    .with_resource(resource.clone())
                    .build(),
            )
        } else {
            None
        };
        let meter = if cfg.signals.metrics {
            let mut b = opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            let exporter = b.build().map_err(|e| built("metric", e))?;
            let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
                .with_interval(cfg.metrics_interval)
                .build();
            Some(
                SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource.clone())
                    .build(),
            )
        } else {
            None
        };
        let logger = if cfg.signals.logs {
            let mut b = opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            let exporter = b.build().map_err(|e| built("log", e))?;
            let processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(exporter)
                .with_batch_config(log_batch_config(&cfg.batch))
                .build();
            Some(
                SdkLoggerProvider::builder()
                    .with_log_processor(processor)
                    .with_resource(resource)
                    .build(),
            )
        } else {
            None
        };
        Ok(Providers {
            tracer,
            meter,
            logger,
        })
    }

    /// Flush and shut every provider down, each bounded by `timeout`.
    fn shutdown(&self, timeout: Duration) {
        let report = |signal: &str, r: opentelemetry_sdk::error::OTelSdkResult| {
            if let Err(e) = r {
                tracing::warn!(signal, error = %e, "OpenTelemetry shutdown");
            }
        };
        if let Some(p) = &self.tracer {
            report("traces", p.shutdown_with_timeout(timeout));
        }
        if let Some(p) = &self.meter {
            report("metrics", p.shutdown_with_timeout(timeout));
        }
        if let Some(p) = &self.logger {
            report("logs", p.shutdown_with_timeout(timeout));
        }
    }
}

fn span_batch_config(t: &BatchTuning) -> opentelemetry_sdk::trace::BatchConfig {
    let mut b = opentelemetry_sdk::trace::BatchConfigBuilder::default();
    if let Some(d) = t.schedule_delay {
        b = b.with_scheduled_delay(d);
    }
    if let Some(n) = t.max_queue_size {
        b = b.with_max_queue_size(n);
    }
    b.build()
}

fn log_batch_config(t: &BatchTuning) -> opentelemetry_sdk::logs::BatchConfig {
    let mut b = opentelemetry_sdk::logs::BatchConfigBuilder::default();
    if let Some(d) = t.schedule_delay {
        b = b.with_scheduled_delay(d);
    }
    if let Some(n) = t.max_queue_size {
        b = b.with_max_queue_size(n);
    }
    b.build()
}

struct GuardInner {
    providers: Providers,
    runtime: tokio::runtime::Runtime,
}

impl GuardInner {
    /// The blocking part of a shutdown: never on a runtime worker.
    fn finish(self, timeout: Duration) {
        self.providers.shutdown(timeout);
        self.runtime.shutdown_background();
    }
}

/// OTLP export while alive. Call [`shutdown`](Self::shutdown) (async) or
/// [`shutdown_blocking`](Self::shutdown_blocking) to flush, bounded by
/// [`SHUTDOWN_TIMEOUT`]; dropping it without either only starts the flush
/// on a thread of its own and never waits.
pub struct TelemetryGuard {
    inner: Option<GuardInner>,
}

impl TelemetryGuard {
    pub fn tracer_provider(&self) -> Option<&SdkTracerProvider> {
        self.inner.as_ref()?.providers.tracer.as_ref()
    }

    pub fn meter_provider(&self) -> Option<&SdkMeterProvider> {
        self.inner.as_ref()?.providers.meter.as_ref()
    }

    pub fn logger_provider(&self) -> Option<&SdkLoggerProvider> {
        self.inner.as_ref()?.providers.logger.as_ref()
    }

    /// Flush and stop, waiting at most about [`SHUTDOWN_TIMEOUT`]; the
    /// blocking work runs off the caller's runtime workers.
    pub async fn shutdown(mut self) {
        let Some(inner) = self.take() else { return };
        let work = tokio::task::spawn_blocking(move || inner.finish(SHUTDOWN_TIMEOUT));
        if tokio::time::timeout(SHUTDOWN_TIMEOUT * 2, work)
            .await
            .is_err()
        {
            tracing::warn!("OpenTelemetry shutdown did not finish in time; abandoned");
        }
    }

    /// [`shutdown`](Self::shutdown) for a caller outside any runtime.
    pub fn shutdown_blocking(mut self) {
        let Some(inner) = self.take() else { return };
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            inner.finish(SHUTDOWN_TIMEOUT);
            let _ = done.send(());
        });
        if wait.recv_timeout(SHUTDOWN_TIMEOUT * 2).is_err() {
            tracing::warn!("OpenTelemetry shutdown did not finish in time; abandoned");
        }
    }

    fn take(&mut self) -> Option<GuardInner> {
        let inner = self.inner.take();
        if inner.is_some() {
            ACTIVE_GUARDS.fetch_sub(1, Ordering::SeqCst);
        }
        inner
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.take() {
            // Fallback only: never block whoever drops us.
            std::thread::spawn(move || inner.finish(SHUTDOWN_TIMEOUT));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve(
        options: &TelemetryOptions,
        env: &[(&str, &str)],
    ) -> Result<TelemetryConfig, TelemetryError> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let identity = NodeIdentity {
            node_id: Some(3),
            cluster: Some("c-1".into()),
            host_name: Some("mg-2".into()),
        };
        TelemetryConfig::resolve_with_env(options, &identity, |k| env.get(k).cloned())
    }

    fn attr<'a>(cfg: &'a TelemetryConfig, key: &str) -> Option<&'a str> {
        cfg.resource
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn with_endpoint(e: &str) -> TelemetryOptions {
        TelemetryOptions {
            endpoint: Some(e.into()),
            ..Default::default()
        }
    }

    #[test]
    fn off_without_an_endpoint() {
        let cfg = resolve(&TelemetryOptions::default(), &[]).unwrap();
        assert!(!cfg.is_enabled());
        assert!(init(&cfg).unwrap().is_none());
    }

    #[test]
    fn the_flag_wins_over_the_environment() {
        let cfg = resolve(
            &with_endpoint("http://flag:4317/"),
            &[(ENV_ENDPOINT, "http://env:4317")],
        )
        .unwrap();
        assert_eq!(cfg.endpoint.as_deref(), Some("http://flag:4317"));
        let cfg = resolve(
            &TelemetryOptions::default(),
            &[(ENV_ENDPOINT, "http://env:4317")],
        )
        .unwrap();
        assert_eq!(cfg.endpoint.as_deref(), Some("http://env:4317"));
        assert_eq!(cfg.signals, Signals::ALL);
        assert_eq!(cfg.metrics_interval, DEFAULT_METRICS_INTERVAL);
    }

    #[test]
    fn bad_explicit_endpoints_are_refused() {
        for bad in [
            "https://c:4317",
            "c:4317",
            "ftp://c",
            "http://",
            "http://c:4317/v1/traces",
            "::",
        ] {
            let e = resolve(&with_endpoint(bad), &[]).unwrap_err();
            assert!(
                matches!(
                    e,
                    TelemetryError::BadEndpoint {
                        source: "--otlp-endpoint",
                        ..
                    }
                ),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn https_from_the_environment_disables_and_others_refuse() {
        let cfg = resolve(
            &TelemetryOptions::default(),
            &[(ENV_ENDPOINT, "https://c:4317")],
        )
        .unwrap();
        assert!(!cfg.is_enabled());
        let e = resolve(&TelemetryOptions::default(), &[(ENV_ENDPOINT, "c:4317")]).unwrap_err();
        assert!(matches!(
            e,
            TelemetryError::BadEndpoint {
                source: ENV_ENDPOINT,
                ..
            }
        ));
        for var in ENV_SIGNAL_ENDPOINTS {
            let e = resolve(
                &TelemetryOptions::default(),
                &[(ENV_ENDPOINT, "http://c:4317"), (var, "http://x:4317")],
            )
            .unwrap_err();
            assert!(matches!(e, TelemetryError::UnsupportedEnv { .. }), "{var}");
        }
        let e = resolve(
            &TelemetryOptions::default(),
            &[
                (ENV_ENDPOINT, "http://c:4317"),
                (ENV_PROTOCOL, "http/protobuf"),
            ],
        )
        .unwrap_err();
        assert!(e.to_string().contains(ENV_PROTOCOL), "{e}");
        let ok = resolve(
            &TelemetryOptions::default(),
            &[(ENV_ENDPOINT, "http://c:4317"), (ENV_PROTOCOL, "grpc")],
        );
        assert!(ok.unwrap().is_enabled());
    }

    #[test]
    fn sdk_disabled_turns_the_environment_off_but_not_a_flag() {
        let env = [(ENV_ENDPOINT, "http://c:4317"), (ENV_SDK_DISABLED, "TRUE")];
        assert!(!resolve(&TelemetryOptions::default(), &env)
            .unwrap()
            .is_enabled());
        assert!(resolve(&with_endpoint("http://f:4317"), &env)
            .unwrap()
            .is_enabled());
    }

    #[test]
    fn signals_parse_and_bad_lists_are_refused() {
        assert_eq!(
            Signals::parse(" Traces, logs ,traces").unwrap(),
            Signals {
                traces: true,
                metrics: false,
                logs: true
            }
        );
        assert_eq!(Signals::parse("traces,metrics,logs").unwrap(), Signals::ALL);
        for bad in ["", " , ", "traces,spans", "all"] {
            assert!(
                matches!(Signals::parse(bad), Err(TelemetryError::BadSignals(_))),
                "{bad}"
            );
        }
        let options = TelemetryOptions {
            signals: Some("bogus".into()),
            ..with_endpoint("http://c:4317")
        };
        assert!(resolve(&options, &[]).is_err());
    }

    /// The whole pipeline against the fake collector: providers start, a
    /// span reaches the collector, and shutdown flushes and ends cleanly.
    #[test]
    fn init_exports_to_a_collector_and_shuts_down_cleanly() {
        use crate::testing::{FakeCollector, FakeSignal};
        use opentelemetry::trace::{Tracer as _, TracerProvider as _};
        let collector = FakeCollector::start();
        let mut cfg = resolve(&with_endpoint(&collector.endpoint()), &[]).unwrap();
        cfg.batch = BatchTuning {
            schedule_delay: Some(Duration::from_millis(20)),
            max_queue_size: Some(64),
            export_timeout: Some(Duration::from_secs(2)),
        };
        let guard = init(&cfg).unwrap().expect("enabled");
        assert!(is_active());
        assert!(guard.meter_provider().is_some() && guard.logger_provider().is_some());
        let tracer = guard.tracer_provider().expect("traces on").tracer("smoke");
        tracer.in_span("smoke-span", |_| {});
        guard.shutdown_blocking();
        assert!(collector.wait_for(FakeSignal::Traces, 1, Duration::from_secs(10)));
        let received = collector.received();
        let span = &received.traces[0].resource_spans[0];
        let has_service = span
            .resource
            .as_ref()
            .is_some_and(|r| r.attributes.iter().any(|kv| kv.key == "service.name"));
        assert!(has_service, "resource carries service.name");
    }

    #[test]
    fn a_zero_metrics_interval_is_refused() {
        let options = TelemetryOptions {
            metrics_interval: Some(Duration::ZERO),
            ..with_endpoint("http://c:4317")
        };
        assert!(matches!(
            resolve(&options, &[]),
            Err(TelemetryError::BadSetting(_))
        ));
    }

    #[test]
    fn service_name_precedence_and_resource_attributes() {
        let env = [
            (
                ENV_RESOURCE_ATTRIBUTES,
                "service.name=attr,deployment.environment=prod%20eu,host.name=pod-7,bad",
            ),
            (ENV_SERVICE_NAME, "env-name"),
        ];
        let flag = TelemetryOptions {
            service_name: Some("flag-name".into()),
            ..with_endpoint("http://c:4317")
        };
        assert_eq!(resolve(&flag, &env).unwrap().service_name, "flag-name");
        let cfg = resolve(&with_endpoint("http://c:4317"), &env).unwrap();
        assert_eq!(
            cfg.service_name, "env-name",
            "OTEL_SERVICE_NAME beats the attribute"
        );
        assert_eq!(attr(&cfg, "service.name"), Some("env-name"));
        assert_eq!(attr(&cfg, "deployment.environment"), Some("prod eu"));
        assert_eq!(attr(&cfg, "host.name"), Some("pod-7"));
        assert_eq!(attr(&cfg, "service.instance.id"), Some("3"));
        assert_eq!(attr(&cfg, "memory_graph.cluster"), Some("c-1"));
        assert_eq!(attr(&cfg, "service.version"), Some(crate::SERVER_VERSION));
        let cfg = resolve(&with_endpoint("http://c:4317"), &env[..1]).unwrap();
        assert_eq!(cfg.service_name, "attr");
        let cfg = resolve(&with_endpoint("http://c:4317"), &[]).unwrap();
        assert_eq!(cfg.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(attr(&cfg, "host.name"), Some("mg-2"));
    }
}
