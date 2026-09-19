//! Entry points for the performance harness in `benches/`.
//!
//! Hidden from generated documentation: this exists so benchmarks can drive the real packaging
//! code and the real server without widening the crate's public surface.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Duration;

use crate::asset::PackagedAsset;
use crate::config::{Config, LimitsConfig};
use crate::http::{AppState, ConnectionLimits, router, serve_connections};
use crate::media::TrackKind;
use crate::observability::metrics::Metrics;

/// A loaded asset, for timing parsing, planning, and segment preparation in isolation.
#[doc(hidden)]
pub struct BenchAsset(Arc<PackagedAsset>);

impl BenchAsset {
    /// Opens, parses, plans, and renders the file at `path`, as server startup does.
    ///
    /// # Errors
    ///
    /// Returns the failure message if the file cannot be loaded.
    pub async fn load(path: &Path, segment_duration_ms: u64) -> Result<Self, String> {
        PackagedAsset::load_local(path, segment_duration_ms, &LimitsConfig::default())
            .await
            .map(|asset| Self(Arc::new(asset)))
            .map_err(|error| error.to_string())
    }

    /// Number of planned segments.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.0.plan.segments.len()
    }

    /// Estimated resident bytes for this asset's index and rendered text.
    #[must_use]
    pub fn index_bytes(&self) -> u64 {
        self.0.index_bytes()
    }

    /// Prepares one video media segment (header and source ranges, no payload read) and returns
    /// its total response length.
    ///
    /// # Errors
    ///
    /// Returns the failure message if the segment does not exist.
    pub fn prepare_video_segment(&self, index: u32) -> Result<u64, String> {
        self.0
            .prepare_media_segment(TrackKind::Video, index)
            .map(|prepared| prepared.content_length)
            .map_err(|error| error.to_string())
    }
}

/// Tunables for [`BenchServer`]; zero means "use the default".
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerOptions {
    /// Target segment duration.
    pub segment_duration_ms: u64,
    /// Concurrent segment read slots.
    pub max_segment_jobs: usize,
    /// Simultaneous TCP connections.
    pub max_connections: usize,
    /// Simultaneous in-progress requests.
    pub max_concurrent_requests: usize,
}

/// The real server bound to an ephemeral loopback port.
#[doc(hidden)]
pub struct BenchServer {
    address: SocketAddr,
    metrics: Arc<Metrics>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

impl BenchServer {
    /// Serves the file at `path` as asset `asset`.
    ///
    /// # Errors
    ///
    /// Returns the failure message if the asset cannot load or the port cannot be bound.
    pub async fn start(path: &Path, options: ServerOptions) -> Result<Self, String> {
        let mut limits = LimitsConfig::default();
        if options.max_segment_jobs != 0 {
            limits.max_segment_jobs = options.max_segment_jobs;
        }
        if options.max_connections != 0 {
            limits.max_connections = options.max_connections;
        }
        if options.max_concurrent_requests != 0 {
            limits.max_concurrent_requests = options.max_concurrent_requests;
        }
        let mut assets = BTreeMap::new();
        let canonical = path.canonicalize().map_err(|error| error.to_string())?;
        assets.insert("asset".to_owned(), canonical.clone());
        let media_root = canonical
            .parent()
            .map_or_else(|| canonical.clone(), Path::to_path_buf);
        let segment_duration_ms = if options.segment_duration_ms == 0 {
            6000
        } else {
            options.segment_duration_ms
        };
        let mut config = Config::for_catalog(media_root, assets, segment_duration_ms, limits);
        config.listen = "127.0.0.1:0".parse().map_err(|error| format!("{error}"))?;
        let state = AppState::new(&config).map_err(|error| error.to_string())?;
        state.preload().await.map_err(|error| error.to_string())?;
        let metrics = Arc::clone(&state.metrics);
        let connection_limits = ConnectionLimits {
            max_connections: state.max_connections,
            header_read_timeout: state.header_read_timeout,
        };
        let listener = TcpListener::bind(config.listen)
            .await
            .map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_connections(
            listener,
            router(state),
            connection_limits,
            Arc::clone(&metrics),
            async move {
                let _ = signal.await;
            },
        ));
        Ok(Self {
            address,
            metrics,
            shutdown: Some(shutdown),
            task,
        })
    }

    /// The bound address.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The Prometheus text the server would expose at `/metrics`.
    #[must_use]
    pub fn metrics_text(&self) -> String {
        self.metrics.render(0)
    }

    /// Stops accepting and waits briefly for the accept loop to finish.
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.task).await;
    }
}
