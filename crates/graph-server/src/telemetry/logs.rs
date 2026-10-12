//! Logs over OTLP (ADR 0009 D7, D9; epic story 53).
//!
//! `serve` installs [`log_layer`] (the `opentelemetry-appender-tracing`
//! bridge) behind the same `--log-level` filter as its fmt layer, so OTLP
//! gets the same events as stderr. The bridge never sees the OpenTelemetry
//! crates' own events (`opentelemetry*` targets) nor anything logged on the
//! exporters' own runtime threads: the exporter never exports its own
//! errors.
//!
//! Each record carries the trace and span ids of the active span when
//! traces are on (the tracing layer attaches the span's context on enter,
//! and the SDK logger reads it). [`current_trace_ids`] reads the same
//! context for the JSON log lines, so one event shows the same ids in
//! stderr, in its OTLP record and on its exported span.
//!
//! Privacy (D9): [`AllowListLogProcessor`] sits in front of the counting
//! and batch processors and drops every attribute not in
//! [`ALLOWED_LOG_FIELDS`] before the record is queued. The message (the
//! record's body) is kept, so messages must never interpolate values.
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord};
use std::time::Duration;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Event fields exported as OTLP log attributes: operational values only
/// (names, outcomes, numeric ids, counts, sizes, durations, log indexes).
/// Anything else (errors, paths, queries, addresses) stays in local logs.
/// `peer`, `from`, `to` and `leader` are deliberately absent: call sites
/// use them for client addresses, member addresses and paths (D9). A field
/// added here must carry only such values at every call site.
pub const ALLOWED_LOG_FIELDS: &[&str] = &[
    "method",
    "rpc",
    "outcome",
    "status",
    "code",
    "signal",
    "kind",
    "role",
    "node",
    "node_id",
    "peer_id",
    "leader_id",
    "term",
    "index",
    "last",
    "next",
    "count",
    "files",
    "dropped",
    "bytes",
    "size",
    "duration_ms",
    "elapsed_ms",
    "delay_ms",
    "grace_ms",
    "attempt",
    "attempts",
];

/// Whether an event field is exported as an OTLP log attribute.
pub fn allowed_log_field(key: &str) -> bool {
    ALLOWED_LOG_FIELDS.contains(&key)
}

/// The prefix of the exporter runtime's thread names (see `init`).
pub(super) const EXPORTER_THREAD: &str = "mg-otel";

/// Whether the log bridge may see this event: never the OpenTelemetry
/// crates' own events, never anything logged on the exporter's threads
/// (its runtime and the SDK's batch threads).
pub fn bridged(m: &Metadata<'_>) -> bool {
    bridged_parts(
        m.is_event(),
        m.target(),
        std::thread::current().name().unwrap_or(""),
    )
}

/// The part of [`bridged`] that depends on the callsite only.
fn bridged_callsite(m: &Metadata<'_>) -> bool {
    m.is_event()
        && !m.target().starts_with("opentelemetry")
        && !m.target().starts_with("tracing_opentelemetry")
}

fn bridged_parts(is_event: bool, target: &str, thread: &str) -> bool {
    is_event
        && !target.starts_with("opentelemetry")
        && !target.starts_with("tracing_opentelemetry")
        && !thread.starts_with(EXPORTER_THREAD)
        // The SDK 0.33 batch processors' thread names
        // ("OpenTelemetry.Logs.BatchProcessor", ".Traces."); re-check on an
        // SDK upgrade (the test below pins the name).
        && !thread.starts_with("OpenTelemetry")
}

/// The OTLP log bridge for the guard's logger, behind [`bridged`]; `None`
/// when logs are not among the signals. Put the `--log-level` filter on it
/// too (`Layer::with_filter`), and install it once.
pub fn log_layer<S>(
    guard: &super::TelemetryGuard,
) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let provider = guard.logger_provider()?;
    Some(
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider)
            // Asked per event: `bridged` depends on the emitting thread,
            // which a cached callsite interest would freeze at the first
            // thread to log there (and see `traces::AskEachTime`).
            .with_filter(super::traces::AskEachTime {
                callsite: bridged_callsite,
                enabled: bridged,
            })
            .boxed(),
    )
}

/// The trace and span ids (lowercase hex, 32 and 16 characters) of the
/// active OpenTelemetry span on this thread; `None` outside one, or when
/// its context is not valid (never zero ids).
pub fn current_trace_ids() -> Option<(String, String)> {
    let cx = opentelemetry::Context::current();
    if !cx.has_active_span() {
        return None;
    }
    let sc = cx.span().span_context().clone();
    sc.is_valid()
        .then(|| (sc.trace_id().to_string(), sc.span_id().to_string()))
}

/// Drops every attribute not in [`ALLOWED_LOG_FIELDS`] before the record
/// goes further (D9). The SDK record has no way to remove an attribute,
/// so a filtered record is rebuilt from a blank one. The rebuild copies
/// every field `SdkLogRecord` has in SDK 0.33 (pinned exactly); a new
/// field in a later SDK must be added here, which the unit test below
/// checks field by field.
#[derive(Debug)]
pub(crate) struct AllowListLogProcessor<P> {
    pub(crate) inner: P,
    blank: SdkLogRecord,
}

impl<P> AllowListLogProcessor<P> {
    pub(crate) fn new(inner: P) -> Self {
        // A provider with no processor only hands out blank records.
        let blank = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
            .build()
            .logger("memory-graph")
            .create_log_record();
        AllowListLogProcessor { inner, blank }
    }

