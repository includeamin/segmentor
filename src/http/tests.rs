//! Router-level tests: real routing, middleware, and streaming through `tower::ServiceExt`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use axum::body::to_bytes;
use axum::http::Request;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ORIGIN, VARY,
};
use tower::ServiceExt;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE, RETRY_AFTER,
};
use axum::response::Response;
use bytes::Bytes;
use tokio::time::{Duration, sleep};

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::middleware::X_REQUEST_ID;
use super::server::{ConnectionLimits, serve_connections};
use super::{AppState, router};
use crate::config::{Config, CorsConfig, LimitsConfig};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4")
}

fn test_config(limits: LimitsConfig, cors: CorsConfig) -> Config {
    let mut assets = BTreeMap::new();
    assets.insert("sample".to_owned(), fixture());
    let mut config = Config::for_catalog(
        fixture().parent().unwrap().to_path_buf(),
        assets,
        1000,
        limits,
    );
    config.cors = cors;
    config
}

fn state() -> AppState {
    AppState::new(&test_config(LimitsConfig::default(), CorsConfig::default()))
        .expect("state should build")
}

fn app() -> Router {
    router(state())
}

/// The asset version, computed on a private runtime so synchronous helpers can call it.
fn version() -> String {
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                crate::asset::PackagedAsset::load_local(fixture(), 1000, &LimitsConfig::default())
                    .await
                    .unwrap()
                    .version()
                    .to_owned()
            })
    })
    .join()
    .unwrap()
}

/// Adds the version query the playlists always emit.
fn versioned(path: &str) -> String {
    format!("{path}?v={}", version())
}

async fn get(app: &Router, uri: &str) -> Response {
    app.clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX).await.unwrap()
}

