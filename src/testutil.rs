//! In-process stand-ins for the services the registry talks to: a mapper and a media origin.
//!
//! Both are real HTTP servers on ephemeral loopback ports built with Axum, so tests exercise the
//! actual request and response handling rather than a fake trait.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::net::TcpListener;

pub(crate) fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub(crate) fn fixtures_dir() -> PathBuf {
    fixture("")
        .parent()
        .expect("fixtures directory has a parent")
        .join("fixtures")
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    address
}

// ---------------------------------------------------------------------------------------------
// Mapper
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub(crate) version: String,
    pub(crate) location: Value,
    pub(crate) ttl_seconds: Option<u64>,
    pub(crate) expires_at: Option<String>,
}

impl Answer {
    pub(crate) fn file(version: &str, path: &str) -> Self {
        Self {
            version: version.to_owned(),
            location: json!({ "type": "file", "path": path }),
            ttl_seconds: Some(300),
            expires_at: None,
        }
    }

    pub(crate) fn http(version: &str, url: &str) -> Self {
        Self {
            version: version.to_owned(),
            location: json!({ "type": "http", "url": url }),
            ttl_seconds: Some(300),
            expires_at: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct MapperState {
    answers: Mutex<HashMap<String, Answer>>,
    /// Every `If-None-Match` value seen, in order.
    pub(crate) conditions: Mutex<Vec<Option<String>>>,
    pub(crate) calls: AtomicUsize,
    /// When non-zero, every asset request answers with this status.
    pub(crate) forced_status: AtomicU16,
    /// When set, the asset request answers `200` with exactly this body.
    pub(crate) raw_body: Mutex<Option<String>>,
    pub(crate) required_token: Mutex<Option<String>>,
    pub(crate) seen_tokens: Mutex<Vec<Option<String>>>,
    pub(crate) unhealthy: AtomicBool,
    pub(crate) delay_ms: AtomicU64,
    /// When set, a `304` is never sent, so every lookup gets a full answer (a fresh signature).
    pub(crate) always_full: AtomicBool,
}

impl MapperState {
    pub(crate) fn set(&self, asset_id: &str, answer: Answer) {
        self.answers
            .lock()
            .unwrap()
            .insert(asset_id.to_owned(), answer);
    }

    pub(crate) fn remove(&self, asset_id: &str) {
        self.answers.lock().unwrap().remove(asset_id);
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

pub(crate) struct MockMapper {
    pub(crate) address: SocketAddr,
    pub(crate) state: Arc<MapperState>,
}

impl MockMapper {
    pub(crate) async fn start() -> Self {
        let state = Arc::new(MapperState::default());
        let app = Router::new()
            .route("/v1/assets/{id}", get(mapper_asset))
            .route("/v1/health", get(mapper_health))
            .with_state(Arc::clone(&state));
        Self {
            address: serve(app).await,
            state,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

async fn mapper_health(State(state): State<Arc<MapperState>>) -> StatusCode {
    if state.unhealthy.load(Ordering::SeqCst) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

async fn mapper_asset(
    State(state): State<Arc<MapperState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let delay = state.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.seen_tokens.lock().unwrap().push(token.clone());
    let condition = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.conditions.lock().unwrap().push(condition.clone());

    if let Some(required) = state.required_token.lock().unwrap().clone()
        && token.as_deref() != Some(&format!("Bearer {required}"))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let forced = state.forced_status.load(Ordering::SeqCst);
    if forced != 0 {
        return StatusCode::from_u16(forced).unwrap().into_response();
    }
    if let Some(body) = state.raw_body.lock().unwrap().clone() {
        return ([(CONTENT_TYPE, "application/json")], body).into_response();
    }
    let Some(answer) = state.answers.lock().unwrap().get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.always_full.load(Ordering::SeqCst)
        && condition.as_deref() == Some(&format!("\"{}\"", answer.version))
    {
        return (StatusCode::NOT_MODIFIED, [(CACHE_CONTROL, "max-age=300")]).into_response();
    }
    let mut body = json!({
        "asset_id": id,
        "version": answer.version,
        "location": answer.location,
    });
    if let Some(ttl) = answer.ttl_seconds {
        body["ttl_seconds"] = json!(ttl);
    }
    if let Some(expires_at) = answer.expires_at {
        body["expires_at"] = json!(expires_at);
    }
    ([(CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

// ---------------------------------------------------------------------------------------------
// Media origin
// ---------------------------------------------------------------------------------------------

type OriginFile = (Arc<Vec<u8>>, OriginValidator);

#[derive(Default)]
pub(crate) struct OriginState {
    /// Served files by name: bytes and the validator to advertise.
    files: Mutex<HashMap<String, OriginFile>>,
    pub(crate) requests: AtomicUsize,
    pub(crate) bytes_served: AtomicU64,
    pub(crate) ranges: Mutex<Vec<String>>,
    pub(crate) ignore_ranges: AtomicBool,
    /// When non-zero, every request answers with this status.
    pub(crate) forced_status: AtomicU16,
    /// Every query string seen, in order.
    pub(crate) queries: Mutex<Vec<String>>,
    /// When set, requests whose query differs get `403`, as an expired signature would.
    pub(crate) required_query: Mutex<Option<String>>,
}

#[derive(Clone)]
pub(crate) enum OriginValidator {
    ETag(String),
    LastModified(String),
    WeakETag(String),
    None,
}

impl OriginState {
    pub(crate) fn add(&self, name: &str, bytes: Vec<u8>, validator: OriginValidator) {
        self.files
            .lock()
            .unwrap()
            .insert(name.to_owned(), (Arc::new(bytes), validator));
    }

    pub(crate) fn set_validator(&self, name: &str, validator: OriginValidator) {
        if let Some(entry) = self.files.lock().unwrap().get_mut(name) {
            entry.1 = validator;
        }
    }
}

pub(crate) struct MockOrigin {
    pub(crate) address: SocketAddr,
    pub(crate) state: Arc<OriginState>,
}

impl MockOrigin {
    pub(crate) async fn start() -> Self {
        let state = Arc::new(OriginState::default());
        let app = Router::new()
            .route("/media/{name}", get(origin_media))
            .with_state(Arc::clone(&state));
        Self {
            address: serve(app).await,
            state,
        }
    }

    pub(crate) fn url(&self, name: &str) -> String {
        format!("http://{}/media/{name}", self.address)
    }

    /// Serves a fixture file under its own name with a strong `ETag`.
    pub(crate) fn add_fixture(&self, name: &str) {
        let bytes = std::fs::read(fixture(name)).unwrap();
        self.state
            .add(name, bytes, OriginValidator::ETag(format!("\"{name}-v1\"")));
    }
}

async fn origin_media(
    State(state): State<Arc<OriginState>>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let query = query.unwrap_or_default();
    state.queries.lock().unwrap().push(query.clone());
    if let Some(required) = state.required_query.lock().unwrap().clone()
        && query != required
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let forced = state.forced_status.load(Ordering::SeqCst);
    if forced != 0 {
        return StatusCode::from_u16(forced).unwrap().into_response();
    }
    let Some((bytes, validator)) = state.files.lock().unwrap().get(&name).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response_headers = HeaderMap::new();
    let current = match &validator {
        OriginValidator::ETag(value) => {
            response_headers.insert(ETAG, HeaderValue::from_str(value).unwrap());
            Some(value.clone())
        }
        OriginValidator::WeakETag(value) => {
            response_headers.insert(ETAG, HeaderValue::from_str(&format!("W/{value}")).unwrap());
            Some(format!("W/{value}"))
        }
        OriginValidator::LastModified(value) => {
            response_headers.insert(
                axum::http::header::LAST_MODIFIED,
                HeaderValue::from_str(value).unwrap(),
            );
            Some(value.clone())
        }
        OriginValidator::None => None,
    };

    let range = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(range) = &range {
        state.ranges.lock().unwrap().push(range.clone());
    }
    let condition_holds = headers
        .get(IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|condition| Some(condition) == current.as_deref());
    let parsed = range
        .as_deref()
        .and_then(|range| range.strip_prefix("bytes="))
        .and_then(|span| span.split_once('-'))
        .and_then(|(first, last)| {
            Some((first.parse::<usize>().ok()?, last.parse::<usize>().ok()?))
        });
    match parsed {
        Some((first, last))
            if condition_holds
                && !state.ignore_ranges.load(Ordering::SeqCst)
                && first < bytes.len() =>
        {
            let last = last.min(bytes.len() - 1);
            let body = bytes[first..=last].to_vec();
            state
                .bytes_served
                .fetch_add(body.len() as u64, Ordering::SeqCst);
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            *response.headers_mut() = response_headers;
            response.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {first}-{last}/{}", bytes.len())).unwrap(),
            );
            response
        }
        _ => {
            state
                .bytes_served
                .fetch_add(bytes.len() as u64, Ordering::SeqCst);
            let mut response = Response::new(Body::from(bytes.as_ref().clone()));
            *response.headers_mut() = response_headers;
            response
        }
    }
}
