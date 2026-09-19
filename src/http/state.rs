//! Shared application state and startup wiring.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, timeout};
use tower_http::cors::CorsLayer;

use super::cors::cors_layer;
use super::error::{HttpError, HttpResult};
use super::router::ROUTES;
use crate::asset::PackagedAsset;
use crate::config::{Config, ResolverSettings};
use crate::error::Result;
use crate::observability::metrics::Metrics;
use crate::registry::{AssetRegistry, RegistrySettings, SourceOpener};
use crate::resolver::{AssetResolver, HttpResolver, LocationPolicy, StaticResolver};
use crate::source::{RemoteReader, RemoteSettings};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) registry: Arc<AssetRegistry>,
    pub(crate) segment_jobs: Arc<Semaphore>,
    pub(crate) request_slots: Arc<Semaphore>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) ready: Arc<AtomicBool>,
    /// Cleared by the background probe while the mapper is unreachable.
    pub(crate) resolver_healthy: Arc<AtomicBool>,
    pub(crate) cors: Option<CorsLayer>,
    pub(crate) segment_queue_timeout: Duration,
    pub(crate) stream_chunk_bytes: usize,
    pub(crate) max_request_header_bytes: usize,
    pub(crate) request_timeout: Duration,
    pub(crate) response_idle_timeout: Duration,
    pub(crate) max_connections: usize,
    pub(crate) header_read_timeout: Duration,
    /// Seconds between mapper reachability probes; zero disables them.
    pub(crate) probe_interval: Duration,
    pub(crate) preload: bool,
}

impl AppState {
    /// Builds the resolver, clients, and registry. Nothing is loaded yet; see [`Self::preload`].
    pub(crate) fn new(config: &Config) -> Result<Self> {
        let cors = cors_layer(&config.cors)?;
        let metrics = Arc::new(Metrics::new(&ROUTES));
        let remote = &config.remote_media;
        let opener = SourceOpener::new(
            config.media_root.clone(),
            RemoteReader::new(RemoteSettings {
                connect_timeout: Duration::from_millis(remote.connect_timeout_ms),
                request_timeout: Duration::from_millis(remote.request_timeout_ms),
                max_retries: remote.max_retries,
                max_inflight_reads: remote.max_inflight_reads,
                allow_private_addresses: remote.allow_private_addresses,
            })?,
        );
        let (resolver, negative_ttl, error_ttl, stale_if_error, probe_interval) =
            match &config.resolver {
                ResolverSettings::Static => (
                    AssetResolver::Static(StaticResolver::new(config.assets.clone())),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::ZERO,
                    Duration::ZERO,
                ),
                ResolverSettings::Http(mapper) => (
                    AssetResolver::Http(HttpResolver::new(mapper, LocationPolicy::new(remote))?),
                    Duration::from_millis(mapper.negative_ttl_ms),
                    Duration::from_millis(mapper.error_ttl_ms),
                    Duration::from_millis(mapper.stale_if_error_ms),
                    Duration::from_millis(mapper.readiness_probe_interval_ms),
                ),
            };
        let registry = Arc::new(AssetRegistry::new(
            resolver,
            opener,
            config,
            RegistrySettings {
                segment_duration_ms: config.segment_duration_ms,
                max_cached_resolutions: config.registry.max_cached_resolutions,
                negative_ttl,
                error_ttl,
                stale_if_error,
                load_queue_timeout: Duration::from_millis(config.registry.load_queue_timeout_ms),
                max_concurrent_loads: config.limits.max_startup_parses,
                loaded_budget_bytes: config.limits.max_index_bytes,
            },
            Arc::clone(&metrics),
        ));
        Ok(Self {
            registry,
            segment_jobs: Arc::new(Semaphore::new(config.limits.max_segment_jobs)),
            request_slots: Arc::new(Semaphore::new(config.limits.max_concurrent_requests)),
            metrics,
            ready: Arc::new(AtomicBool::new(true)),
            resolver_healthy: Arc::new(AtomicBool::new(true)),
            cors,
            segment_queue_timeout: Duration::from_millis(config.limits.segment_queue_timeout_ms),
            stream_chunk_bytes: config.limits.stream_chunk_bytes,
            max_request_header_bytes: config.limits.max_request_header_bytes,
            request_timeout: Duration::from_millis(config.limits.request_timeout_ms),
            response_idle_timeout: Duration::from_millis(config.limits.response_idle_timeout_ms),
            max_connections: config.limits.max_connections,
            header_read_timeout: Duration::from_millis(config.limits.header_read_timeout_ms),
            probe_interval,
            preload: config.registry.preload,
        })
    }

    /// With the static catalog, loads every asset before serving so a bad file stops startup.
    /// Does nothing for a mapper, whose assets load on first request.
    pub(crate) async fn preload(&self) -> Result<()> {
        if self.preload {
            self.registry.preload().await?;
        }
        Ok(())
    }

    pub(crate) async fn asset(&self, asset_id: &str) -> HttpResult<Arc<PackagedAsset>> {
        Ok(self.registry.get(asset_id).await?)
    }

    pub(crate) async fn segment_permit(&self) -> HttpResult<OwnedSemaphorePermit> {
        acquire_segment_permit(&self.segment_jobs, self.segment_queue_timeout)
            .await
            .map_err(|failure| {
                if matches!(failure, PermitFailure::Timeout) {
                    self.metrics.segment_queue_timeout();
                }
                HttpError::unavailable("segment generation queue timed out")
            })
    }
}

pub(crate) enum PermitFailure {
    Timeout,
    Closed,
}

pub(crate) async fn acquire_segment_permit(
    segment_jobs: &Arc<Semaphore>,
    queue_timeout: Duration,
) -> std::result::Result<OwnedSemaphorePermit, PermitFailure> {
    match timeout(queue_timeout, Arc::clone(segment_jobs).acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(PermitFailure::Closed),
        Err(_) => Err(PermitFailure::Timeout),
    }
}
