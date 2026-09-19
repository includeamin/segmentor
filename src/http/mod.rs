//! The HTTP origin: router, middleware, handlers, and response streaming.
//!
//! See `docs/internals/http-server.md` for how the pieces fit together.
mod cors;
mod error;
mod handlers;
mod middleware;
mod range;
mod router;
mod server;
mod shutdown;
mod state;
mod stream;
mod validators;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use shutdown::shutdown_signal;
use tokio::sync::oneshot;
use tokio::time::{Duration, sleep};

pub(crate) use router::router;
pub(crate) use server::{ConnectionLimits, serve_connections};
pub(crate) use state::AppState;

use crate::APP_NAME;
use crate::config::Config;
use crate::error::Result;

pub(crate) async fn serve(config: Config) -> Result<()> {
    tracing::info!(
        event = "service_starting",
        listen.address = %config.listen,
        assets.count = config.assets.len(),
        packaging.segment_duration_ms = config.segment_duration_ms,
    );
    let state = AppState::load(&config)?;
    let ready = Arc::clone(&state.ready);
    let metrics = Arc::clone(&state.metrics);
    let limits = ConnectionLimits {
        max_connections: state.max_connections,
        header_read_timeout: state.header_read_timeout,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(
        event = "service_ready",
        service.name = APP_NAME,
        listen.address = %listener.local_addr()?,
    );

    let delay = Duration::from_millis(config.shutdown_delay_ms);
    let grace = Duration::from_millis(config.shutdown_grace_ms);
    let (draining, drain_started) = oneshot::channel();
    let server = serve_connections(listener, app, limits, metrics, async move {
        shutdown_signal().await;
        ready.store(false, Ordering::Relaxed);
        tracing::info!(
            event = "shutdown_started",
            shutdown.delay_ms = config.shutdown_delay_ms,
            shutdown.grace_ms = config.shutdown_grace_ms,
        );
        let _ = draining.send(());
        // Keep accepting while `/ready` reports 503 so load balancers can drain us.
        sleep(delay).await;
    });
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        () = async {
            let _ = drain_started.await;
            sleep(delay + grace).await;
        } => {
            tracing::warn!(event = "shutdown_grace_expired", "closing with streams still open");
        }
    }
    tracing::info!(event = "service_stopped", service.name = APP_NAME);
    Ok(())
}
