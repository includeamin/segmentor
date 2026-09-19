//! Middleware: request IDs, metrics, header limits, and load shedding.
use std::time::Instant;

use axum::extract::{MatchedPath, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::error::HttpError;
use super::state::AppState;
use crate::observability::request_id;

pub(crate) const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_REQUEST_ID_BYTES: usize = 128;

/// Accepts a well-formed caller-supplied request ID or generates one, and echoes it back.
pub(crate) async fn request_id(mut request: axum::extract::Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(&X_REQUEST_ID)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_REQUEST_ID_BYTES
                && value
                    .as_bytes()
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
        .cloned();
    let id = supplied.unwrap_or_else(|| {
        HeaderValue::from_str(&request_id::generate())
            .expect("hexadecimal digits and hyphens are valid header characters")
    });
    request.headers_mut().insert(X_REQUEST_ID, id.clone());
    let mut response = next.run(request).await;
    response.headers_mut().insert(X_REQUEST_ID, id);
    response
}

pub(crate) async fn record_metrics(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let route = state.metrics.route_index(
        request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    let started = Instant::now();
    let _in_flight = state.metrics.request_started();
    let response = next.run(request).await;
    state
        .metrics
        .request_finished(route, response.status().as_u16(), started.elapsed());
    response
}

pub(crate) async fn enforce_header_limit(
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

/// Rejects new work with `503` once `max_concurrent_requests` handlers are running.
///
/// Probe and metrics routes are exempt so orchestration still works under overload. The slot is
/// held until response headers are ready; body streaming is bounded separately by the segment
/// job limit and the idle timeout.
pub(crate) async fn shed_load(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/health" | "/ready" | "/metrics") {
        return next.run(request).await;
    }
    let Ok(_slot) = state.request_slots.try_acquire() else {
        state.metrics.request_shed();
        return HttpError::unavailable("too many concurrent requests").into_response();
    };
    next.run(request).await
}
