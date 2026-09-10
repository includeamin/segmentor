use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rayon::prelude::*;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{Duration, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{
    DefaultMakeSpan, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer,
};
use tracing::Level;

use crate::asset::PackagedAsset;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::media::TrackKind;
use crate::{APP_NAME, dash, hls};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    assets: Arc<HashMap<String, Arc<PackagedAsset>>>,
    segment_jobs: Arc<Semaphore>,
    segment_queue_timeout: Duration,
    stream_chunk_bytes: usize,
    max_request_header_bytes: usize,
    request_timeout: Duration,
}

impl AppState {
    pub(crate) fn load(config: &Config) -> Result<Self> {
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
                        elapsed_ms = started.elapsed().as_millis(),
                    );
                    Ok((asset_id.clone(), asset))
                })
                .collect::<Result<HashMap<_, _>>>()
        })?;
        Ok(Self {
            assets: Arc::new(assets),
            segment_jobs: Arc::new(Semaphore::new(config.limits.max_segment_jobs)),
            segment_queue_timeout: Duration::from_millis(config.limits.segment_queue_timeout_ms),
            stream_chunk_bytes: config.limits.stream_chunk_bytes,
            max_request_header_bytes: config.limits.max_request_header_bytes,
            request_timeout: Duration::from_millis(config.limits.request_timeout_ms),
        })
    }

    fn asset(&self, asset_id: &str) -> HttpResult<Arc<PackagedAsset>> {
        self.assets
            .get(asset_id)
            .cloned()
            .ok_or_else(|| HttpError::not_found("asset does not exist"))
    }
}

pub(crate) async fn serve(config: Config) -> Result<()> {
    tracing::info!(
        event = "service_starting",
        listen.address = %config.listen,
        assets.count = config.assets.len(),
        packaging.segment_duration_ms = config.segment_duration_ms,
    );
    let state = AppState::load(&config)?;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(
        event = "service_ready",
        service.name = APP_NAME,
        listen.address = %listener.local_addr()?,
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!(event = "service_stopped", service.name = APP_NAME);
    Ok(())
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/hls/{asset_id}/master.m3u8", get(master_playlist))
        .route("/hls/{asset_id}/{track}/index.m3u8", get(media_playlist))
        .route("/hls/{asset_id}/{track}/init.mp4", get(init_segment))
        .route(
            "/hls/{asset_id}/{track}/segments/{segment_index}/media.m4s",
            get(media_segment),
        )
        .route("/dash/{asset_id}/manifest.mpd", get(dash_manifest))
        .route("/dash/{asset_id}/{track}/init.mp4", get(init_segment))
        .route(
            "/dash/{asset_id}/{track}/segments/{segment_index}/media.m4s",
            get(media_segment),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::HEAD]),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            state.request_timeout,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_header_limit,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::DEBUG))
                .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                .on_response(DefaultOnResponse::new().level(Level::DEBUG))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

async fn enforce_header_limit(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let header_bytes = request
        .headers()
        .iter()
        .fold(0usize, |total, (name, value)| {
            total
                .saturating_add(name.as_str().len())
                .saturating_add(value.as_bytes().len())
        });
    if header_bytes > state.max_request_header_bytes {
        return (
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request headers are too large",
        )
            .into_response();
    }
    next.run(request).await
}

async fn health() -> &'static str {
    "ok\n"
}

async fn metrics() -> Response {
    let body = format!(
        "# HELP vod_log_dropped_lines_total Log records dropped by the non-blocking writer.\n# TYPE vod_log_dropped_lines_total counter\nvod_log_dropped_lines_total {}\n",
        crate::logging::dropped_lines()
    );
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn master_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let etag = entity_tag(&asset, "hls-master");
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    playlist_response(hls::master_playlist(&asset)?, etag)
}

