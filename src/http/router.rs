//! Route table and middleware ordering.
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use super::handlers::{
    admin_status, dash_manifest, health, iframe_playlist, iframe_segment, init_segment,
    master_playlist, media_playlist, media_segment, metrics, ready, subtitle_file,
    subtitle_playlist,
};
use super::middleware::{
    X_REQUEST_ID, enforce_header_limit, record_metrics, request_id, shed_load,
};
use super::state::AppState;

const HEALTH: &str = "/health";
const ADMIN_STATUS: &str = "/admin/status";
const READY: &str = "/ready";
const METRICS: &str = "/metrics";
const HLS_MASTER: &str = "/hls/{asset_id}/master.m3u8";
const HLS_MEDIA_PLAYLIST: &str = "/hls/{asset_id}/{track}/index.m3u8";
const HLS_IFRAME_PLAYLIST: &str = "/hls/{asset_id}/video/iframes.m3u8";
const HLS_IFRAME_SEGMENT: &str = "/hls/{asset_id}/video/iframes/{frame_index}/media.m4s";
const HLS_SUBTITLE_PLAYLIST: &str = "/hls/{asset_id}/subtitles/{language}/index.m3u8";
const HLS_SUBTITLE_FILE: &str = "/hls/{asset_id}/subtitles/{language}/sub.vtt";
const DASH_SUBTITLE_FILE: &str = "/dash/{asset_id}/subtitles/{language}/sub.vtt";
const HLS_INIT: &str = "/hls/{asset_id}/{track}/init.mp4";
const HLS_SEGMENT: &str = "/hls/{asset_id}/{track}/segments/{segment_index}/media.m4s";
const DASH_MANIFEST: &str = "/dash/{asset_id}/manifest.mpd";
const DASH_INIT: &str = "/dash/{asset_id}/{track}/init.mp4";
const DASH_SEGMENT: &str = "/dash/{asset_id}/{track}/segments/{segment_index}/media.m4s";

/// Every route template, used both to register handlers and to label metrics, so a route
/// cannot be added to one and forgotten in the other.
pub(crate) const ROUTES: [&str; 16] = [
    HEALTH,
    ADMIN_STATUS,
    READY,
    METRICS,
    HLS_MASTER,
    HLS_MEDIA_PLAYLIST,
    HLS_IFRAME_PLAYLIST,
    HLS_IFRAME_SEGMENT,
    HLS_SUBTITLE_PLAYLIST,
    HLS_SUBTITLE_FILE,
    DASH_SUBTITLE_FILE,
    HLS_INIT,
    HLS_SEGMENT,
    DASH_MANIFEST,
    DASH_INIT,
    DASH_SEGMENT,
];

pub(crate) fn router(state: AppState) -> Router {
    let router = Router::new()
        .route(HEALTH, get(health))
        .route(ADMIN_STATUS, get(admin_status))
        .route(READY, get(ready))
        .route(METRICS, get(metrics))
        .route(HLS_MASTER, get(master_playlist))
        .route(HLS_MEDIA_PLAYLIST, get(media_playlist))
        .route(HLS_IFRAME_PLAYLIST, get(iframe_playlist))
        .route(HLS_IFRAME_SEGMENT, get(iframe_segment))
        .route(HLS_SUBTITLE_PLAYLIST, get(subtitle_playlist))
        .route(HLS_SUBTITLE_FILE, get(subtitle_file))
        .route(DASH_SUBTITLE_FILE, get(subtitle_file))
        .route(HLS_INIT, get(init_segment))
        .route(HLS_SEGMENT, get(media_segment))
        .route(DASH_MANIFEST, get(dash_manifest))
        .route(DASH_INIT, get(init_segment))
        .route(DASH_SEGMENT, get(media_segment))
        // Layers run outermost-last: timeout is innermost, request IDs outermost. CORS sits
        // outside the limit layers so 408, 431, and 503 responses stay readable by browsers.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            state.request_timeout,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_header_limit,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), shed_load));
    let router = match state.cors.clone() {
        Some(cors) => router.layer(cors),
        None => router,
    };
    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_metrics,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<Body>| {
                    let request_id = request
                        .headers()
                        .get(&X_REQUEST_ID)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default();
                    tracing::info_span!(
                        "request",
                        http.method = %request.method(),
                        http.path = %request.uri().path(),
                        request_id,
                    )
                })
                .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                .on_response(DefaultOnResponse::new().level(Level::DEBUG))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .layer(middleware::from_fn(request_id))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::ROUTES;

    #[test]
    fn route_templates_are_unique() {
        let mut sorted = ROUTES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ROUTES.len());
    }
}
