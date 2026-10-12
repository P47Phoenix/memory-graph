//! `serve` logging (ADR 0004 D10): a `tracing` subscriber writing to
//! stderr, as text (the default) or JSON lines, filtered by `--log-level` /
//! `MEMORY_GRAPH_LOG` (`tracing_subscriber::EnvFilter` syntax, e.g. `info`
//! or `info,graph_server=debug`).
//!
//! JSON: one object per line with `timestamp`, `level`, `target`, `message`
//! (the event's fields flattened beside it) and `span` (the innermost span
//! with its fields, e.g. `rpc` with `method`, `peer`, `outcome`,
//! `duration_ms`, or `apply` with `index`, `kind`, `files`,
//! `duration_ms`), plus `trace_id` and `span_id` last when OTLP traces are
//! on and the event is inside an exported span (ADR 0009 D7). With OTLP
//! logs on, the same events also go to the collector. stdout stays free of logs: `serve` prints its
//! `listening on <addr>` start line there.
use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// An extra layer for the global subscriber: OpenTelemetry's, when traces
/// are exported (ADR 0009 D5, `graph_server::telemetry::tracing_layer`).
pub type ExtraLayer =
    Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>;

/// The environment variable `--log-level` reads.
pub const ENV_LOG: &str = "MEMORY_GRAPH_LOG";

/// `--log-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines.
    Text,
    /// One JSON object per line.
    Json,
}

/// Parse a filter (so a typo fails at start-up, naming the flag).
pub fn filter(spec: &str) -> Result<EnvFilter> {
    EnvFilter::try_new(spec).with_context(|| {
        format!("--log-level / {ENV_LOG} `{spec}`: not a tracing filter (e.g. `info`, `debug`, `info,graph_server=debug`)")
    })
}

/// The OpenTelemetry layers for the global subscriber (ADR 0009 D5, D7),
/// each present only when its signal is exported.
#[derive(Default)]
pub struct OtelLayers {
    /// The tracing layer (`graph_server::telemetry::tracing_layer`): traces
    /// on. JSON log lines then carry `trace_id` and `span_id`.
    pub traces: Option<ExtraLayer>,
    /// The OTLP log bridge (`graph_server::telemetry::log_layer`): logs on.
    /// It gets the `--log-level` filter too, so OTLP sees what stderr does.
    pub logs: Option<ExtraLayer>,
}

/// Install the global subscriber (once per process; a second call fails).
/// With any OpenTelemetry layer it is a `Registry` with the fmt layer
/// behind the `--log-level` filter, the tracing layer behind its own and
/// the log bridge behind the `--log-level` filter as well; without, the
/// fmt subscriber exactly as before (ADR 0009 D1). JSON lines gain
/// `trace_id` and `span_id` only with traces on, and only inside a span
/// with a valid context (D7).
pub fn init(format: LogFormat, spec: &str, otel: OtelLayers) -> Result<()> {
    init_to(format, spec, otel, std::io::stderr)
}

/// [`init`] writing to `writer` instead of stderr (tests).
#[doc(hidden)]
pub fn init_to<W>(format: LogFormat, spec: &str, otel: OtelLayers, writer: W) -> Result<()>
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    let f = filter(spec)?;
    if otel.traces.is_some() || otel.logs.is_some() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer;
        let ids = otel.traces.is_some();
        let fmt = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false);
        let fmt = match format {
            LogFormat::Text => fmt.with_filter(f).boxed(),
            LogFormat::Json if ids => fmt
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .map_event_format(WithTraceIds)
                .with_filter(f)
                .boxed(),
            LogFormat::Json => fmt
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_filter(f)
                .boxed(),
        };
        // The `EnvFilter`s here are per-layer filters that may cache
        // `Interest::always`, and a callsite with that interest is decided
        // from the thread's per-layer filter state as the last `enabled`
        // call left it (#309, #310). That is safe here: the OpenTelemetry
        // layers filter with `AskEachTime`, which answers `sometimes` or
        // `never` but never `always`, so the combined interest of any
        // callsite fmt logs is `sometimes` and every span and event runs
        // `enabled` on all filters first (tests/log_fmt_stale_hint.rs).
        let mut layers: Vec<ExtraLayer> = Vec::with_capacity(3);
        layers.extend(otel.traces);
        if let Some(l) = otel.logs {
            layers.push(l.with_filter(filter(spec)?).boxed());
        }
        layers.push(fmt);
        return tracing_subscriber::registry()
            .with(layers)
            .try_init()
            .map_err(|e| anyhow::anyhow!("installing the log subscriber: {e}"));
    }
    let b = tracing_subscriber::fmt()
        .with_env_filter(f)
        .with_writer(writer)
        .with_ansi(false);
    let r = match format {
        LogFormat::Text => b.try_init(),
        LogFormat::Json => b
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .try_init(),
    };
    r.map_err(|e| anyhow::anyhow!("installing the log subscriber: {e}"))
}

