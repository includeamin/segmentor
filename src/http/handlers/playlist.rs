//! HLS playlists and the DASH manifest.
//!
//! Each is served compressed when the client accepts it: brotli, then gzip, then identity (see
//! [`Encoding::negotiate`]). Every encoding is its own representation with its own strong
//! entity tag, and responses carry `Vary: Accept-Encoding` so caches keep them apart.
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, VARY,
};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;

use super::parse_track;
use crate::error::Result;
use crate::http::authorize::QueryToken;
use crate::http::error::{HttpError, HttpResult};
use crate::http::state::AppState;
use crate::http::validators::{entity_tag, not_modified, not_modified_response};
use crate::protocol::hls::MUXED;
use crate::protocol::{Encoding, Manifest};

const HLS: &str = "application/vnd.apple.mpegurl";
const DASH: &str = "application/dash+xml";
const CACHE: &str = "public, max-age=60";

pub(crate) async fn master_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
    token: Option<Extension<QueryToken>>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    serve(
        &headers,
        token.as_ref().map(|Extension(token)| token),
        asset.version(),
        "hls-master",
        HLS,
        || Ok(asset.hls_master_playlist()),
    )
}

pub(crate) async fn media_playlist(
    State(state): State<AppState>,
    Path((asset_id, track)): Path<(String, String)>,
    headers: HeaderMap,
    token: Option<Extension<QueryToken>>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    if track == MUXED {
        return serve(
            &headers,
            token.as_ref().map(|Extension(token)| token),
            asset.version(),
            "hls-muxed-playlist",
            HLS,
            || asset.hls_muxed_playlist(),
        );
    }
    let requested = parse_track(&track)?;
    serve(
        &headers,
        token.as_ref().map(|Extension(token)| token),
        asset.version(),
        &format!("hls-{track}-playlist"),
        HLS,
        || asset.hls_media_playlist(requested.rendition.as_deref(), requested.key),
    )
}

pub(crate) async fn iframe_playlist(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
    token: Option<Extension<QueryToken>>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    serve(
        &headers,
        token.as_ref().map(|Extension(token)| token),
        asset.version(),
        "hls-iframe-playlist",
        HLS,
        || asset.hls_iframe_playlist(),
    )
}

pub(crate) async fn subtitle_playlist(
    State(state): State<AppState>,
    Path((asset_id, language)): Path<(String, String)>,
    headers: HeaderMap,
    token: Option<Extension<QueryToken>>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    serve(
        &headers,
        token.as_ref().map(|Extension(token)| token),
        asset.version(),
        &format!("hls-subtitle-{}-playlist", language.to_ascii_lowercase()),
        HLS,
        || asset.hls_subtitle_playlist(&language),
    )
}

pub(crate) async fn dash_manifest(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
    token: Option<Extension<QueryToken>>,
) -> HttpResult<Response> {
    let asset = state.asset(&asset_id).await?;
    serve(
        &headers,
        token.as_ref().map(|Extension(token)| token),
        asset.version(),
        "dash-manifest",
        DASH,
        || asset.dash_manifest(),
    )
}

/// Answers with `manifest` in the best encoding the client accepts, or with `304 Not Modified`
/// before rendering anything when the client already holds that encoding.
/// `token` is present when the viewer's token arrived in the query string (TDD 0007): the
/// playlist then carries it into every URL it lists, which makes the response the viewer's own, so
/// it is built per request, uncompressed, with no entity tag, and never cached.
fn serve(
    headers: &HeaderMap,
    token: Option<&QueryToken>,
    version: &str,
    resource: &str,
    content_type: &'static str,
    manifest: impl FnOnce() -> Result<Manifest>,
) -> HttpResult<Response> {
    if let Some(token) = token {
        let manifest = manifest()?;
        let body = carry_token(&manifest, version, token, content_type == DASH);
        return Response::builder()
            .header(CONTENT_TYPE, HeaderValue::from_static(content_type))
            .header(CACHE_CONTROL, HeaderValue::from_static("private, no-store"))
            .body(Body::from(body))
            .map_err(|error| HttpError::internal(error.to_string()));
    }
    let encoding = Encoding::negotiate(
        headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    let etag = match encoding.header() {
        Some(coding) => entity_tag(version, &format!("{resource}-{coding}")),
        None => entity_tag(version, resource),
    };
    let vary = HeaderValue::from_static("Accept-Encoding");
    if not_modified(headers, &etag) {
        let mut response = not_modified_response(etag, CACHE)?;
        response.headers_mut().insert(VARY, vary);
        return Ok(response);
    }
    let body = manifest()?.encoded(encoding);
    let mut response = Response::builder()
        .header(CONTENT_TYPE, HeaderValue::from_static(content_type))
        .header(CACHE_CONTROL, HeaderValue::from_static(CACHE))
        .header(ETAG, etag)
        .header(VARY, vary);
    if let Some(coding) = encoding.header() {
        response = response.header(CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    response
        .body(Body::from(body))
        .map_err(|error| HttpError::internal(error.to_string()))
}

/// `playlist` with the viewer's token added to every URL that carries the asset's version, which
/// is every URL a playlist or manifest lists. `xml` writes the `&` that joins the parameters as
/// `&amp;`, as an MPD attribute requires. A token is base64url and dots, so it needs no escaping.
fn carry_token(playlist: &[u8], version: &str, token: &QueryToken, xml: bool) -> String {
    let joiner = if xml { "&amp;" } else { "&" };
    String::from_utf8_lossy(playlist).replace(
        &format!("?v={version}"),
        &format!("?v={version}{joiner}{}={}", token.parameter, token.token),
    )
}
