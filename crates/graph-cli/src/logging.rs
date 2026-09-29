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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_parse_or_name_the_flag() {
        assert!(filter("info").is_ok());
        assert!(filter("info,graph_server=debug").is_ok());
        let e = filter("info,[[[").unwrap_err();
        assert!(format!("{e:#}").contains("--log-level"), "{e:#}");
    }
}