async fn media_playlist(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let kind = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("hls-{track}-playlist"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    playlist_response(hls::media_playlist(&asset, kind)?, etag)
}

async fn dash_manifest(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let etag = entity_tag(&asset, "dash-manifest");
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    manifest_response(dash::manifest(&asset)?, etag)
}

async fn init_segment(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let kind = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("{track}-init"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let bytes = asset.init_segment(kind)?;
    let Ok(range) = requested_range(&headers, bytes.len() as u64) else {
        return range_not_satisfiable(bytes.len() as u64);
    };
    media_response(&bytes, etag, range)
}

async fn media_segment(
    State(state): State<AppState>,
    Path((asset_id, track, segment_index)): Path<(String, String, u32)>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let kind = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("{track}-segment-{segment_index}"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=31536000, immutable");
    }
    let started = Instant::now();
    let prepared = asset.prepare_media_segment(kind, segment_index)?;
    let total_length = prepared.content_length;
    let requested_interval = match requested_range(&headers, total_length) {
        Ok(range) => range.unwrap_or(ByteInterval {
            start: 0,
            end: total_length,
        }),
        Err(()) => return range_not_satisfiable(total_length),
    };
    let content_length = requested_interval.end - requested_interval.start;
    let permit = timeout(
        state.segment_queue_timeout,
        Arc::clone(&state.segment_jobs).acquire_owned(),
    )
    .await
    .map_err(|_| HttpError::unavailable("segment generation queue timed out"))?
    .map_err(|error| HttpError::internal(error.to_string()))?;
    let (sender, receiver) = mpsc::channel(2);
    let chunk_size = state.stream_chunk_bytes;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let header_end = prepared.header.len() as u64;
        if let Some(overlap) = requested_interval.overlap(0, header_end) {
            let (Ok(start), Ok(end)) =
                (usize::try_from(overlap.start), usize::try_from(overlap.end))
            else {
                let _ = sender.blocking_send(Err(std::io::Error::other(
                    "header range does not fit in memory",
                )));
                return;
            };
            let chunk = prepared.header.slice(start..end);
            if sender.blocking_send(Ok(chunk)).is_err() {
                return;
            }
        }
        let mut virtual_offset = header_end;
        for source_range in prepared.ranges {
            let source_end = virtual_offset + source_range.length;
            let Some(overlap) = requested_interval.overlap(virtual_offset, source_end) else {
                virtual_offset = source_end;
                continue;
            };
            let mut offset = source_range.offset + overlap.start - virtual_offset;
            let mut remaining = overlap.end - overlap.start;
            while remaining != 0 {
                let length = remaining.min(chunk_size as u64);
                let chunk = asset
                    .read_range(crate::source::ByteRange::new(offset, length))
                    .map_err(|error| std::io::Error::other(error.to_string()));
                if sender.blocking_send(chunk).is_err() {
                    return;
                }
                offset += length;
                remaining -= length;
            }
            virtual_offset = source_end;
        }
    });
    tracing::debug!(
        event = "media_segment_generated",
        asset.id = %asset_id,
        media.track = %track,
        media.segment = segment_index,
        response.bytes = content_length,
        elapsed_us = started.elapsed().as_micros(),
    );
    streaming_media_response(
        ReceiverStream::new(receiver),
        total_length,
        requested_interval,
        etag,
    )
}

fn parse_track(track: &str) -> HttpResult<TrackKind> {
    match track {
        "audio" => Ok(TrackKind::Audio),
        "video" => Ok(TrackKind::Video),
        _ => Err(HttpError::not_found("track does not exist")),
    }
}

fn playlist_response(playlist: String, etag: HeaderValue) -> HttpResult<Response> {
    response(
        Body::from(playlist),
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
        HeaderValue::from_static("public, max-age=60"),
        etag,
    )
}

fn manifest_response(manifest: String, etag: HeaderValue) -> HttpResult<Response> {
    response(
        Body::from(manifest),
        HeaderValue::from_static("application/dash+xml"),
        HeaderValue::from_static("public, max-age=60"),
        etag,
    )
}

fn media_response(
    bytes: &bytes::Bytes,
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
    media_response_builder(total, selected, etag)
        .body(Body::from(bytes.slice(start..end)))
        .map_err(|error| HttpError::internal(error.to_string()))
}

fn streaming_media_response(
    stream: impl tokio_stream::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static,
    total_length: u64,
    range: ByteInterval,
    etag: HeaderValue,
) -> HttpResult<Response> {
    media_response_builder(total_length, range, etag)
        .body(Body::from_stream(stream))
        .map_err(|error| HttpError::internal(error.to_string()))
}

fn media_response_builder(
    total_length: u64,
    range: ByteInterval,
    etag: HeaderValue,
) -> axum::http::response::Builder {
    let partial = range.start != 0 || range.end != total_length;
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(CONTENT_TYPE, HeaderValue::from_static("video/mp4"))
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

fn response(
    body: Body,
    content_type: HeaderValue,
    cache_control: HeaderValue,
    etag: HeaderValue,
) -> HttpResult<Response> {
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, cache_control)
        .header(ETAG, etag)
        .body(body)
        .map_err(|error| HttpError::internal(error.to_string()))
}

fn entity_tag(asset: &PackagedAsset, resource: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{}-{resource}\"", asset.version()))
        .expect("asset versions and internal resource names are valid header values")
}

fn not_modified(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .is_some_and(|value| value == etag || value == "*")
}

fn not_modified_response(etag: HeaderValue, cache_control: &'static str) -> HttpResult<Response> {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(ETAG, etag)
        .header(CACHE_CONTROL, HeaderValue::from_static(cache_control))
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()))
}

#[derive(Debug, Clone, Copy)]
struct ByteInterval {
    start: u64,
    end: u64,
}

impl ByteInterval {
    fn overlap(self, start: u64, end: u64) -> Option<Self> {
        let overlap = Self {
            start: self.start.max(start),
            end: self.end.min(end),
        };
        (overlap.start < overlap.end).then_some(overlap)
    }
}

fn requested_range(
    headers: &HeaderMap,
    total: u64,
) -> std::result::Result<Option<ByteInterval>, ()> {
    let Some(value) = headers.get(RANGE) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        return Err(());
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    let end = if end.is_empty() {
        total
    } else {
        end.parse::<u64>()
            .map_err(|_| ())?
            .checked_add(1)
            .ok_or(())?
            .min(total)
    };
    if start >= end || start >= total {
        return Err(());
    }
    Ok(Some(ByteInterval { start, end }))
}

