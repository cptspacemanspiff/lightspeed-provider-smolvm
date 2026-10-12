//! Log output. Events go to stdout, filtered by `RUST_LOG` (default `info`).
//!
//! This is the one place that installs a subscriber, so an OpenTelemetry
//! exporter can be added here as another layer without touching call sites.

use tracing_subscriber::{EnvFilter, fmt};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per event, for log collectors.
    Json,
}

pub fn init(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stdout);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .init(),
    }
}
