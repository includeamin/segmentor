//! Resource limits enforced across parsing, planning, and serving.

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LimitsConfig {
    pub(crate) max_assets: usize,
    pub(crate) max_source_bytes: u64,
    pub(crate) max_metadata_bytes: u64,
    pub(crate) max_fragments: usize,
    pub(crate) metadata_concurrency: usize,
    /// Serve the complete fragments of a fragmented file that ends partway through one, instead
    /// of refusing it.
    pub(crate) tolerate_truncated_tail: bool,
    pub(crate) max_tracks: usize,
    pub(crate) max_samples_per_track: usize,
    pub(crate) max_samples_per_segment: usize,
    pub(crate) max_segment_bytes: u64,
    pub(crate) max_segment_jobs: usize,
    pub(crate) segment_queue_timeout_ms: u64,
    pub(crate) stream_chunk_bytes: usize,
    pub(crate) max_request_header_bytes: usize,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_startup_parses: usize,
    pub(crate) max_concurrent_requests: usize,
    pub(crate) response_idle_timeout_ms: u64,
    pub(crate) max_index_bytes: u64,
    pub(crate) max_connections: usize,
    pub(crate) header_read_timeout_ms: u64,
    /// Sidecar subtitle files per asset.
    pub(crate) max_subtitles: usize,
    /// One subtitle file.
    pub(crate) max_subtitle_bytes: u64,
    /// All of one asset's subtitle files together.
    pub(crate) max_subtitles_total_bytes: u64,
    /// Video renditions an adaptive asset may list.
    pub(crate) max_renditions: usize,
}

impl LimitsConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.max_assets == 0
            || self.max_source_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_fragments == 0
            || self.metadata_concurrency == 0
            || self.max_tracks == 0
            || self.max_samples_per_track == 0
            || self.max_samples_per_segment == 0
            || self.max_segment_bytes == 0
            || self.max_segment_jobs == 0
            || self.segment_queue_timeout_ms == 0
            || self.stream_chunk_bytes == 0
            || self.max_request_header_bytes == 0
            || self.request_timeout_ms == 0
            || self.max_startup_parses == 0
            || self.max_concurrent_requests == 0
            || self.response_idle_timeout_ms == 0
            || self.max_index_bytes == 0
            || self.max_connections == 0
            || self.header_read_timeout_ms == 0
            || self.max_subtitles == 0
            || self.max_subtitle_bytes == 0
            || self.max_subtitles_total_bytes == 0
            || self.max_renditions == 0
        {
            return Err(Error::Configuration(
                "all resource limits must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_assets: 1000,
            max_source_bytes: 1024 * 1024 * 1024 * 1024,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_fragments: 20_000,
            metadata_concurrency: 16,
            tolerate_truncated_tail: false,
            max_tracks: 8,
            max_samples_per_track: 2_000_000,
            max_samples_per_segment: 100_000,
            max_segment_bytes: 64 * 1024 * 1024,
            max_segment_jobs: default_segment_jobs(),
            segment_queue_timeout_ms: 2000,
            stream_chunk_bytes: 256 * 1024,
            max_request_header_bytes: 16 * 1024,
            request_timeout_ms: 30_000,
            max_startup_parses: 4,
            max_concurrent_requests: 10_000,
            response_idle_timeout_ms: 30_000,
            max_index_bytes: 4 * 1024 * 1024 * 1024,
            max_connections: 10_000,
            header_read_timeout_ms: 10_000,
            max_subtitles: 16,
            max_subtitle_bytes: 2 * 1024 * 1024,
            max_subtitles_total_bytes: 8 * 1024 * 1024,
            max_renditions: 8,
        }
    }
}

fn default_segment_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |parallelism| parallelism.get().saturating_mul(2).min(32))
}
