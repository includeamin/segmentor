//! Liveness, readiness, and metrics endpoints.
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};

use crate::http::state::AppState;
use crate::observability::logging;

pub(crate) async fn health() -> &'static str {
    "ok\n"
}

/// Readiness differs from liveness: it flips to `503` when shutdown begins.
pub(crate) async fn ready(State(state): State<AppState>) -> Response {
    if state.ready.load(Ordering::Relaxed) && state.resolver_healthy.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
    }
}

pub(crate) async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render(logging::dropped_lines()),
    )
        .into_response()
}
