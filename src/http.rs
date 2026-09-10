use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{
    DefaultMakeSpan, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer,
};
use tracing::Level;

use crate::asset::PackagedAsset;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::media::TrackKind;
use crate::{APP_NAME, hls};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    assets: Arc<HashMap<String, Arc<PackagedAsset>>>,
}

impl AppState {
    pub(crate) fn load(config: &Config) -> Result<Self> {
        let mut assets = HashMap::with_capacity(config.assets.len());
        for (asset_id, path) in &config.assets {
            let started = Instant::now();
            tracing::debug!(event = "asset_load_started", asset.id = %asset_id, asset.path = %path.display());
            let asset = Arc::new(PackagedAsset::load(path, config.segment_duration_ms)?);
            tracing::info!(
                event = "asset_loaded",
                asset.id = %asset_id,
                media.tracks = asset.index.tracks.len(),
                media.segments = asset.plan.segments.len(),
                elapsed_ms = started.elapsed().as_millis(),
            );
            assets.insert(asset_id.clone(), asset);
        }
        Ok(Self {
            assets: Arc::new(assets),
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
        .route("/hls/{asset_id}/master.m3u8", get(master_playlist))
        .route("/hls/{asset_id}/{track}/index.m3u8", get(media_playlist))
        .route("/hls/{asset_id}/{track}/init.mp4", get(init_segment))
        .route(
            "/hls/{asset_id}/{track}/segments/{segment_index}/media.m4s",
            get(media_segment),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::HEAD]),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::DEBUG))
                .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                .on_response(DefaultOnResponse::new().level(Level::DEBUG))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

async fn health() -> &'static str {
    "ok\n"
}

async fn master_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    playlist_response(hls::master_playlist(&asset)?)
}

async fn media_playlist(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    playlist_response(hls::media_playlist(&asset, parse_track(&track)?)?)
}

async fn init_segment(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    media_response(asset.init_segment(parse_track(&track)?)?)
}

async fn media_segment(
    State(state): State<AppState>,
    Path((asset_id, track, segment_index)): Path<(String, String, u32)>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id)?;
    let kind = parse_track(&track)?;
    let started = Instant::now();
    let bytes = tokio::task::spawn_blocking(move || asset.media_segment(kind, segment_index))
        .await
        .map_err(|error| HttpError::internal(error.to_string()))??;
    tracing::debug!(
        event = "media_segment_generated",
        asset.id = %asset_id,
        media.track = %track,
        media.segment = segment_index,
        response.bytes = bytes.len(),
        elapsed_us = started.elapsed().as_micros(),
    );
    media_response(bytes)
}

fn parse_track(track: &str) -> HttpResult<TrackKind> {
    match track {
        "audio" => Ok(TrackKind::Audio),
        "video" => Ok(TrackKind::Video),
        _ => Err(HttpError::not_found("track does not exist")),
    }
}

fn playlist_response(playlist: String) -> HttpResult<Response> {
    response(
        Body::from(playlist),
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
        HeaderValue::from_static("public, max-age=60"),
    )
}

fn media_response(bytes: bytes::Bytes) -> HttpResult<Response> {
    response(
        Body::from(bytes),
        HeaderValue::from_static("video/mp4"),
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    )
}

fn response(
    body: Body,
    content_type: HeaderValue,
    cache_control: HeaderValue,
) -> HttpResult<Response> {
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, cache_control)
        .body(body)
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
        if self.status.is_server_error() {
            tracing::error!(
                event = "request_failed",
                http.status = self.status.as_u16(),
                error = %self.message,
            );
        } else {
            tracing::warn!(
                event = "request_rejected",
                http.status = self.status.as_u16(),
                error = %self.message,
            );
        }
        (self.status, self.message).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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
