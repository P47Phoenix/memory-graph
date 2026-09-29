//! `serve` logging (ADR 0004 D10): a `tracing` subscriber writing to
//! stderr, as text (the default) or JSON lines, filtered by `--log-level` /
//! `MEMORY_GRAPH_LOG` (`tracing_subscriber::EnvFilter` syntax, e.g. `info`
//! or `info,graph_server=debug`).
//!
//! JSON: one object per line with `timestamp`, `level`, `target`, `message`
//! (the event's fields flattened beside it) and `span` (the innermost span
//! with its fields, e.g. `rpc` with `method`, `peer`, `outcome`,
//! `duration_ms`, or `apply` with `index`, `kind`, `files`,
//! `duration_ms`). stdout stays free of logs: `serve` prints its
//! `listening on <addr>` start line there.
use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

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

/// Install the global subscriber (once per process; a second call fails).
pub fn init(format: LogFormat, spec: &str) -> Result<()> {
    let f = filter(spec)?;
    let b = tracing_subscriber::fmt()
        .with_env_filter(f)
        .with_writer(std::io::stderr)
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
    fn filters_parse_or_name_the_flag() {
        assert!(filter("info").is_ok());
        assert!(filter("info,graph_server=debug").is_ok());
        let e = filter("info,[[[").unwrap_err();
        assert!(format!("{e:#}").contains("--log-level"), "{e:#}");
    }
}
