use std::io;

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::config::{LogFormat, LoggingConfig};
use crate::error::{Error, Result};

pub(crate) fn init(config: &LoggingConfig) -> Result<WorkerGuard> {
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(config.buffer_capacity)
        .lossy(true)
        .thread_name("vod-log-writer")
        .finish(io::stdout());
    let filter = EnvFilter::new(config.level.as_str());
    let result = match config.format {
        LogFormat::Compact => tracing_subscriber::fmt()
            .compact()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init(),
    };
    result.map_err(|error| Error::Logging(error.to_string()))?;
    Ok(guard)
}
