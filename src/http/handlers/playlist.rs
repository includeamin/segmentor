//! HLS playlists and the DASH manifest.
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use bytes::Bytes;

use super::parse_track;
use crate::http::error::{HttpError, HttpResult};
use crate::http::state::AppState;
use crate::http::validators::{entity_tag, not_modified, not_modified_response};

pub(crate) async fn master_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    let etag = entity_tag(&asset, "hls-master");
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    playlist_response(asset.hls_master_playlist(), etag)
}

pub(crate) async fn media_playlist(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    let key = parse_track(&track)?;
    let etag = entity_tag(&asset, &format!("hls-{track}-playlist"));
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    playlist_response(asset.hls_media_playlist(key)?, etag)
}

pub(crate) async fn iframe_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    let etag = entity_tag(&asset, "hls-iframe-playlist");
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    playlist_response(asset.hls_iframe_playlist()?, etag)
}

pub(crate) async fn dash_manifest(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    let etag = entity_tag(&asset, "dash-manifest");
    if not_modified(&headers, &etag) {
        return not_modified_response(etag, "public, max-age=60");
    }
    manifest_response(asset.dash_manifest(), etag)
}

fn playlist_response(playlist: Bytes, etag: HeaderValue) -> HttpResult<Response> {
    response(
        Body::from(playlist),
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
        HeaderValue::from_static("public, max-age=60"),
        etag,
    )
}

fn manifest_response(manifest: Bytes, etag: HeaderValue) -> HttpResult<Response> {
    response(
        Body::from(manifest),
        HeaderValue::from_static("application/dash+xml"),
        HeaderValue::from_static("public, max-age=60"),
        etag,
    )
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