#[tokio::test]
async fn serves_master_playlist_and_media_objects() {
    let master = get(&app(), "/hls/sample/master.m3u8").await;
    assert_eq!(master.status(), StatusCode::OK);
    assert_eq!(
        master.headers()[CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let master_body = body(master).await;
    assert!(
        master_body
            .windows(16)
            .any(|window| window == b"video/index.m3u8")
    );

    let init = get(&app(), &versioned("/hls/sample/video/init.mp4")).await;
    assert_eq!(init.status(), StatusCode::OK);
    assert_eq!(init.headers()[CONTENT_TYPE], "video/mp4");
    assert_eq!(&body(init).await[4..8], b"ftyp");

    let segment = get(&app(), &versioned("/hls/sample/video/segments/0/media.m4s")).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_eq!(&body(segment).await[4..8], b"moof");
}

#[tokio::test]
async fn serves_audio_media_with_an_audio_content_type() {
    let init = get(&app(), &versioned("/hls/sample/audio-1/init.mp4")).await;
    assert_eq!(init.headers()[CONTENT_TYPE], "audio/mp4");
    let segment = get(
        &app(),
        &versioned("/hls/sample/audio-1/segments/0/media.m4s"),
    )
    .await;
    assert_eq!(segment.headers()[CONTENT_TYPE], "audio/mp4");
}

#[tokio::test]
async fn playlist_urls_carry_the_real_version_that_media_routes_accept() {
    let version = version();
    let playlist = String::from_utf8(
        body(get(&app(), "/hls/sample/video/index.m3u8").await)
            .await
            .to_vec(),
    )
    .unwrap();
    assert!(playlist.contains(&format!("init.mp4?v={version}")));
    assert!(playlist.contains(&format!("segments/0/media.m4s?v={version}")));
    assert!(!playlist.contains('{'));
}

#[tokio::test]
async fn only_the_canonical_track_names_exist() {
    for uri in [
        "/hls/sample/audio/index.m3u8",
        "/hls/sample/audio-0/index.m3u8",
        "/hls/sample/audio-01/index.m3u8",
        "/hls/sample/audio-2/index.m3u8",
        "/hls/sample/Video/index.m3u8",
    ] {
        let response = get(&app(), uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    for uri in [
        "/hls/sample/video/index.m3u8",
        "/hls/sample/audio-1/index.m3u8",
    ] {
        let response = get(&app(), uri).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn rejects_media_urls_with_a_missing_or_stale_version() {
    for uri in [
        "/hls/sample/video/init.mp4",
        "/hls/sample/video/init.mp4?v=0000000000000000",
        "/hls/sample/video/segments/0/media.m4s",
        "/dash/sample/audio-1/segments/0/media.m4s?v=stale",
    ] {
        let response = get(&app(), uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{uri}");
    }
}

#[tokio::test]
async fn serves_dash_manifest() {
    let response = get(&app(), "/dash/sample/manifest.mpd").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/dash+xml");
    let body = body(response).await;
    assert!(body.windows(11).any(|window| window == b"<Adaptation"));
}

#[tokio::test]
async fn returns_not_modified_for_matching_media_etag() {
    let uri = versioned("/hls/sample/video/segments/0/media.m4s");
    let first = get(&app(), &uri).await;
    let etag = first.headers()[ETAG].clone();

    let response = app()
        .oneshot(
            Request::get(&uri)
                .header(IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(body(response).await.is_empty());
}

#[tokio::test]
async fn if_none_match_accepts_lists_weak_tags_and_wildcards() {
    let uri = versioned("/hls/sample/video/init.mp4");
    let etag = get(&app(), &uri).await.headers()[ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    for condition in [
        format!("\"other\", {etag}"),
        format!("W/{etag}"),
        "*".to_owned(),
    ] {
        let response = app()
            .oneshot(
                Request::get(&uri)
                    .header(IF_NONE_MATCH, &condition)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{condition}");
    }
    let response = app()
        .oneshot(
            Request::get(&uri)
                .header(IF_NONE_MATCH, "\"other\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn serves_single_media_byte_ranges() {
    let response = app()
        .oneshot(
            Request::get(versioned("/hls/sample/video/segments/0/media.m4s"))
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
    assert_eq!(body(response).await, b"moof".as_slice());
}

#[tokio::test]
async fn serves_suffix_byte_ranges() {
    let uri = versioned("/hls/sample/video/init.mp4");
    let full = body(get(&app(), &uri).await).await;
    let response = app()
        .oneshot(
            Request::get(&uri)
                .header(RANGE, "bytes=-8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body(response).await, full.slice(full.len() - 8..));
}

#[tokio::test]
async fn ignores_ranges_when_if_range_does_not_match() {
    let uri = versioned("/hls/sample/video/init.mp4");
    let response = app()
        .oneshot(
            Request::get(&uri)
                .header(RANGE, "bytes=0-3")
                .header(IF_RANGE, "\"stale\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await.len() > 4);
}

#[tokio::test]
async fn rejects_unsatisfiable_media_ranges() {
    let response = app()
        .oneshot(
            Request::get(versioned("/hls/sample/video/init.mp4"))
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
async fn head_reports_segment_length_without_reading_the_source() {
    let uri = versioned("/hls/sample/video/segments/0/media.m4s");
    let get_response = get(&app(), &uri).await;
    let length = get_response.headers()[axum::http::header::CONTENT_LENGTH].clone();
    assert_eq!(
        body(get_response).await.len().to_string(),
        length.to_str().unwrap()
    );

    let state = state();
    // Exhausting every job slot proves HEAD never needs one.
    let _held = state
        .segment_jobs
        .clone()
        .acquire_many_owned(u32::try_from(state.segment_jobs.available_permits()).unwrap())
        .await
        .unwrap();
    let response = router(state)
        .oneshot(Request::head(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_LENGTH],
        length
    );
    assert!(body(response).await.is_empty());
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
async fn exposes_request_and_log_metrics() {
    let app = app();
    get(&app, "/hls/sample/master.m3u8").await;
    get(&app, "/nope").await;

    let response = get(&app, "/metrics").await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body(response).await.to_vec()).unwrap();
    assert!(text.contains("vod_log_dropped_lines_total"));
    assert!(text.contains(
        "vod_http_requests_total{route=\"/hls/{asset_id}/master.m3u8\",status=\"200\"} 1"
    ));
    assert!(text.contains("vod_http_requests_total{route=\"unmatched\",status=\"404\"} 1"));
    assert!(text.contains("vod_http_request_duration_seconds_count"));
}

#[tokio::test]
async fn counts_streamed_bytes_in_metrics() {
    let app = app();
    let segment = get(&app, &versioned("/hls/sample/video/segments/0/media.m4s")).await;
    let length = body(segment).await.len();

    let text = String::from_utf8(body(get(&app, "/metrics").await).await.to_vec()).unwrap();

    assert!(text.contains(&format!("vod_http_response_bytes_total {length}")));
    assert!(text.contains("vod_source_read_bytes_total"));
}

#[tokio::test]
async fn readiness_flips_when_shutdown_begins() {
    let state = state();
    let app = router(state.clone());
    assert_eq!(get(&app, "/ready").await.status(), StatusCode::OK);

    state.ready.store(false, Ordering::Relaxed);

    assert_eq!(
        get(&app, "/ready").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(get(&app, "/health").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn generates_and_propagates_request_ids() {
    let generated = get(&app(), "/health").await;
    assert!(!generated.headers()[&X_REQUEST_ID].is_empty());

    let echoed = app()
        .oneshot(
            Request::get("/health")
                .header(&X_REQUEST_ID, "trace-42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(echoed.headers()[&X_REQUEST_ID], "trace-42");

    let replaced = app()
        .oneshot(
            Request::get("/health")
                .header(&X_REQUEST_ID, "bad id with spaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(replaced.headers()[&X_REQUEST_ID], "bad id with spaces");
}

#[tokio::test]
async fn sheds_requests_over_the_concurrency_limit_but_not_probes() {
    let state = state();
    let _held = Arc::clone(&state.request_slots)
        .acquire_many_owned(u32::try_from(state.request_slots.available_permits()).unwrap())
        .await
        .unwrap();
    let app = router(state);

    let shed = get(&app, "/hls/sample/master.m3u8").await;
    assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(shed.headers()[RETRY_AFTER], "1");
    assert_eq!(get(&app, "/health").await.status(), StatusCode::OK);
    assert_eq!(get(&app, "/metrics").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn returns_503_when_no_segment_job_slot_frees_in_time() {
    let limits = LimitsConfig {
        max_segment_jobs: 1,
        segment_queue_timeout_ms: 20,
        ..LimitsConfig::default()
    };
    let state = AppState::new(&test_config(limits, CorsConfig::default())).unwrap();
    let _held = Arc::clone(&state.segment_jobs)
        .acquire_owned()
        .await
        .unwrap();

    let response = get(
        &router(state),
        &versioned("/hls/sample/video/segments/0/media.m4s"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn drops_clients_that_stop_reading_and_frees_the_job_slot() {
    let limits = LimitsConfig {
        stream_chunk_bytes: 64,
        response_idle_timeout_ms: 50,
        max_segment_jobs: 1,
        ..LimitsConfig::default()
    };
    let state = AppState::new(&test_config(limits, CorsConfig::default())).unwrap();
    let app = router(state.clone());
    let uri = versioned("/hls/sample/video/segments/0/media.m4s");

    // Hold the response without reading its body for longer than the idle timeout.
    let stalled = get(&app, &uri).await;
    let declared = stalled.headers()[axum::http::header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    sleep(Duration::from_millis(300)).await;

    // The stalled stream must not be holding the only job slot.
    let second = get(&app, &uri).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(body(second).await.len(), declared);
    let delivered = body(stalled).await.len();
    assert!(delivered < declared, "stalled stream should be cut short");
    let metrics = state.metrics.render(0);
    assert!(
        metrics.contains("vod_segment_stream_aborts_idle_total 1"),
        "{metrics}"
    );
}

#[tokio::test]
async fn cors_defaults_expose_the_headers_players_need() {
    let response = app()
        .oneshot(
            Request::get(versioned("/hls/sample/video/init.mp4"))
                .header(ORIGIN, "https://player.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let exposed = response.headers()[ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()
        .unwrap()
        .to_owned();
    for name in ["content-range", "etag", "accept-ranges", "content-length"] {
        assert!(exposed.contains(name), "{name} missing from {exposed}");
    }
}

#[tokio::test]
async fn answers_preflight_for_range_requests() {
    let response = app()
        .oneshot(
            Request::options(versioned("/hls/sample/video/segments/0/media.m4s"))
                .header(ORIGIN, "https://player.example.com")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "range,if-none-match")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(response.status().is_success());
    let allowed = response.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap();
    assert!(allowed.contains("range") && allowed.contains("if-none-match"));
    assert_eq!(response.headers()["access-control-max-age"], "86400");
}

#[tokio::test]
async fn cors_error_responses_stay_readable_by_browsers() {
    let response = app()
        .oneshot(
            Request::get("/health")
                .header(ORIGIN, "https://player.example.com")
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
    assert_eq!(response.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], "*");
}

#[tokio::test]
async fn restricts_cors_to_configured_origins() {
    let cors = CorsConfig {
        allowed_origins: vec!["https://player.example.com".to_owned()],
        ..CorsConfig::default()
    };
    let app = router(AppState::new(&test_config(LimitsConfig::default(), cors)).unwrap());
    let request = |origin: &'static str| {
        Request::get("/health")
            .header(ORIGIN, origin)
            .body(Body::empty())
            .unwrap()
    };

    let allowed = app
        .clone()
        .oneshot(request("https://player.example.com"))
        .await
        .unwrap();
    let denied = app
        .clone()
        .oneshot(request("https://evil.example.com"))
        .await
        .unwrap();

    assert_eq!(
        allowed.headers()[ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://player.example.com"
    );
    assert!(allowed.headers().get(VARY).is_some());
    assert!(denied.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn disabled_cors_adds_no_headers() {
    let cors = CorsConfig {
        enabled: false,
        ..CorsConfig::default()
    };
    let app = router(AppState::new(&test_config(LimitsConfig::default(), cors)).unwrap());

    let response = app
        .oneshot(
            Request::get("/health")
                .header(ORIGIN, "https://player.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response
            .headers()
            .get(ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}

#[test]
fn rejects_unparseable_cors_header_names_at_startup() {
    let cors = CorsConfig {
        allowed_headers: vec!["not a header".to_owned()],
        ..CorsConfig::default()
    };

    let error = AppState::new(&test_config(LimitsConfig::default(), cors))
        .expect_err("invalid header name should fail startup");

    assert!(error.to_string().contains("cors.allowed_headers"));
}

#[tokio::test]
async fn rejects_asset_indexes_over_the_memory_budget() {
    let limits = LimitsConfig {
        max_index_bytes: 1024,
        ..LimitsConfig::default()
    };
    let state = AppState::new(&test_config(limits, CorsConfig::default())).unwrap();

    let error = state
        .preload()
        .await
        .expect_err("tiny budget should fail startup");

    assert!(error.to_string().contains("max_index_bytes"));
}

#[tokio::test]
async fn ffmpeg_decodes_hls_and_dash_presentations() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping media validation because ffmpeg is unavailable");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        "/hls/unknown/master.m3u8".to_owned(),
        versioned("/hls/sample/video/segments/99/media.m4s"),
        "/hls/sample/subtitle/index.m3u8".to_owned(),
    ] {
        let response = get(&app(), &uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

/// A running server on an ephemeral port, driven through real sockets.
struct TestServer {
    address: SocketAddr,
    metrics: Arc<crate::observability::metrics::Metrics>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(limits: ConnectionLimits) -> Self {
        Self::start_with(limits, None).await
    }

    async fn start_tls(limits: ConnectionLimits, tls: crate::config::TlsConfig) -> Self {
        Self::start_with(limits, Some(tls)).await
    }

    async fn start_with(limits: ConnectionLimits, tls: Option<crate::config::TlsConfig>) -> Self {
        let state = state();
        let metrics = Arc::clone(&state.metrics);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_connections(
            listener,
            router(state),
            limits,
            tls,
            Arc::clone(&metrics),
            async move {
                let _ = signal.await;
            },
        ));
        Self {
            address,
            metrics,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.address).await.unwrap()
    }

    /// One complete `Connection: close` request; returns the raw response.
    async fn get(&self, path: &str) -> String {
        let mut client = self.connect().await;
        client
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(3), client.read_to_end(&mut response))
            .await
            .expect("server should answer")
            .unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }
}

fn connection_limits(max_connections: usize, header_read_timeout_ms: u64) -> ConnectionLimits {
    ConnectionLimits {
        max_connections,
        header_read_timeout: Duration::from_millis(header_read_timeout_ms),
    }
}

#[tokio::test]
async fn closes_connections_that_stall_while_sending_headers() {
    let server = TestServer::start(connection_limits(100, 150)).await;
    let mut slow = server.connect().await;
    // A request whose header block never ends.
    slow.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();

    let mut received = Vec::new();
    let closed = timeout(Duration::from_secs(3), slow.read_to_end(&mut received)).await;

    assert!(
        closed.is_ok(),
        "the server should give up on a stalled header read"
    );
    assert!(server.get("/health").await.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn closes_idle_keep_alive_connections_after_the_header_timeout() {
    let server = TestServer::start(connection_limits(100, 150)).await;
    let mut client = server.connect().await;
    client
        .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0u8; 512];
    let read = client.read(&mut first).await.unwrap();
    assert!(String::from_utf8_lossy(&first[..read]).starts_with("HTTP/1.1 200"));

    // The connection is now idle, waiting for a second request that never comes.
    let mut rest = Vec::new();
    let closed = timeout(Duration::from_secs(3), client.read_to_end(&mut rest)).await;

    assert!(
        closed.is_ok(),
        "an idle connection should not be held forever"
    );
}

#[tokio::test]
async fn rejects_connections_beyond_the_cap_and_recovers() {
    let server = TestServer::start(connection_limits(2, 5_000)).await;
    let first = server.connect().await;
    let _second = server.connect().await;
    sleep(Duration::from_millis(100)).await;

    let mut third = server.connect().await;
    let mut received = Vec::new();
    let closed = timeout(Duration::from_secs(2), third.read_to_end(&mut received)).await;
    assert!(
        closed.is_ok(),
        "the over-limit connection should be closed at accept"
    );
    assert!(received.is_empty());
    assert_eq!(
        server
            .metrics
            .render(0)
            .matches("vod_http_connections_rejected_total 1")
            .count(),
        1
    );

    drop(first);
    sleep(Duration::from_millis(100)).await;
    assert!(server.get("/health").await.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn shutdown_drains_and_closes_idle_connections() {
    let mut server = TestServer::start(connection_limits(100, 5_000)).await;
    let mut client = server.connect().await;
    client
        .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buffer = [0u8; 512];
    assert!(client.read(&mut buffer).await.unwrap() > 0);

    server.shutdown.take().unwrap().send(()).unwrap();

    let finished = timeout(Duration::from_secs(3), &mut server.task).await;
    assert!(
        finished.is_ok(),
        "shutdown should not wait on an idle connection"
    );
}

// ---------------------------------------------------------------------------------------------
// Admin status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn admin_status_lists_the_static_catalog_and_what_is_cached() {
    let app = app();
    let empty: serde_json::Value =
        serde_json::from_slice(&body(get(&app, "/admin/status").await).await).unwrap();

    assert_eq!(empty["resolver"]["kind"], "static");
    assert_eq!(empty["resolver"]["healthy"], true);
    assert_eq!(empty["resolver"]["base_url"], serde_json::Value::Null);
    assert_eq!(
        empty["resolver"]["known_assets"],
        serde_json::json!(["sample"])
    );
    assert_eq!(empty["cache"]["count"], 0, "nothing has been requested yet");

    get(&app, "/hls/sample/master.m3u8").await;
    let after: serde_json::Value =
        serde_json::from_slice(&body(get(&app, "/admin/status").await).await).unwrap();

    assert_eq!(after["cache"]["count"], 1);
    let cached = &after["cache"]["assets"][0];
    assert_eq!(cached["asset_id"], "sample");
    // The static resolver's cache key is a constant, not the content hash `version()` computes
    // for URLs; the two are different things and only the latter appears in playlist URLs.
    assert_eq!(cached["version"], "static");
    assert!(cached["bytes"].as_u64().unwrap() > 0);
    assert!(cached["tracks"].as_u64().unwrap() >= 1);
    assert!(cached["duration_seconds"].as_f64().unwrap() > 0.0);
    assert!(
        after["cache"]["bytes"].as_u64().unwrap() >= cached["bytes"].as_u64().unwrap(),
        "the total covers every cached asset"
    );

    let response = get(&app, "/admin/status").await;
    assert_eq!(
        response.headers().get(CACHE_CONTROL).unwrap(),
        "no-store",
        "a live status must never be cached"
    );
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

fn tls_fixture(name: &str) -> PathBuf {
    fixture().parent().unwrap().join("tls").join(name)
}

fn test_tls_config() -> crate::config::TlsConfig {
    crate::config::tls_for_test(tls_fixture("cert.pem"), tls_fixture("key.pem"))
        .expect("the committed test certificate and key should load")
}

/// A client trusting only the fixture certificate — a stand-in for a real client trusting a real
/// CA, without needing one for a self-signed test certificate.
fn tls_connector() -> tokio_rustls::TlsConnector {
    use rustls_pki_types::pem::PemObject;
    let der = rustls_pki_types::CertificateDer::from_pem_file(tls_fixture("cert.pem"))
        .expect("the fixture certificate should parse");
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots
        .add(der)
        .expect("the fixture certificate should be a valid trust root");
    let mut config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn tls_terminates_the_connection_and_serves_a_real_request() {
    let server = TestServer::start_tls(connection_limits(10, 5_000), test_tls_config()).await;
    let connector = tls_connector();
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();

    let tcp = server.connect().await;
    let mut tls = connector
        .connect(name, tcp)
        .await
        .expect("the handshake should succeed");
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"http/1.1".as_slice()),
        "the server only ever offers http/1.1"
    );

    tls.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tls.read_to_end(&mut response).await.unwrap();

    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok\n"), "{response}");
}

#[tokio::test]
async fn a_plain_tcp_client_cannot_talk_to_a_tls_listener() {
    let server = TestServer::start_tls(connection_limits(10, 5_000), test_tls_config()).await;

    let mut client = server.connect().await;
    client
        .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    let read = timeout(Duration::from_secs(2), client.read_to_end(&mut response)).await;

    // Either the handshake attempt fails outright, or the server closes without ever answering
    // in plaintext; either way, no HTTP response comes back.
    assert!(
        read.is_err() || !String::from_utf8_lossy(&response).starts_with("HTTP/"),
        "{:?}",
        String::from_utf8_lossy(&response)
    );
}
