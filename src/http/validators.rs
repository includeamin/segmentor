//! Entity tags and conditional-request evaluation.
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

use super::error::{HttpError, HttpResult};

pub(crate) fn entity_tag(version: &str, resource: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}-{resource}\""))
        .expect("asset versions and internal resource names are valid header values")
}

/// `If-None-Match` per RFC 9110: any listed tag matches by weak comparison, and `*` matches all.
pub(crate) fn not_modified(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(current) = etag.to_str() else {
        return false;
    };
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == current
        })
}

pub(crate) fn not_modified_response(
    etag: HeaderValue,
    cache_control: &'static str,
) -> HttpResult<Response> {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(ETAG, etag)
        .header(CACHE_CONTROL, HeaderValue::from_static(cache_control))
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()))
}