    fn filter(&self, data: &mut SdkLogRecord) {
        if data
            .attributes_iter()
            .all(|(k, _)| allowed_log_field(k.as_str()))
        {
            return;
        }
        let mut out = self.blank.clone();
        if let Some(n) = data.event_name() {
            out.set_event_name(n);
        }
        if let Some(t) = data.target() {
            out.set_target(t.clone());
        }
        if let Some(t) = data.timestamp() {
            out.set_timestamp(t);
        }
        if let Some(t) = data.observed_timestamp() {
            out.set_observed_timestamp(t);
        }
        if let Some(s) = data.severity_text() {
            out.set_severity_text(s);
        }
        if let Some(s) = data.severity_number() {
            out.set_severity_number(s);
        }
        if let Some(b) = data.body() {
            out.set_body(b.clone());
        }
        if let Some(tc) = data.trace_context() {
            out.set_trace_context(tc.trace_id, tc.span_id, tc.trace_flags);
        }
        out.add_attributes(
            data.attributes_iter()
                .filter(|(k, _)| allowed_log_field(k.as_str()))
                .cloned(),
        );
        *data = out;
    }
}

impl<P: LogProcessor> LogProcessor for AllowListLogProcessor<P> {
    fn emit(&self, data: &mut SdkLogRecord, instrumentation: &opentelemetry::InstrumentationScope) {
        self.filter(data);
        self.inner.emit(data, instrumentation);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
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
    use opentelemetry::logs::{AnyValue, Severity};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default, Clone)]
    struct Keep(Arc<Mutex<Vec<SdkLogRecord>>>);

    impl LogProcessor for Keep {
        fn emit(&self, data: &mut SdkLogRecord, _: &opentelemetry::InstrumentationScope) {
            self.0.lock().unwrap().push(data.clone());
        }
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    #[test]
    fn only_allow_listed_fields_survive_and_the_rest_of_the_record_is_kept() {
        let keep = Keep::default();
        let p = AllowListLogProcessor::new(keep.clone());
        let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder().build();
        let mut r = provider.logger("t").create_log_record();
        r.set_target("graph_server::x");
        r.set_event_name("ev");
        r.set_severity_number(Severity::Warn);
        r.set_severity_text("WARN");
        r.set_body(AnyValue::from("refused"));
        r.set_trace_context(
            opentelemetry::trace::TraceId::from_bytes(7u128.to_be_bytes()),
            opentelemetry::trace::SpanId::from_bytes(9u64.to_be_bytes()),
            None,
        );
        r.add_attribute("method", "Store/Search");
        r.add_attribute("query", "Zq7Sentinel");
        r.add_attribute("error", "path /secret");
        r.add_attribute("duration_ms", 3i64);
        p.emit(&mut r, &opentelemetry::InstrumentationScope::default());
        let got = keep.0.lock().unwrap()[0].clone();
        let keys: Vec<String> = got
            .attributes_iter()
            .map(|(k, _)| k.as_str().to_owned())
            .collect();
        assert_eq!(keys, ["method", "duration_ms"]);
        assert_eq!(got.body(), Some(&AnyValue::from("refused")));
        assert_eq!(got.severity_number(), Some(Severity::Warn));
        assert_eq!(got.severity_text(), Some("WARN"));
        assert_eq!(got.target().map(|t| t.as_ref()), Some("graph_server::x"));
        assert_eq!(got.event_name(), Some("ev"));
        let tc = got.trace_context().unwrap();
        assert_eq!(
            tc.trace_id,
            opentelemetry::trace::TraceId::from_bytes(7u128.to_be_bytes())
        );
        assert_eq!(
            tc.span_id,
            opentelemetry::trace::SpanId::from_bytes(9u64.to_be_bytes())
        );
    }

    /// The rebuild loses nothing but the stripped attributes: compared with
    /// `PartialEq`, which covers every field of the SDK's record, so a
    /// field a later SDK adds and the rebuild misses fails here.
    #[test]
    fn the_rebuild_equals_the_record_without_the_stripped_fields() {
        let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder().build();
        let mut want = provider.logger("t").create_log_record();
        want.set_target("graph_server::x");
        want.set_event_name("ev");
        want.set_timestamp(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(5));
        want.set_observed_timestamp(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(6));
        want.set_severity_number(Severity::Info);
        want.set_severity_text("INFO");
        want.set_body(AnyValue::from("m"));
        want.set_trace_context(
            opentelemetry::trace::TraceId::from_bytes(1u128.to_be_bytes()),
            opentelemetry::trace::SpanId::from_bytes(2u64.to_be_bytes()),
            Some(opentelemetry::trace::TraceFlags::SAMPLED),
        );
        want.add_attribute("count", 1i64);
        let mut got = want.clone();
        got.add_attribute("peer", "10.0.0.1:5");
        let p = AllowListLogProcessor::new(Keep::default());
        p.filter(&mut got);
        assert_eq!(got, want);
    }

    #[test]
    fn the_bridge_skips_opentelemetry_targets_and_exporter_threads() {
        assert!(bridged_parts(true, "graph_server::raft", "main"));
        assert!(bridged_parts(true, "memory_graph::serve", ""));
        assert!(!bridged_parts(true, "opentelemetry_sdk", "main"));
        assert!(!bridged_parts(true, "opentelemetry-otlp", "main"));
        assert!(!bridged_parts(true, "h2::codec", "mg-otel"));
        assert!(!bridged_parts(
            true,
            "h2::proto",
            "OpenTelemetry.Logs.BatchProcessor"
        ));
        assert!(!bridged_parts(true, "tracing_opentelemetry::layer", "main"));
        assert!(!bridged_parts(false, "graph_server::raft", "main"));
    }

    #[test]
    fn no_ids_outside_a_span() {
        assert_eq!(current_trace_ids(), None);
    }
}
