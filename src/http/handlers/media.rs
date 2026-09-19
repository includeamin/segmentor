//! Initialization and media segment handlers.
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::parse_track;
use crate::asset::PackagedAsset;
use crate::http::error::{HttpError, HttpResult};
use crate::http::range::{ByteInterval, range_not_satisfiable, requested_range};
use crate::http::state::AppState;
use crate::http::stream::StreamJob;
use crate::http::validators::{entity_tag, not_modified, not_modified_response};
use crate::media::TrackKind;

/// The `v` query parameter carried by every init and media URL.
#[derive(Debug, Deserialize)]
pub(crate) struct VersionQuery {
    v: Option<String>,
}

impl VersionQuery {
    /// Immutable objects must never be served under a URL naming a different version, or a CDN
    /// would cache new bytes under an old key.
    fn require(&self, asset: &PackagedAsset) -> HttpResult<()> {
        if self.v.as_deref() == Some(asset.version()) {
            Ok(())
        } else {
            Err(HttpError::not_found("asset version does not exist"))
        }
    }
}

pub(crate) async fn init_segment(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    version.require(&asset)?;
    let kind = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("{track}-init"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let bytes = asset.init_segment(kind)?;
    let Ok(range) = requested_range(&headers, bytes.len() as u64, &etag) else {
        return range_not_satisfiable(bytes.len() as u64);
    };
    media_response(&bytes, kind, etag, range)
}

pub(crate) async fn media_segment(
    State(state): State<AppState>,
    method: Method,
    Path((asset_id, track, segment_index)): Path<(String, String, u32)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    version.require(&asset)?;
    let kind = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("{track}-segment-{segment_index}"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let started = Instant::now();
    // Header generation is CPU work proportional to the segment's sample count, so it runs on
    // the blocking pool rather than an async worker.
    let prepared = {
        let asset = Arc::clone(&asset);
        tokio::task::spawn_blocking(move || asset.prepare_media_segment(kind, segment_index))
            .await
            .map_err(|error| HttpError::internal(error.to_string()))??
    };
    let total_length = prepared.content_length;
    let requested_interval = match requested_range(&headers, total_length, &etag) {
        Ok(range) => range.unwrap_or(ByteInterval {
            start: 0,
            end: total_length,
        }),
        Err(()) => return range_not_satisfiable(total_length),
    };
    let content_length = requested_interval.end - requested_interval.start;
    if method == Method::HEAD {
        // Length and range are fully determined by metadata; no source read or job slot needed.
        return media_response_builder(
            total_length,
            requested_interval,
            etag,
            segment_content_type(kind),
        )
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()));
    }

    let permit = state.segment_permit().await?;
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(
        StreamJob {
            asset,
            prepared,
            interval: requested_interval,
            first_permit: Some(permit),
            segment_jobs: Arc::clone(&state.segment_jobs),
            queue_timeout: state.segment_queue_timeout,
            idle_timeout: state.response_idle_timeout,
            chunk_size: state.stream_chunk_bytes,
            metrics: Arc::clone(&state.metrics),
            sender,
        }
        .run(),
    );
    tracing::debug!(
        event = "media_segment_generated",
        asset.id = %asset_id,
        media.track = %track,
        media.segment = segment_index,
        response.bytes = content_length,
        elapsed_us = started.elapsed().as_micros(),
    );
    media_response_builder(
        total_length,
        requested_interval,
        etag,
        segment_content_type(kind),
    )
    .body(Body::from_stream(ReceiverStream::new(receiver)))
    .map_err(|error| HttpError::internal(error.to_string()))
}

const fn segment_content_type(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video/mp4",
        TrackKind::Audio => "audio/mp4",
    }
}

fn media_response(
    bytes: &Bytes,
    kind: TrackKind,
    etag: HeaderValue,
    range: Option<ByteInterval>,
) -> HttpResult<Response> {
    let total = bytes.len() as u64;
    let selected = range.unwrap_or(ByteInterval {
        start: 0,
        end: total,
    });
    let start = usize::try_from(selected.start)
        .map_err(|_| HttpError::internal("media range does not fit in memory".to_owned()))?;
    let end = usize::try_from(selected.end)
        .map_err(|_| HttpError::internal("media range does not fit in memory".to_owned()))?;
    media_response_builder(total, selected, etag, segment_content_type(kind))
        .body(Body::from(bytes.slice(start..end)))
        .map_err(|error| HttpError::internal(error.to_string()))
}

fn media_response_builder(
    total_length: u64,
    range: ByteInterval,
    etag: HeaderValue,
    content_type: &'static str,
) -> axum::http::response::Builder {
    let partial = range.start != 0 || range.end != total_length;
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(CONTENT_TYPE, HeaderValue::from_static(content_type))
        .header(
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        )
        .header(ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .header(axum::http::header::CONTENT_LENGTH, range.end - range.start)
        .header(ETAG, etag);
    if partial {
        builder = builder.header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{total_length}", range.start, range.end - 1),
        );
    }
    builder
}