/// The JSON event format with the active span's OpenTelemetry ids: inside
/// a span with a valid context, `trace_id` and `span_id` are appended as
/// the line's last fields (the ids its OTLP record and exported span
/// carry); elsewhere the line is the inner format's, byte for byte.
struct WithTraceIds<F>(F);

impl<S, N, F> tracing_subscriber::fmt::FormatEvent<S, N> for WithTraceIds<F>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
    F: tracing_subscriber::fmt::FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let Some((trace_id, span_id)) = graph_server::telemetry::current_trace_ids() else {
            return self.0.format_event(ctx, writer, event);
        };
        let mut line = String::new();
        self.0.format_event(
            ctx,
            tracing_subscriber::fmt::format::Writer::new(&mut line),
            event,
        )?;
        writer.write_str(&with_ids(&line, &trace_id, &span_id))
    }
}

/// `line` (one JSON object, then a newline) with the two ids appended as
/// its last fields; unchanged if it is not an object.
fn with_ids(line: &str, trace_id: &str, span_id: &str) -> String {
    let body = line.trim_end_matches(['\n', '\r']);
    let Some(open) = body.strip_suffix('}') else {
        return line.to_owned();
    };
    let sep = if open.trim_end().ends_with('{') {
        ""
    } else {
        ","
    };
    format!(
        "{open}{sep}\"trace_id\":\"{trace_id}\",\"span_id\":\"{span_id}\"}}{}",
        &line[body.len()..]
    )
}

/// `serve`'s start lines on stdout (`metrics on ...`, then `listening on
/// ...`, which scripts wait for). Text: `memory-graph serve: <message>`.
/// JSON: an object like a log line (`timestamp`, `level`, `target`,
/// `message`) plus `event` (`metrics` / `listening`) and `addr`, so a
/// container runtime merging stdout into the JSON log stream sees only JSON.
/// The message is the text line either way, so a parser looking for
/// `listening on <addr> ` reads both.
pub fn start_line(format: LogFormat, event: &str, message: &str, addr: &str) -> String {
    let text = format!("memory-graph serve: {message}");
    match format {
        LogFormat::Text => text,
        LogFormat::Json => {
            use tracing_subscriber::fmt::time::FormatTime;
            let mut ts = String::new();
            let _ = tracing_subscriber::fmt::time::SystemTime
                .format_time(&mut tracing_subscriber::fmt::format::Writer::new(&mut ts));
            serde_json::json!({
                "timestamp": ts,
                "level": "INFO",
                "target": "memory_graph::serve",
                "message": text,
                "event": event,
                "addr": addr,
            })
            .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_lines_are_text_or_one_json_object() {
        let t = start_line(
            LogFormat::Text,
            "listening",
            "listening on 1.2.3.4:5 (db x)",
            "1.2.3.4:5",
        );
        assert_eq!(t, "memory-graph serve: listening on 1.2.3.4:5 (db x)");
        let j = start_line(
            LogFormat::Json,
            "listening",
            "listening on 1.2.3.4:5 (db C:\\x \"y\")",
            "1.2.3.4:5",
        );
        assert!(!j.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["event"], "listening");
        assert_eq!(v["addr"], "1.2.3.4:5");
        assert_eq!(v["level"], "INFO");
        assert!(!v["timestamp"].as_str().unwrap().is_empty());
        // The text parsers' `listening on <addr> ` still finds the address.
        let addr = j
            .split("listening on ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next());
        assert_eq!(addr, Some("1.2.3.4:5"));
    }

    #[test]
    fn ids_are_appended_as_the_last_fields_of_the_object() {
        let t = "0123456789abcdef0123456789abcdef";
        let s = "0123456789abcdef";
        let line = "{\"level\":\"INFO\",\"message\":\"x }\",\"span\":{\"name\":\"rpc\"}}\n";
        let out = with_ids(line, t, s);
        assert!(out.ends_with("}\n") && out.matches('\n').count() == 1);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["trace_id"], t);
        assert_eq!(v["span_id"], s);
        assert_eq!(v["message"], "x }");
        assert_eq!(v["span"]["name"], "rpc");
        assert!(out.starts_with(line.trim_end().trim_end_matches('}')));
        let v: serde_json::Value = serde_json::from_str(&with_ids("{}\n", t, s)).unwrap();
        assert_eq!(v["span_id"], s);
        assert_eq!(with_ids("not json\n", t, s), "not json\n");
    }

    #[test]
    fn filters_parse_or_name_the_flag() {
        assert!(filter("info").is_ok());
        assert!(filter("info,graph_server=debug").is_ok());
        let e = filter("info,[[[").unwrap_err();
        assert!(format!("{e:#}").contains("--log-level"), "{e:#}");
    }
}
