//! The accept loop: a connection cap, a header-read timeout, and graceful draining.
//!
//! `axum::serve` exposes neither a header-read timeout nor a connection limit, so this module
//! runs hyper directly. Both protections matter for an origin that may be reachable without a
//! proxy: a client that dribbles request headers, or opens connections and goes quiet, would
//! otherwise hold a task and a file descriptor indefinitely.

use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::{Duration, sleep, timeout};
use tokio_rustls::TlsAcceptor;

use crate::config::TlsConfig;
use crate::observability::metrics::Metrics;

/// Connection-level protections applied by [`serve_connections`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionLimits {
    /// Connections above this number are closed immediately at accept.
    pub(crate) max_connections: usize,
    /// How long a connection may take to deliver a complete request header block. It also
    /// bounds how long an idle keep-alive connection waits for its next request.
    pub(crate) header_read_timeout: Duration,
}

/// What a freshly accepted connection needs before HTTP can start.
#[derive(Clone)]
enum Acceptor {
    Plain,
    Tls {
        acceptor: TlsAcceptor,
        handshake_timeout: Duration,
    },
}

impl From<Option<TlsConfig>> for Acceptor {
    fn from(tls: Option<TlsConfig>) -> Self {
        match tls {
            None => Self::Plain,
            Some(tls) => Self::Tls {
                acceptor: TlsAcceptor::from(tls.server_config),
                handshake_timeout: Duration::from_millis(tls.handshake_timeout_ms),
            },
        }
    }
}

/// Accepts connections until `shutdown` completes, then drains those still open.
///
/// Returns once every connection has finished. Callers that need a deadline race this future
/// against a timer.
pub(crate) async fn serve_connections(
    listener: TcpListener,
    app: Router,
    limits: ConnectionLimits,
    tls: Option<TlsConfig>,
    metrics: Arc<Metrics>,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let acceptor = Acceptor::from(tls);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let builder = Arc::new(builder);
    let mut shutdown = pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(connection) => connection,
            Err(error) if is_per_connection_error(&error) => continue,
            Err(error) => {
                // Usually descriptor exhaustion. Back off instead of spinning.
                tracing::error!(event = "accept_failed", %error);
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            metrics.connection_rejected();
            tracing::warn!(event = "connection_rejected", peer.address = %peer);
            continue;
        };
        let _ = stream.set_nodelay(true);

        // Subscribed now, watched once the connection (and, with TLS, its handshake) is ready:
        // a shutdown signalled in between is still seen, since the channel is already subscribed.
        let watcher = graceful.watcher();
        let acceptor = acceptor.clone();
        let builder = Arc::clone(&builder);
        let app = app.clone();
        let open = metrics.connection_opened();
        tokio::spawn(async move {
            let _slot = slot;
            let _open = open;
            let result = match acceptor {
                Acceptor::Plain => {
                    watcher
                        .watch(
                            builder
                                .serve_connection(
                                    TokioIo::new(stream),
                                    TowerToHyperService::new(app),
                                )
                                .into_owned(),
                        )
                        .await
                }
                Acceptor::Tls {
                    acceptor,
                    handshake_timeout,
                } => match timeout(handshake_timeout, acceptor.accept(stream)).await {
                    Ok(Ok(tls_stream)) => {
                        watcher
                            .watch(
                                builder
                                    .serve_connection(
                                        TokioIo::new(tls_stream),
                                        TowerToHyperService::new(app),
                                    )
                                    .into_owned(),
                            )
                            .await
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(event = "tls_handshake_failed", peer.address = %peer, %error);
                        return;
                    }
                    Err(_) => {
                        tracing::debug!(event = "tls_handshake_timed_out", peer.address = %peer);
                        return;
                    }
                },
            };
            if let Err(error) = result {
                tracing::debug!(event = "connection_closed_with_error", peer.address = %peer, %error);
            }
        });
    }

    // Stop accepting, then wait for open connections. Idle keep-alive connections close
    // promptly; connections mid-response finish first.
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// Errors that describe one bad connection, not the listener.
fn is_per_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}