fn range_not_satisfiable(total: u64) -> HttpResult<Response> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(CONTENT_RANGE, format!("bytes */{total}"))
        .header(ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for shutdown signal");
    }
}

type HttpResult<T> = std::result::Result<T, HttpError>;

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.to_owned(),
        }
    }

    fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }

    fn unavailable(message: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.to_owned(),
        }
    }
}

impl From<Error> for HttpError {
    fn from(error: Error) -> Self {
        match error {
            Error::InvalidMedia(message) if message == "segment does not exist" => {
                Self::not_found(&message)
            }
            Error::Unsupported(message) if message == "requested track is not present" => {
                Self::not_found(message)
            }
            error => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let public_message = if self.status.is_server_error() {
            tracing::error!(
                event = "request_failed",
                http.status = self.status.as_u16(),
                error = %self.message,
            );
            if self.status == StatusCode::SERVICE_UNAVAILABLE {
                "service unavailable"
            } else {
                "internal server error"
            }
        } else {
            tracing::warn!(
                event = "request_rejected",
                http.status = self.status.as_u16(),
                error = %self.message,
            );
            &self.message
        };
        (self.status, public_message.to_owned()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::process::Command;

    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn app() -> Router {
        let mut assets = BTreeMap::new();
        assets.insert(
            "sample".to_owned(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4"),
        );
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            segment_duration_ms: 1000,
            assets,
            logging: crate::config::LoggingConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };
        router(AppState::load(&config).expect("fixture should load"))
    }

    #[tokio::test]
    async fn serves_master_playlist_and_media_objects() {
        let master = app()
            .oneshot(
                Request::get("/hls/sample/master.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(master.status(), StatusCode::OK);
        assert_eq!(
            master.headers()[CONTENT_TYPE],
            "application/vnd.apple.mpegurl"
        );
        let master_body = to_bytes(master.into_body(), usize::MAX).await.unwrap();
        assert!(
            master_body
                .windows(16)
                .any(|window| window == b"video/index.m3u8")
        );

        let init = app()
            .oneshot(
                Request::get("/hls/sample/video/init.mp4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(init.status(), StatusCode::OK);
        let init_body = to_bytes(init.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&init_body[4..8], b"ftyp");

        let segment = app()
            .oneshot(
                Request::get("/hls/sample/video/segments/0/media.m4s")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(segment.status(), StatusCode::OK);
        let segment_body = to_bytes(segment.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&segment_body[4..8], b"moof");
    }

    #[tokio::test]
    async fn serves_dash_manifest() {
        let response = app()
            .oneshot(
                Request::get("/dash/sample/manifest.mpd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/dash+xml");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.windows(11).any(|window| window == b"<Adaptation"));
    }

    #[tokio::test]
    async fn returns_not_modified_for_matching_media_etag() {
        let uri = "/hls/sample/video/segments/0/media.m4s";
        let first = app()
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let etag = first.headers()[ETAG].clone();

        let response = app()
            .oneshot(
                Request::get(uri)
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn serves_single_media_byte_ranges() {
        let response = app()
            .oneshot(
                Request::get("/hls/sample/video/segments/0/media.m4s")
                    .header(RANGE, "bytes=4-7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(
            response.headers()[CONTENT_RANGE]
                .to_str()
                .unwrap()
                .starts_with("bytes 4-7/")
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            b"moof".as_slice()
        );
    }

    #[tokio::test]
    async fn rejects_unsatisfiable_media_ranges() {
        let response = app()
            .oneshot(
                Request::get("/hls/sample/video/init.mp4")
                    .header(RANGE, "bytes=999999-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(
            response.headers()[CONTENT_RANGE]
                .to_str()
                .unwrap()
                .starts_with("bytes */")
        );
    }

    #[tokio::test]
    async fn rejects_headers_over_the_configured_limit() {
        let response = app()
            .oneshot(
                Request::get("/health")
                    .header("x-oversized", "x".repeat(17 * 1024))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn exposes_dropped_log_metric() {
        let response = app()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            body.windows(27)
                .any(|window| window == b"vod_log_dropped_lines_total")
        );
    }

    #[tokio::test]
    async fn ffmpeg_decodes_hls_and_dash_presentations() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping media validation because ffmpeg is unavailable");
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app()).await.unwrap();
        });

        for path in ["hls/sample/master.m3u8", "dash/sample/manifest.mpd"] {
            let url = format!("http://{address}/{path}");
            let output = tokio::task::spawn_blocking(move || {
                Command::new("ffmpeg")
                    .args(["-v", "error", "-i", &url, "-f", "null", "-"])
                    .output()
            })
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        server.abort();
    }

    #[tokio::test]
    async fn returns_not_found_for_unknown_assets_and_segments() {
        for uri in [
            "/hls/unknown/master.m3u8",
            "/hls/sample/video/segments/99/media.m4s",
            "/hls/sample/subtitle/index.m3u8",
        ] {
            let response = app()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
