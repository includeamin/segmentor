//! Shared application state and startup loading.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rayon::prelude::*;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, timeout};
use tower_http::cors::CorsLayer;

use super::cors::cors_layer;
use super::error::{HttpError, HttpResult};
use super::router::ROUTES;
use crate::asset::PackagedAsset;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::observability::metrics::Metrics;

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) assets: Arc<HashMap<String, Arc<PackagedAsset>>>,
    pub(crate) segment_jobs: Arc<Semaphore>,
    pub(crate) request_slots: Arc<Semaphore>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) ready: Arc<AtomicBool>,
    pub(crate) cors: Option<CorsLayer>,
    pub(crate) segment_queue_timeout: Duration,
    pub(crate) stream_chunk_bytes: usize,
    pub(crate) max_request_header_bytes: usize,
    pub(crate) request_timeout: Duration,
    pub(crate) response_idle_timeout: Duration,
    pub(crate) max_connections: usize,
    pub(crate) header_read_timeout: Duration,
}

impl AppState {
    pub(crate) fn load(config: &Config) -> Result<Self> {
        let cors = cors_layer(&config.cors)?;
        let parse_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.limits.max_startup_parses)
            .thread_name(|index| format!("vod-startup-parser-{index}"))
            .build()
            .map_err(|error| Error::Configuration(error.to_string()))?;
        let assets = parse_pool.install(|| {
            config
                .assets
                .par_iter()
                .map(|(asset_id, path)| {
                    let started = Instant::now();
                    tracing::debug!(event = "asset_load_started", asset.id = %asset_id, asset.path = %path.display());
                    let asset = Arc::new(PackagedAsset::load(
                        path,
                        config.segment_duration_ms,
                        &config.limits,
                    )?);
                    tracing::info!(
                        event = "asset_loaded",
                        asset.id = %asset_id,
                        media.tracks = asset.index.tracks.len(),
                        media.segments = asset.plan.segments.len(),
                        index.bytes = asset.index_bytes(),
                        elapsed_ms = started.elapsed().as_millis(),
                    );
                    Ok((asset_id.clone(), asset))
                })
                .collect::<Result<HashMap<_, _>>>()
        })?;
        let index_bytes = assets
            .values()
            .map(|asset| asset.index_bytes())
            .fold(0u64, u64::saturating_add);
        if index_bytes > config.limits.max_index_bytes {
            return Err(Error::Configuration(format!(
                "loaded asset indexes need {index_bytes} bytes, exceeding limits.max_index_bytes {}",
                config.limits.max_index_bytes
            )));
        }
        Ok(Self {
            assets: Arc::new(assets),
            segment_jobs: Arc::new(Semaphore::new(config.limits.max_segment_jobs)),
            request_slots: Arc::new(Semaphore::new(config.limits.max_concurrent_requests)),
            metrics: Arc::new(Metrics::new(&ROUTES)),
            ready: Arc::new(AtomicBool::new(true)),
            cors,
            segment_queue_timeout: Duration::from_millis(config.limits.segment_queue_timeout_ms),
            stream_chunk_bytes: config.limits.stream_chunk_bytes,
            max_request_header_bytes: config.limits.max_request_header_bytes,
            request_timeout: Duration::from_millis(config.limits.request_timeout_ms),
            response_idle_timeout: Duration::from_millis(config.limits.response_idle_timeout_ms),
            max_connections: config.limits.max_connections,
            header_read_timeout: Duration::from_millis(config.limits.header_read_timeout_ms),
        })
    }

    pub(crate) fn asset(&self, asset_id: &str) -> HttpResult<Arc<PackagedAsset>> {
        self.assets
            .get(asset_id)
            .cloned()
            .ok_or_else(|| HttpError::not_found("asset does not exist"))
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
