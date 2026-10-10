//! W3C trace context in gRPC metadata (ADR 0009 D5): `traceparent` and
//! `tracestate`, written from the current `tracing` span's OpenTelemetry
//! context and read back on the server, with the SDK's
//! `TraceContextPropagator`.
//!
//! Injecting is a no-op unless the current span has a valid OpenTelemetry
//! context, which needs a `tracing-opentelemetry` layer in the process: a
//! client or a node with traces off sends nothing extra. Header values are
//! never logged.
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tonic::codegen::http::HeaderMap;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The W3C header carrying the trace and parent span ids.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// The W3C header carrying vendor trace state.
pub const TRACESTATE_HEADER: &str = "tracestate";

struct MetadataInjector<'a>(&'a mut MetadataMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(k), Ok(v)) = (
            MetadataKey::from_bytes(key.as_bytes()),
            MetadataValue::try_from(value.as_str()),
        ) {
            self.0.insert(k, v);
        }
    }
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Write `cx`'s span context into `md`; nothing when it has no valid span.
pub fn inject_context(cx: &Context, md: &mut MetadataMap) {
    if !cx.span().span_context().is_valid() {
        return;
    }
    TraceContextPropagator::new().inject_context(cx, &mut MetadataInjector(md));
}

/// Write the current `tracing` span's context into `md` (what every
/// outgoing interceptor calls).
pub fn inject_current(md: &mut MetadataMap) {
    let span = tracing::Span::current();
    if span.is_disabled() {
        return;
    }
    inject_context(&span.context(), md);
}

/// The remote context in `headers`: an empty context when there is none or
/// it does not parse.
pub fn extract(headers: &HeaderMap) -> Context {
    if !headers.contains_key(TRACEPARENT_HEADER) {
        return Context::new();
    }
    TraceContextPropagator::new().extract(&HeaderExtractor(headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};

    fn remote() -> Context {
        let sc = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::from_key_value([("mg", "1")]).unwrap(),
        );
        Context::new().with_remote_span_context(sc)
    }

    #[test]
    fn a_context_round_trips_through_metadata() {
        let mut md = MetadataMap::new();
        inject_context(&remote(), &mut md);
        assert_eq!(
            md.get(TRACEPARENT_HEADER).unwrap(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert_eq!(md.get(TRACESTATE_HEADER).unwrap(), "mg=1");
        let back = extract(&md.into_headers());
        let sc = back.span().span_context().clone();
        assert!(sc.is_valid() && sc.is_remote() && sc.is_sampled());
        assert_eq!(sc.span_id(), SpanId::from_hex("00f067aa0ba902b7").unwrap());
        assert_eq!(sc.trace_state().header(), "mg=1");
    }

    #[test]
    fn nothing_is_injected_without_a_valid_context() {
        let mut md = MetadataMap::new();
        inject_context(&Context::new(), &mut md);
        // No tracing-opentelemetry layer here: the current span has none.
        inject_current(&mut md);
        assert!(md.is_empty());
    }

    #[test]
    fn missing_or_garbage_headers_extract_to_nothing() {
        assert!(!extract(&HeaderMap::new()).span().span_context().is_valid());
        let mut h = HeaderMap::new();
        h.insert(TRACEPARENT_HEADER, "00-zz-yy-01".parse().unwrap());
        assert!(!extract(&h).span().span_context().is_valid());
    }
}
