//! Initialization and media segment handlers.
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::parse_track;
use crate::asset::PackagedAsset;
use crate::composite::ServedAsset;
use crate::fmp4::PreparedSegment;
use crate::http::error::{HttpError, HttpResult};
use crate::http::range::{ByteInterval, range_not_satisfiable, requested_range};
use crate::http::state::AppState;
use crate::http::stream::StreamJob;
use crate::http::validators::{entity_tag, not_modified, not_modified_response};
use crate::media::TrackKind;
use crate::protocol::hls::MUXED;
use crate::source::ByteRange;

/// The `v` query parameter carried by every init and media URL.
#[derive(Debug, Deserialize)]
pub(crate) struct VersionQuery {
    v: Option<String>,
}

impl VersionQuery {
    /// Immutable objects must never be served under a URL naming a different version, or a CDN
    /// would cache new bytes under an old key.
    fn require(&self, version: &str) -> HttpResult<()> {
        if self.v.as_deref() == Some(version) {
            Ok(())
        } else {
            Err(HttpError::not_found("asset version does not exist"))
        }
    }
}

/// Whether `track` names the muxed HLS stream (TDD 0011). It exists only under `/hls/`: the
/// DASH routes share these handlers, and DASH never offers it.
fn is_muxed(track: &str, uri: &Uri) -> HttpResult<bool> {
    if track != MUXED {
        return Ok(false);
    }
    if uri.path().starts_with("/hls/") {
        Ok(true)
    } else {
        Err(HttpError::not_found("track does not exist"))
    }
}

