use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::config::{LogFormat, LoggingConfig};
use crate::error::{Error, Result};

static DROPPED_LOG_LINES: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn dropped_lines() -> usize {
    DROPPED_LOG_LINES.load(Ordering::Relaxed)
}

pub(crate) struct LoggingGuard {
    shutdown: Sender<()>,
    monitor: Option<JoinHandle<()>>,
    _writer: WorkerGuard,
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

pub(crate) fn init(config: &LoggingConfig) -> Result<LoggingGuard> {
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(config.buffer_capacity)
        .lossy(true)
        .thread_name("vod-log-writer")
        .finish(io::stdout());
    let dropped = writer.error_counter();
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
    let (shutdown, receiver) = mpsc::channel();
    let monitor = thread::Builder::new()
        .name("vod-log-monitor".to_owned())
        .spawn(move || {
            let mut reported = 0usize;
            loop {
                match receiver.recv_timeout(Duration::from_secs(10)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let current = dropped.dropped_lines();
                        DROPPED_LOG_LINES.store(current, Ordering::Relaxed);
                        if current > reported {
                            eprintln!(
                                "vod-module-rs: non-blocking logger dropped {} additional records ({} total)",
                                current - reported,
                                current
                            );
                            reported = current;
                        }
                    }
                }
            }
        })
        .map_err(|error| Error::Logging(error.to_string()))?;
    Ok(LoggingGuard {
        shutdown,
        monitor: Some(monitor),
        _writer: guard,
    })
}