pub(crate) async fn init_segment(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    Query(version): Query<VersionQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    version.require(asset.version())?;
    if is_muxed(&track, &uri)? {
        let etag = entity_tag(asset.version(), "muxed-init");
        if not_modified(&headers, &etag) {
            return not_modified_response(etag, "public, max-age=31536000, immutable");
        }
        let bytes = asset.muxed_init_segment()?;
        let Ok(range) = requested_range(&headers, bytes.len() as u64, &etag) else {
            return range_not_satisfiable(bytes.len() as u64);
        };
        return media_response(&bytes, TrackKind::Video, etag, range);
    }
    let requested = parse_track(&track)?;
    let etag = entity_tag(asset.version(), &format!("{track}-init"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let bytes = asset.init_segment(requested.rendition.as_deref(), requested.key)?;
    let Ok(range) = requested_range(&headers, bytes.len() as u64, &etag) else {
        return range_not_satisfiable(bytes.len() as u64);
    };
    media_response(&bytes, requested.key.kind, etag, range)
}

pub(crate) async fn media_segment(
    State(state): State<AppState>,
    method: Method,
    Path((asset_id, track, segment_index)): Path<(String, String, u32)>,
    Query(version): Query<VersionQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    version.require(asset.version())?;
    let etag = entity_tag(asset.version(), &format!("{track}-segment-{segment_index}"));
    if is_muxed(&track, &uri)? {
        let what = format!("{asset_id}/{track}/segments/{segment_index}");
        return serve_segment(
            &state,
            &method,
            &headers,
            asset,
            etag,
            TrackKind::Video,
            what,
            move |asset| asset.prepare_muxed_segment(segment_index),
        )
        .await;
    }
    let requested = parse_track(&track)?;
    let kind = requested.key.kind;
    let what = format!("{asset_id}/{track}/segments/{segment_index}");
    serve_segment(
        &state,
        &method,
        &headers,
        asset,
        etag,
        kind,
        what,
        move |asset| {
            asset.prepare_media_segment(
                requested.rendition.as_deref(),
                requested.key,
                segment_index,
            )
        },
    )
    .await
}

/// One clip's init segment, for a sequence: its clips may be encoded differently, so each has its
/// own (TDD 0008, "URLs").
pub(crate) async fn clip_init_segment(
    State(state): State<AppState>,
    Path((asset_id, track, clip)): Path<(String, String, u32)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    version.require(asset.version())?;
    let requested = parse_track(&track)?;
    let etag = entity_tag(asset.version(), &format!("{track}-clip-{clip}-init"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let clip = usize::try_from(clip).map_err(|_| HttpError::not_found("clip does not exist"))?;
    let bytes = asset.clip_init_segment(requested.rendition.as_deref(), requested.key, clip)?;
    let Ok(range) = requested_range(&headers, bytes.len() as u64, &etag) else {
        return range_not_satisfiable(bytes.len() as u64);
    };
    media_response(&bytes, requested.key.kind, etag, range)
}

/// A sidecar `WebVTT` file, served under both protocols.
pub(crate) async fn subtitle_file(
    State(state): State<AppState>,
    Path((asset_id, language)): Path<(String, String)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    version.require(asset.version())?;
    let etag = entity_tag(
        asset.version(),
        &format!("subtitle-{}", language.to_ascii_lowercase()),
    );
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let bytes = asset.subtitle(&language)?;
    let total = bytes.len() as u64;
    let Ok(range) = requested_range(&headers, total, &etag) else {
        return range_not_satisfiable(total);
    };
    let selected = range.unwrap_or(ByteInterval {
        start: 0,
        end: total,
    });
    let (start, end) = (
        usize::try_from(selected.start).map_err(|_| HttpError::internal("range".to_owned()))?,
        usize::try_from(selected.end).map_err(|_| HttpError::internal("range".to_owned()))?,
    );
    media_response_builder(total, selected, etag, "text/vtt; charset=utf-8")
        .body(Body::from(bytes.slice(start..end)))
        .map_err(|error| HttpError::internal(error.to_string()))
}

/// One keyframe of the video track as a fragment of its own, for HLS I-frame playlists.
pub(crate) async fn iframe_segment(
    State(state): State<AppState>,
    method: Method,
    Path((asset_id, frame_index)): Path<(String, u32)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    version.require(asset.version())?;
    let etag = entity_tag(asset.version(), &format!("iframe-{frame_index}"));
    let what = format!("{asset_id}/video/iframes/{frame_index}");
    serve_segment(
        &state,
        &method,
        &headers,
        asset,
        etag,
        TrackKind::Video,
        what,
        move |asset| asset.prepare_iframe(frame_index),
    )
    .await
}

/// Reads an encrypted track's samples, encrypts them off the async workers, and returns the
/// finished fragment as a header-only segment, so ranges, `HEAD`, and streaming from here on are
/// the clear path's (TDD 0009, "The encrypted segment path").
///
/// The returned job slot covers the encrypted bytes for as long as they live: the caller hands
/// it to the stream, which releases it when the response finishes or is aborted.
async fn encrypt_segment(
    state: &AppState,
    source: &PackagedAsset,
    ranges: Vec<ByteRange>,
    pending: crate::cenc::PendingEncryption,
    what: &str,
) -> HttpResult<(PreparedSegment, OwnedSemaphorePermit)> {
    let started = Instant::now();
    let permit = state.segment_permit().await?;
    let length = ranges
        .iter()
        .try_fold(0u64, |total, range| total.checked_add(range.length))
        .and_then(|total| usize::try_from(total).ok())
        .and_then(|total| total.checked_add(pending.header_room()))
        .ok_or_else(|| {
            HttpError::internal("encrypted segment does not fit in memory".to_owned())
        })?;
    let mut payload = Vec::with_capacity(length);
    for range in ranges {
        match source.read_range(range).await {
            Ok(bytes) => payload.extend_from_slice(&bytes),
            Err(error) => {
                state.metrics.encryption_failed();
                return Err(error.into());
            }
        }
    }
    let finished = match tokio::task::spawn_blocking(move || pending.finish(payload)).await {
        Ok(finished) => finished,
        Err(error) => {
            state.metrics.encryption_failed();
            return Err(HttpError::internal(error.to_string()));
        }
    };
    match finished {
        Ok(bytes) => {
            state.metrics.encrypted_segment(started.elapsed());
            let prepared = PreparedSegment {
                content_length: bytes.len() as u64,
                header: bytes,
                ranges: Vec::new(),
                encryption: None,
            };
            Ok((prepared, permit))
        }
        Err(error) => {
            state.metrics.encryption_failed();
            tracing::error!(event = "segment_encryption_failed", segment = what, %error);
            Err(error.into())
        }
    }
}

/// Answers a request for a generated fragment: conditional, ranged, and streamed from the source
/// through a bounded queue.
#[allow(
    clippy::too_many_arguments,
    reason = "one call shape for both segment routes; a parameter struct would only rename them"
)]
async fn serve_segment(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    asset: Arc<ServedAsset>,
    etag: HeaderValue,
    kind: TrackKind,
    what: String,
    prepare: impl FnOnce(&ServedAsset) -> crate::error::Result<(Arc<PackagedAsset>, PreparedSegment)>
    + Send
    + 'static,
) -> HttpResult<Response> {
    if not_modified(headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let started = Instant::now();
    // Header generation is CPU work proportional to the segment's sample count, so it runs on
    // the blocking pool rather than an async worker. `source` is the specific underlying asset
    // (a composite has several) whose bytes the prepared fragment's offsets refer to.
    let (source, mut prepared) = {
        let asset = Arc::clone(&asset);
        tokio::task::spawn_blocking(move || prepare(&asset))
            .await
            .map_err(|error| HttpError::internal(error.to_string()))??
    };
    // An encrypted segment arrives with the job slot that covers its bytes in memory.
    let (prepared, encrypted_permit) = match prepared.encryption.take() {
        None => (prepared, None),
        Some(pending) => {
            let (prepared, permit) =
                encrypt_segment(state, &source, prepared.ranges, *pending, &what).await?;
            (prepared, Some(permit))
        }
    };
    let total_length = prepared.content_length;
    let requested_interval = match requested_range(headers, total_length, &etag) {
        Ok(range) => range.unwrap_or(ByteInterval {
            start: 0,
            end: total_length,
        }),
        Err(()) => return range_not_satisfiable(total_length),
    };
    let content_length = requested_interval.end - requested_interval.start;
    if method == Method::HEAD {
        // Length and range are known without streaming. A clear segment needs no source read or
        // job slot; an encrypted one was read and encrypted to learn its length, and its slot is
        // released as this returns.
        return media_response_builder(
            total_length,
            requested_interval,
            etag,
            segment_content_type(kind),
        )
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()));
    }

    let permit = match encrypted_permit {
        Some(permit) => permit,
        None => state.segment_permit().await?,
    };
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(
        StreamJob {
            asset: source,
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
        media.kind = ?kind,
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
