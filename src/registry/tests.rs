//! Registry, mapper, and remote-media tests against in-process mapper and origin servers.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

use crate::config::{
    Config, LimitsConfig, MapperConfig, RemoteMediaConfig, ResolverSettings, Secret,
};
use crate::http::{AppState, router, spawn_resolver_probe};
use crate::testutil::{Answer, MockMapper, MockOrigin, OriginValidator, fixture, fixtures_dir};

fn mapper_settings(url: &str) -> MapperConfig {
    MapperConfig {
        base_url: url.to_owned(),
        bearer_token: None,
        connect_timeout_ms: 500,
        request_timeout_ms: 2000,
        max_retries: 2,
        max_response_bytes: 16 * 1024,
        default_ttl_ms: 60_000,
        min_ttl_ms: 10,
        max_ttl_ms: 60_000,
        negative_ttl_ms: 60_000,
        error_ttl_ms: 100,
        stale_if_error_ms: 60_000,
        refresh_margin_ms: 100,
        readiness_probe_interval_ms: 0,
    }
}

fn config_for(mapper: &MockMapper, tune: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::for_catalog(
        fixtures_dir(),
        BTreeMap::new(),
        1000,
        LimitsConfig::default(),
    );
    config.resolver = ResolverSettings::Http(mapper_settings(&mapper.url()));
    config.remote_media = RemoteMediaConfig {
        allowed_hosts: vec!["127.0.0.1".to_owned()],
        allow_insecure_http: true,
        allow_private_addresses: true,
        max_retries: 1,
        ..RemoteMediaConfig::default()
    };
    tune(&mut config);
    config
}

struct Harness {
    mapper: MockMapper,
    state: AppState,
    app: Router,
}

async fn harness() -> Harness {
    harness_with(|_| {}).await
}

async fn harness_with(tune: impl FnOnce(&mut Config)) -> Harness {
    let mapper = MockMapper::start().await;
    let config = config_for(&mapper, tune);
    let state = AppState::new(&config).unwrap();
    let app = router(state.clone());
    Harness { mapper, state, app }
}

async fn fetch(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
    (parts.status, parts.headers, bytes)
}

async fn status(app: &Router, uri: &str) -> StatusCode {
    fetch(app, uri).await.0
}

fn version_in(master: &Bytes) -> String {
    String::from_utf8_lossy(master)
        .split("?v=")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect()
}

fn metric(state: &AppState, line: &str) -> bool {
    state
        .metrics
        .render(0)
        .lines()
        .any(|candidate| candidate == line)
}

#[tokio::test]
async fn serves_a_file_location_resolved_by_the_mapper() {
    let h = harness().await;
    h.mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));

    for _ in 0..6 {
        assert_eq!(
            status(&h.app, "/hls/movie/master.m3u8").await,
            StatusCode::OK
        );
    }

    assert_eq!(
        h.mapper.state.calls(),
        1,
        "later requests must hit the resolution cache"
    );
    assert!(metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"));
    assert!(metric(
        &h.state,
        "vod_resolver_requests_total{outcome=\"ok\"} 1"
    ));
}

#[tokio::test]
async fn concurrent_first_requests_share_one_resolve_and_load() {
    let h = harness().await;
    h.mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));
    h.mapper.state.delay_ms.store(100, Ordering::SeqCst);

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let app = h.app.clone();
        tasks.spawn(async move { status(&app, "/hls/movie/master.m3u8").await });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap(), StatusCode::OK);
    }

    assert_eq!(h.mapper.state.calls(), 1);
    assert!(metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"));
    assert!(
        !metric(&h.state, "vod_registry_coalesced_waiters_total 0"),
        "waiters should have been coalesced"
    );
}

#[tokio::test]
async fn unknown_assets_are_not_found_and_negatively_cached() {
    let h = harness().await;

    assert_eq!(
        status(&h.app, "/hls/nothing/master.m3u8").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&h.app, "/hls/nothing/master.m3u8").await,
        StatusCode::NOT_FOUND
    );

    assert_eq!(h.mapper.state.calls(), 1);
    assert!(metric(
        &h.state,
        "vod_resolution_cache_events_total{event=\"negative_hit\"} 1"
    ));
}

#[tokio::test]
async fn malformed_asset_ids_never_reach_the_mapper() {
    let h = harness().await;
    for uri in ["/hls/a.b/master.m3u8", "/hls/a%20b/master.m3u8"] {
        assert_eq!(status(&h.app, uri).await, StatusCode::NOT_FOUND);
    }
    let long = format!("/hls/{}/master.m3u8", "a".repeat(200));
    assert_eq!(status(&h.app, &long).await, StatusCode::NOT_FOUND);
    assert_eq!(h.mapper.state.calls(), 0);
}

#[tokio::test]
async fn a_mapper_outage_is_503_after_retrying() {
    let h = harness().await;
    h.mapper.state.forced_status.store(500, Ordering::SeqCst);

    let (status, headers, _) = fetch(&h.app, "/hls/movie/master.m3u8").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "1");
    assert_eq!(h.mapper.state.calls(), 3, "one attempt plus max_retries");
    // The failure is cached briefly so a struggling mapper is not hammered.
    assert_eq!(
        self::status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(h.mapper.state.calls(), 3);
}

#[tokio::test]
async fn rate_limiting_and_credential_failures_map_correctly() {
    let h = harness().await;
    h.mapper.state.forced_status.store(429, Ordering::SeqCst);
    assert_eq!(
        status(&h.app, "/hls/a/master.m3u8").await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    h.mapper.state.forced_status.store(401, Ordering::SeqCst);
    assert_eq!(
        status(&h.app, "/hls/b/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    h.mapper.state.forced_status.store(403, Ordering::SeqCst);
    assert_eq!(
        status(&h.app, "/hls/c/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    h.mapper.state.forced_status.store(418, Ordering::SeqCst);
    assert_eq!(
        status(&h.app, "/hls/d/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn sends_the_bearer_token_and_fails_without_it() {
    let mapper = MockMapper::start().await;
    *mapper.state.required_token.lock().unwrap() = Some("s3cret".to_owned());
    mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));

    let with_token = config_for(&mapper, |config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.bearer_token = Some(Secret::new("s3cret".to_owned()));
    });
    let app = router(AppState::new(&with_token).unwrap());
    assert_eq!(status(&app, "/hls/movie/master.m3u8").await, StatusCode::OK);
    assert_eq!(
        mapper
            .state
            .seen_tokens
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .as_deref(),
        Some("Bearer s3cret")
    );

    let without = router(AppState::new(&config_for(&mapper, |_| {})).unwrap());
    assert_eq!(
        status(&without, "/hls/movie/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn invalid_mapper_answers_are_rejected_with_502() {
    let past = "2000-01-01T00:00:00Z";
    let bodies = [
        r#"{"asset_id":"movie","version":"v1","location":{"type":"file","path":"../secret.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"v1","location":{"type":"file","path":"/etc/passwd"}}"#.to_owned(),
        r#"{"asset_id":"other","version":"v1","location":{"type":"file","path":"h264-aac.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"v1","location":{"type":"ftp","url":"ftp://x/y"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"","location":{"type":"file","path":"h264-aac.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"has space","location":{"type":"file","path":"h264-aac.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","location":{"type":"file","path":"h264-aac.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"v1","location":{"type":"http","url":"http://evil.example/x.mp4"}}"#.to_owned(),
        r#"{"asset_id":"movie","version":"v1","location":{"type":"http","url":"http://user:pw@127.0.0.1/x.mp4"}}"#.to_owned(),
        format!(r#"{{"asset_id":"movie","version":"v1","expires_at":"{past}","location":{{"type":"file","path":"h264-aac.mp4"}}}}"#),
        r#"{"asset_id":"movie","version":"v1","expires_at":"soon","location":{"type":"file","path":"h264-aac.mp4"}}"#.to_owned(),
        "this is not json".to_owned(),
        String::new(),
    ];
    for body in bodies {
        let h = harness().await;
        *h.mapper.state.raw_body.lock().unwrap() = Some(body.clone());
        assert_eq!(
            status(&h.app, "/hls/movie/master.m3u8").await,
            StatusCode::BAD_GATEWAY,
            "{body}"
        );
    }
}

#[tokio::test]
async fn oversized_mapper_answers_are_rejected() {
    let h = harness_with(|config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.max_response_bytes = 64;
    })
    .await;
    h.mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));
    *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
        r#"{{"asset_id":"movie","version":"v1","padding":"{}","location":{{"type":"file","path":"h264-aac.mp4"}}}}"#,
        "x".repeat(500)
    ));

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn unknown_response_fields_are_ignored() {
    let h = harness().await;
    *h.mapper.state.raw_body.lock().unwrap() = Some(
        r#"{"asset_id":"movie","version":"v1","future_field":{"a":1},"location":{"type":"file","path":"h264-aac.mp4","extra":true}}"#
            .to_owned(),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn revalidation_uses_if_none_match_and_keeps_the_loaded_asset() {
    let h = harness().await;
    let mut answer = Answer::file("v1", "h264-aac.mp4");
    answer.ttl_seconds = Some(0);
    h.mapper.state.set("movie", answer);
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );

    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );

    assert_eq!(h.mapper.state.calls(), 2);
    assert_eq!(
        h.mapper
            .state
            .conditions
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .as_deref(),
        Some("\"v1\"")
    );
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"),
        "the asset must not reload"
    );
    assert!(metric(
        &h.state,
        "vod_resolver_requests_total{outcome=\"unchanged\"} 1"
    ));
}

#[tokio::test]
async fn a_new_version_replaces_the_old_and_stale_urls_stop_resolving() {
    let h = harness().await;
    let mut first = Answer::file("v1", "h264-aac.mp4");
    first.ttl_seconds = Some(0);
    h.mapper.state.set("movie", first);
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let old = version_in(&master);
    assert_eq!(
        status(&h.app, &format!("/hls/movie/video/init.mp4?v={old}")).await,
        StatusCode::OK
    );

    let mut second = Answer::file("v2", "h264-video-only.mp4");
    second.ttl_seconds = Some(0);
    h.mapper.state.set("movie", second);
    tokio::time::sleep(Duration::from_millis(40)).await;
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let new = version_in(&master);

    assert_ne!(old, new);
    assert!(
        !String::from_utf8_lossy(&master).contains("audio"),
        "the new file has no audio"
    );
    assert_eq!(
        status(&h.app, &format!("/hls/movie/video/init.mp4?v={old}")).await,
        StatusCode::NOT_FOUND,
        "the previous version gets no grace period"
    );
    assert_eq!(
        status(&h.app, &format!("/hls/movie/video/init.mp4?v={new}")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn removed_assets_stop_being_served() {
    let h = harness().await;
    let mut answer = Answer::file("v1", "h264-aac.mp4");
    answer.ttl_seconds = Some(0);
    h.mapper.state.set("movie", answer);
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );

    h.mapper.state.remove("movie");
    tokio::time::sleep(Duration::from_millis(40)).await;

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::NOT_FOUND
    );
    assert!(
        metric(&h.state, "vod_loaded_assets 0"),
        "the loaded copy must be evicted"
    );
}

#[tokio::test]
async fn stale_answers_cover_a_mapper_outage_until_the_window_closes() {
    let h = harness_with(|config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.stale_if_error_ms = 300;
        settings.max_retries = 0;
    })
    .await;
    let mut answer = Answer::file("v1", "h264-aac.mp4");
    answer.ttl_seconds = Some(0);
    h.mapper.state.set("movie", answer);
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );

    h.mapper.state.forced_status.store(500, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK,
        "a stale answer should cover the outage"
    );
    assert!(metric(
        &h.state,
        "vod_resolution_cache_events_total{event=\"stale\"} 1"
    ));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "stale data is not served past stale_if_error_ms"
    );
}

#[tokio::test]
async fn a_dead_location_is_never_served_stale() {
    let h = harness_with(|config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.max_retries = 0;
    })
    .await;
    let mut answer = Answer::file("v1", "h264-aac.mp4");
    answer.expires_at = Some(
        (OffsetDateTime::now_utc() + Duration::from_millis(600))
            .format(&Rfc3339)
            .unwrap(),
    );
    h.mapper.state.set("movie", answer);
    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK
    );

    h.mapper.state.forced_status.store(500, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "an expired signed location must not be reused even though stale answers are allowed"
    );
}

#[tokio::test]
async fn unsupported_media_is_a_load_failure_not_a_gateway_error() {
    let h = harness().await;
    h.mapper
        .state
        .set("modern", Answer::file("v1", "h264-mp3.mp4"));

    assert_eq!(
        status(&h.app, "/hls/modern/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(metric(
        &h.state,
        "vod_asset_loads_total{outcome=\"failed\"} 1"
    ));
    // The failure is cached briefly rather than reparsed on every request.
    assert_eq!(
        status(&h.app, "/hls/modern/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(metric(
        &h.state,
        "vod_asset_loads_total{outcome=\"failed\"} 1"
    ));
}

#[tokio::test]
async fn a_missing_file_location_is_a_bad_upstream() {
    let h = harness().await;
    h.mapper
        .state
        .set("ghost", Answer::file("v1", "does-not-exist.mp4"));
    assert_eq!(
        status(&h.app, "/hls/ghost/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

// ---------------------------------------------------------------------------------------------
// Remote media
// ---------------------------------------------------------------------------------------------

async fn remote_harness(origin: &MockOrigin) -> Harness {
    let h = harness().await;
    h.mapper
        .state
        .set("remote", Answer::http("v1", &origin.url("h264-aac.mp4")));
    h
}

#[tokio::test]
async fn a_remote_fragmented_file_is_indexed_like_the_local_one_from_a_few_requests() {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac-fragmented.mp4");
    let h = harness().await;
    h.mapper.state.set(
        "remote",
        Answer::http("v1", &origin.url("h264-aac-fragmented.mp4")),
    );
    h.mapper
        .state
        .set("local", Answer::file("v1", "h264-aac-fragmented.mp4"));

    let (status_code, _, remote_master) = fetch(&h.app, "/hls/remote/master.m3u8").await;
    assert_eq!(status_code, StatusCode::OK);
    let (_, _, local_master) = fetch(&h.app, "/hls/local/master.m3u8").await;
    assert_eq!(remote_master, local_master);
    let version = version_in(&remote_master);

    // Three fragments should cost about one request each, plus the probe, `moov`, and the
    // check that the file did not change: not one request per top-level box header.
    let requests = origin.state.requests.load(Ordering::SeqCst);
    assert!(requests <= 3 + 6, "loading made {requests} requests");
    let file_len = std::fs::metadata(fixture("h264-aac-fragmented.mp4"))
        .unwrap()
        .len();
    let served = origin.state.bytes_served.load(Ordering::SeqCst);
    assert!(
        served < file_len / 2,
        "loading read {served} of {file_len} bytes; only metadata should be needed"
    );

    for path in [
        format!("video/init.mp4?v={version}"),
        format!("audio-1/init.mp4?v={version}"),
        format!("video/segments/1/media.m4s?v={version}"),
        format!("audio-1/segments/2/media.m4s?v={version}"),
    ] {
        let (_, _, remote) = fetch(&h.app, &format!("/hls/remote/{path}")).await;
        let (_, _, local) = fetch(&h.app, &format!("/hls/local/{path}")).await;
        assert_eq!(remote, local, "{path}");
    }
}

#[tokio::test]
async fn remote_media_is_served_byte_for_byte_like_local_media() {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    let h = remote_harness(&origin).await;
    h.mapper
        .state
        .set("local", Answer::file("v1", "h264-aac.mp4"));

    let (status_code, _, remote_master) = fetch(&h.app, "/hls/remote/master.m3u8").await;
    assert_eq!(status_code, StatusCode::OK);
    let (_, _, local_master) = fetch(&h.app, "/hls/local/master.m3u8").await;
    assert_eq!(
        remote_master, local_master,
        "same media must give identical playlists"
    );
    let version = version_in(&remote_master);

    // Loading fetched only headers and metadata, never the media payload.
    let file_len = std::fs::metadata(fixture("h264-aac.mp4")).unwrap().len();
    let served = origin.state.bytes_served.load(Ordering::SeqCst);
    assert!(
        served < file_len / 2,
        "loading read {served} of {file_len} bytes; only metadata should be needed"
    );
    assert_eq!(
        origin.state.requests.load(Ordering::SeqCst),
        origin.state.ranges.lock().unwrap().len(),
        "every request to the origin must be a range request"
    );

    for path in [
        format!("video/init.mp4?v={version}"),
        format!("audio-1/init.mp4?v={version}"),
        format!("video/segments/0/media.m4s?v={version}"),
        format!("video/segments/2/media.m4s?v={version}"),
        format!("audio-1/segments/1/media.m4s?v={version}"),
    ] {
        let (remote_status, _, remote) = fetch(&h.app, &format!("/hls/remote/{path}")).await;
        let (_, _, local) = fetch(&h.app, &format!("/hls/local/{path}")).await;
        assert_eq!(remote_status, StatusCode::OK, "{path}");
        assert_eq!(
            remote, local,
            "{path} must match the locally packaged bytes"
        );
    }
}

#[tokio::test]
async fn remote_ranges_reach_the_origin_as_ranges() {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    let h = remote_harness(&origin).await;
    let (_, _, master) = fetch(&h.app, "/hls/remote/master.m3u8").await;
    let version = version_in(&master);
    origin.state.ranges.lock().unwrap().clear();

    fetch(
        &h.app,
        &format!("/hls/remote/video/segments/1/media.m4s?v={version}"),
    )
    .await;

    let ranges = origin.state.ranges.lock().unwrap().clone();
    assert!(!ranges.is_empty());
    assert!(
        ranges.iter().all(|range| range.starts_with("bytes=")),
        "{ranges:?}"
    );
}

#[tokio::test]
async fn origins_that_cannot_be_ranged_or_validated_are_refused() {
    // No range support.
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    origin.state.ignore_ranges.store(true, Ordering::SeqCst);
    let h = remote_harness(&origin).await;
    assert_eq!(
        status(&h.app, "/hls/remote/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );

    // No validator, and only a weak validator.
    for validator in [
        OriginValidator::None,
        OriginValidator::WeakETag("\"w\"".to_owned()),
    ] {
        let origin = MockOrigin::start().await;
        origin.state.add(
            "h264-aac.mp4",
            std::fs::read(fixture("h264-aac.mp4")).unwrap(),
            validator,
        );
        let h = remote_harness(&origin).await;
        assert_eq!(
            status(&h.app, "/hls/remote/master.m3u8").await,
            StatusCode::BAD_GATEWAY
        );
    }

    // Origin errors: 404 is the mapper's problem, 500 is the origin's outage.
    let origin = MockOrigin::start().await;
    let h = remote_harness(&origin).await;
    assert_eq!(
        status(&h.app, "/hls/remote/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    origin.state.forced_status.store(500, Ordering::SeqCst);
    let h = remote_harness(&origin).await;
    assert_eq!(
        status(&h.app, "/hls/remote/master.m3u8").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn last_modified_is_an_acceptable_validator() {
    let origin = MockOrigin::start().await;
    origin.state.add(
        "h264-aac.mp4",
        std::fs::read(fixture("h264-aac.mp4")).unwrap(),
        OriginValidator::LastModified("Sat, 19 Sep 2026 10:00:00 GMT".to_owned()),
    );
    let h = remote_harness(&origin).await;

    let (status_code, _, master) = fetch(&h.app, "/hls/remote/master.m3u8").await;
    assert_eq!(status_code, StatusCode::OK);
    let version = version_in(&master);
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/remote/video/segments/0/media.m4s?v={version}")
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_object_that_changes_mid_playback_fails_instead_of_mixing_versions() {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    let h = remote_harness(&origin).await;
    let (_, _, master) = fetch(&h.app, "/hls/remote/master.m3u8").await;
    let version = version_in(&master);

    origin.state.set_validator(
        "h264-aac.mp4",
        OriginValidator::ETag("\"replaced\"".to_owned()),
    );
    let response = h
        .app
        .clone()
        .oneshot(
            Request::get(format!(
                "/hls/remote/video/segments/0/media.m4s?v={version}"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await;

    assert!(
        body.is_err(),
        "the body must abort, not deliver mixed bytes"
    );
    assert!(metric(&h.state, "vod_segment_stream_aborts_error_total 1"));
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    // A literal loopback address is refused by policy.
    let strict = harness_with(|config| config.remote_media.allow_private_addresses = false).await;
    strict
        .mapper
        .state
        .set("remote", Answer::http("v1", &origin.url("h264-aac.mp4")));
    assert_eq!(
        status(&strict.app, "/hls/remote/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        origin.state.requests.load(Ordering::SeqCst),
        0,
        "nothing may reach the origin"
    );

    // A name that resolves to loopback is refused when the connection is made.
    let by_name = harness_with(|config| {
        config.remote_media.allowed_hosts = vec!["localhost".to_owned()];
        config.remote_media.allow_private_addresses = false;
    })
    .await;
    by_name.mapper.state.set(
        "remote",
        Answer::http(
            "v1",
            &format!(
                "http://localhost:{}/media/h264-aac.mp4",
                origin.address.port()
            ),
        ),
    );
    assert_eq!(
        status(&by_name.app, "/hls/remote/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(origin.state.requests.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------------------------
// Readiness, preload, and cache behavior
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn readiness_follows_mapper_health() {
    let h = harness_with(|config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.readiness_probe_interval_ms = 40;
    })
    .await;
    spawn_resolver_probe(&h.state);
    assert_eq!(status(&h.app, "/ready").await, StatusCode::OK);

    h.mapper.state.unhealthy.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        status(&h.app, "/ready").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        status(&h.app, "/health").await,
        StatusCode::OK,
        "liveness is unaffected"
    );

    h.mapper.state.unhealthy.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(status(&h.app, "/ready").await, StatusCode::OK);
}

#[tokio::test]
async fn admin_status_reflects_the_mapper_live_not_the_background_probe() {
    // No readiness_probe_interval_ms is set, so the background flag never updates; the status
    // endpoint must still check the mapper itself rather than trust that stale flag.
    let h = harness().await;
    h.mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();
    assert_eq!(json["resolver"]["kind"], "mapper");
    assert_eq!(json["resolver"]["healthy"], true);
    assert!(
        json["resolver"]["base_url"]
            .as_str()
            .unwrap()
            .contains(&h.mapper.address.port().to_string()),
        "{json}"
    );
    assert_eq!(json["resolver"]["known_assets"], serde_json::Value::Null);
    assert_eq!(json["cache"]["count"], 0);

    h.mapper.state.unhealthy.store(true, Ordering::SeqCst);
    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();
    assert_eq!(json["resolver"]["healthy"], false, "checked live: {json}");
}

#[tokio::test]
async fn admin_status_lists_a_loaded_mapper_asset() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::file("v1", "h264-aac.mp4").with_subtitle("en", "subtitles-en.vtt"),
    );
    status(&h.app, "/hls/movie/master.m3u8").await;

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();

    assert_eq!(json["cache"]["count"], 1);
    let cached = &json["cache"]["assets"][0];
    assert_eq!(cached["asset_id"], "movie");
    assert_eq!(cached["version"], "v1");
    assert_eq!(cached["subtitles"], 1);
    assert!(cached["tracks"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn a_bad_catalog_asset_stops_startup_and_names_the_asset() {
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/registry-tests");
    std::fs::create_dir_all(&directory).unwrap();
    let bad = directory.join("garbage.mp4");
    std::fs::write(&bad, b"not an mp4 file at all").unwrap();
    let mut assets = BTreeMap::new();
    assets.insert("good".to_owned(), fixture("h264-aac.mp4"));
    assets.insert("bad".to_owned(), bad.canonicalize().unwrap());
    let config = Config::for_catalog(fixtures_dir(), assets, 1000, LimitsConfig::default());

    let error = AppState::new(&config)
        .unwrap()
        .preload()
        .await
        .expect_err("a bad asset must fail startup");

    assert!(error.to_string().contains("asset `bad`"), "{error}");
}

#[tokio::test]
async fn the_static_catalog_still_loads_lazily_when_preload_is_off() {
    let mut assets = BTreeMap::new();
    assets.insert("sample".to_owned(), fixture("h264-aac.mp4"));
    let mut config = Config::for_catalog(fixtures_dir(), assets, 1000, LimitsConfig::default());
    config.registry.preload = false;
    let state = AppState::new(&config).unwrap();
    state.preload().await.unwrap();
    assert!(
        metric(&state, "vod_loaded_assets 0"),
        "nothing loaded before the first request"
    );

    let app = router(state.clone());
    assert_eq!(
        status(&app, "/hls/sample/master.m3u8").await,
        StatusCode::OK
    );
    assert!(metric(&state, "vod_loaded_assets 1"));
}

#[tokio::test]
async fn least_recently_used_assets_are_evicted_over_the_byte_budget() {
    let mut assets = BTreeMap::new();
    assets.insert("a".to_owned(), fixture("h264-aac.mp4"));
    assets.insert("b".to_owned(), fixture("h264-video-only.mp4"));
    assets.insert("c".to_owned(), fixture("h264-variable-timing.mp4"));
    // A budget one byte short of all three, so any two fit but not three.
    let mut total = 0;
    for name in [
        "h264-aac.mp4",
        "h264-video-only.mp4",
        "h264-variable-timing.mp4",
    ] {
        total +=
            crate::asset::PackagedAsset::load_local(fixture(name), 1000, &LimitsConfig::default())
                .await
                .unwrap()
                .index_bytes();
    }
    let limits = LimitsConfig {
        max_index_bytes: total - 1,
        ..LimitsConfig::default()
    };
    let mut config = Config::for_catalog(fixtures_dir(), assets, 1000, limits);
    config.registry.preload = false;
    let state = AppState::new(&config).unwrap();
    let app = router(state.clone());

    for id in ["a", "b", "c"] {
        assert_eq!(
            status(&app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::OK
        );
    }

    let text = state.metrics.render(0);
    assert!(
        text.lines().any(|line| line == "vod_loaded_assets 2"),
        "the budget holds two assets:\n{text}"
    );
    // The evicted asset reloads transparently.
    assert_eq!(status(&app, "/hls/a/master.m3u8").await, StatusCode::OK);
}

// ---------------------------------------------------------------------------------------------
// Signed URLs
// ---------------------------------------------------------------------------------------------

/// A remote asset whose URL carries a signature the origin insists on.
async fn signed_harness(tune: impl FnOnce(&mut Config)) -> (Harness, MockOrigin) {
    let origin = MockOrigin::start().await;
    origin.add_fixture("h264-aac.mp4");
    *origin.state.required_query.lock().unwrap() = Some("sig=1".to_owned());
    let h = harness_with(tune).await;
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    (h, origin)
}

fn signed(origin: &MockOrigin, signature: &str) -> String {
    format!("{}?sig={signature}", origin.url("h264-aac.mp4"))
}

fn short_ttl(mut answer: Answer) -> Answer {
    answer.ttl_seconds = Some(0);
    answer
}

#[tokio::test]
async fn a_rotated_signature_is_applied_in_place_without_reloading() {
    let (h, origin) = signed_harness(|_| {}).await;
    h.mapper.state.set(
        "movie",
        short_ttl(Answer::http("v1", &signed(&origin, "1"))),
    );
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let version = version_in(&master);

    // The mapper re-signs the same object and the old signature stops working.
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper.state.set(
        "movie",
        short_ttl(Answer::http("v1", &signed(&origin, "2"))),
    );
    tokio::time::sleep(Duration::from_millis(40)).await;
    let before = origin.state.queries.lock().unwrap().len();

    let (status_code, _, segment) = fetch(
        &h.app,
        &format!("/hls/movie/video/segments/0/media.m4s?v={version}"),
    )
    .await;

    assert_eq!(status_code, StatusCode::OK);
    assert!(!segment.is_empty());
    let used = origin.state.queries.lock().unwrap()[before..].to_vec();
    assert!(
        !used.is_empty() && used.iter().all(|query| query == "sig=2"),
        "the first read after rotation must already use the new signature, got {used:?}"
    );
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"),
        "the asset must not reload"
    );
    assert!(metric(&h.state, "vod_location_rotations_total 1"));
    assert_eq!(
        origin.state.queries.lock().unwrap().last().unwrap(),
        "sig=2"
    );
}

#[tokio::test]
async fn signed_locations_are_refreshed_before_they_expire() {
    let (h, origin) = signed_harness(|config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.refresh_margin_ms = 1000;
    })
    .await;
    let expiry = |millis: u64| {
        (OffsetDateTime::now_utc() + Duration::from_millis(millis))
            .format(&Rfc3339)
            .unwrap()
    };
    let mut first = Answer::http("v1", &signed(&origin, "1"));
    first.expires_at = Some(expiry(1500));
    h.mapper.state.set("movie", first);
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let version = version_in(&master);
    assert_eq!(h.mapper.state.calls(), 1);

    // Halfway to the deadline the next request asks for a fresh signature, while the old one
    // still works.
    let mut second = Answer::http("v1", &signed(&origin, "2"));
    second.expires_at = Some(expiry(10_000));
    h.mapper.state.set("movie", second);
    tokio::time::sleep(Duration::from_millis(900)).await;
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());

    let status_code = status(
        &h.app,
        &format!("/hls/movie/video/segments/1/media.m4s?v={version}"),
    )
    .await;

    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(
        h.mapper.state.calls(),
        2,
        "refreshed ahead of the 1.5 s deadline"
    );
    assert!(metric(&h.state, "vod_location_rotations_total 1"));
}

#[tokio::test]
async fn an_origin_rejection_triggers_one_refresh_and_the_read_succeeds() {
    let (h, origin) = signed_harness(|_| {}).await;
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "1")));
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let version = version_in(&master);

    // The signature is revoked well before the answer's TTL ends.
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "2")));
    // The refresh is skipped for locations fetched moments ago, so let the answer age.
    tokio::time::sleep(Duration::from_millis(2100)).await;

    let (status_code, _, segment) = fetch(
        &h.app,
        &format!("/hls/movie/video/segments/0/media.m4s?v={version}"),
    )
    .await;

    assert_eq!(status_code, StatusCode::OK);
    assert!(
        !segment.is_empty(),
        "the retried read must deliver the segment"
    );
    assert_eq!(h.mapper.state.calls(), 2, "exactly one refresh");
    assert!(metric(&h.state, "vod_location_rotations_total 1"));
}

#[tokio::test]
async fn concurrent_rejections_share_one_refresh() {
    let (h, origin) = signed_harness(|_| {}).await;
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "1")));
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let version = version_in(&master);
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "2")));
    tokio::time::sleep(Duration::from_millis(2100)).await;

    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..3 {
        let app = h.app.clone();
        let uri = format!("/hls/movie/video/segments/{index}/media.m4s?v={version}");
        tasks.spawn(async move { status(&app, &uri).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap(), StatusCode::OK);
    }

    assert_eq!(
        h.mapper.state.calls(),
        2,
        "one initial lookup and one shared refresh"
    );
}

#[tokio::test]
async fn a_location_the_mapper_cannot_fix_fails_after_one_refresh() {
    let (h, origin) = signed_harness(|_| {}).await;
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "1")));
    let (_, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let version = version_in(&master);

    // The origin now refuses every signature the mapper can produce.
    *origin.state.required_query.lock().unwrap() = Some("sig=never".to_owned());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let response = h
        .app
        .clone()
        .oneshot(
            Request::get(format!("/hls/movie/video/segments/0/media.m4s?v={version}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await;

    assert!(body.is_err(), "the stream must end in an error, not loop");
    assert!(
        h.mapper.state.calls() <= 3,
        "one refresh at most, then give up"
    );
}

#[tokio::test]
async fn a_rejected_location_while_loading_is_a_bad_gateway() {
    let (h, origin) = signed_harness(|_| {}).await;
    // The very first signature is already refused.
    h.mapper
        .state
        .set("movie", Answer::http("v1", &signed(&origin, "expired")));

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(h.mapper.state.calls(), 1, "no refresh loop during a load");
}

#[test]
fn locations_that_differ_only_in_their_query_are_the_same_object() {
    use crate::resolver::AssetLocation::{File, Http};
    let url = |value: &str| Http(reqwest::Url::parse(value).unwrap());

    assert!(
        url("https://o.example/a.mp4?sig=1").same_object(&url("https://o.example/a.mp4?sig=2"))
    );
    assert!(url("https://o.example/a.mp4").same_object(&url("https://o.example:443/a.mp4?x=1")));
    assert!(!url("https://o.example/a.mp4").same_object(&url("https://o.example/b.mp4")));
    assert!(!url("https://o.example/a.mp4").same_object(&url("https://p.example/a.mp4")));
    assert!(!url("https://o.example/a.mp4").same_object(&url("http://o.example/a.mp4")));
    assert!(File("a.mp4".into()).same_object(&File("a.mp4".into())));
    assert!(!File("a.mp4".into()).same_object(&url("https://o.example/a.mp4")));
}

// ---------------------------------------------------------------------------------------------
// Sidecar subtitles
// ---------------------------------------------------------------------------------------------

fn text(bytes: &Bytes) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn subtitles_from_the_mapper_appear_in_both_protocols_and_are_served() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::file("v1", "h264-aac.mp4")
            .with_subtitle("en", "subtitles-en.vtt")
            .with_subtitle("fr", "subtitles-fr.vtt"),
    );

    let (status, _, master) = fetch(&h.app, "/hls/movie/master.m3u8").await;
    let master_text = text(&master);
    let version = version_in(&master);

    assert_eq!(status, StatusCode::OK);
    assert!(master_text.contains("TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"EN\",LANGUAGE=\"en\""));
    assert!(master_text.contains(&format!("URI=\"subtitles/fr/index.m3u8?v={version}\"")));
    assert!(master_text.contains(",SUBTITLES=\"subs\""), "{master_text}");

    let (status, headers, playlist) = fetch(&h.app, "/hls/movie/subtitles/en/index.m3u8").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/vnd.apple.mpegurl");
    assert!(text(&playlist).contains(&format!("sub.vtt?v={version}")));

    let uri = format!("/hls/movie/subtitles/en/sub.vtt?v={version}");
    let (status, headers, file) = fetch(&h.app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/vtt; charset=utf-8");
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(text(&file).starts_with("WEBVTT"));
    assert_eq!(
        fetch(
            &h.app,
            &format!("/dash/movie/subtitles/en/sub.vtt?v={version}")
        )
        .await
        .2,
        file
    );

    let (_, _, manifest) = fetch(&h.app, "/dash/movie/manifest.mpd").await;
    let manifest = text(&manifest);
    assert!(
        manifest.contains("lang=\"fr\" mimeType=\"text/vtt\""),
        "{manifest}"
    );
    assert!(manifest.contains(&format!(
        "<BaseURL>subtitles/en/sub.vtt?v={version}</BaseURL>"
    )));
}

#[tokio::test]
async fn subtitle_urls_need_the_current_version_and_a_listed_language() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::file("v1", "h264-aac.mp4").with_subtitle("en", "subtitles-en.vtt"),
    );
    let version = version_in(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    for uri in [
        "/hls/movie/subtitles/en/sub.vtt".to_owned(),
        "/hls/movie/subtitles/en/sub.vtt?v=stale".to_owned(),
        format!("/hls/movie/subtitles/de/sub.vtt?v={version}"),
        "/hls/movie/subtitles/de/index.m3u8".to_owned(),
    ] {
        assert_eq!(status(&h.app, &uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    let uri = format!("/hls/movie/subtitles/EN/sub.vtt?v={version}");
    assert_eq!(
        status(&h.app, &uri).await,
        StatusCode::OK,
        "language matches without case"
    );
}

#[tokio::test]
async fn an_asset_without_subtitles_is_unchanged() {
    let h = harness().await;
    h.mapper
        .state
        .set("movie", Answer::file("v1", "h264-aac.mp4"));

    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    assert!(!master.contains("SUBTITLES"), "{master}");
    assert!(!text(&fetch(&h.app, "/dash/movie/manifest.mpd").await.2).contains("text/vtt"));
}

#[tokio::test]
async fn changing_a_caption_gives_the_asset_new_urls() {
    let h = harness().await;
    let mut first = Answer::file("v1", "h264-aac.mp4").with_subtitle("en", "subtitles-en.vtt");
    first.ttl_seconds = Some(0);
    h.mapper.state.set("movie", first);
    let old = version_in(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    // The mapper changes its version when a caption changes, as its contract says it must.
    let mut second = Answer::file("v2", "h264-aac.mp4").with_subtitle("en", "subtitles-fr.vtt");
    second.ttl_seconds = Some(0);
    h.mapper.state.set("movie", second);
    tokio::time::sleep(Duration::from_millis(40)).await;
    let new = version_in(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    assert_ne!(old, new);
    assert_eq!(
        status(&h.app, &format!("/hls/movie/subtitles/en/sub.vtt?v={old}")).await,
        StatusCode::NOT_FOUND,
        "the old URL stops resolving"
    );
    let file = fetch(&h.app, &format!("/hls/movie/subtitles/en/sub.vtt?v={new}"))
        .await
        .2;
    assert!(text(&file).contains("Bonjour"));
}

#[tokio::test]
async fn a_bad_subtitle_fails_the_asset() {
    for (path, expected) in [
        // Not WebVTT: the media is fine and the file is not.
        ("subtitles-bad.srt", StatusCode::INTERNAL_SERVER_ERROR),
        // Not there: the mapper pointed at something that does not exist.
        ("missing.vtt", StatusCode::BAD_GATEWAY),
    ] {
        let h = harness().await;
        h.mapper.state.set(
            "movie",
            Answer::file("v1", "h264-aac.mp4").with_subtitle("fr", path),
        );

        assert_eq!(
            status(&h.app, "/hls/movie/master.m3u8").await,
            expected,
            "{path}"
        );
    }
}

#[tokio::test]
async fn subtitle_limits_are_enforced() {
    let h = harness_with(|config| config.limits.max_subtitle_bytes = 20).await;
    h.mapper.state.set(
        "movie",
        Answer::file("v1", "h264-aac.mp4").with_subtitle("en", "subtitles-en.vtt"),
    );
    assert_ne!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK,
        "a file over max_subtitle_bytes fails the asset"
    );

    let h = harness_with(|config| config.limits.max_subtitles = 1).await;
    h.mapper.state.set(
        "movie",
        Answer::file("v1", "h264-aac.mp4")
            .with_subtitle("en", "subtitles-en.vtt")
            .with_subtitle("fr", "subtitles-fr.vtt"),
    );
    assert_ne!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::OK,
        "more files than max_subtitles fails the asset"
    );
}

#[tokio::test]
async fn invalid_subtitle_entries_from_the_mapper_are_rejected() {
    for entries in [
        // A language that is not a URL-safe tag.
        r#"[{"language":"../x","location":{"type":"file","path":"subtitles-en.vtt"}}]"#,
        // The same language twice, differing only in case.
        r#"[{"language":"en","location":{"type":"file","path":"subtitles-en.vtt"}},{"language":"EN","location":{"type":"file","path":"subtitles-fr.vtt"}}]"#,
        // Two defaults.
        r#"[{"language":"en","default":true,"location":{"type":"file","path":"subtitles-en.vtt"}},{"language":"fr","default":true,"location":{"type":"file","path":"subtitles-fr.vtt"}}]"#,
        // A path that escapes the media root.
        r#"[{"language":"en","location":{"type":"file","path":"../secret.vtt"}}]"#,
        // A remote host the policy does not allow.
        r#"[{"language":"en","location":{"type":"http","url":"http://evil.example/x.vtt"}}]"#,
    ] {
        let h = harness().await;
        *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
            r#"{{"asset_id":"movie","version":"v1","location":{{"type":"file","path":"h264-aac.mp4"}},"subtitles":{entries}}}"#
        ));
        assert_eq!(
            status(&h.app, "/hls/movie/master.m3u8").await,
            StatusCode::BAD_GATEWAY,
            "{entries}"
        );
    }
}

#[tokio::test]
async fn cues_follow_the_shared_offset_of_the_edit_lists_and_nothing_else() {
    let h = harness().await;
    for (asset, file) in [
        // The edit lists trim encoder delay, so everything is served 66.7 ms later than the source.
        ("edited", "h264-aac-default-edits.mp4"),
        // The video starts 1.5 s in, which the presentation the cues were written against already
        // contains: no shift, or the cues would be late by that much.
        ("delayed", "h264-aac-video-delay.mp4"),
        ("plain", "h264-aac.mp4"),
    ] {
        h.mapper.state.set(
            asset,
            Answer::file("v1", file).with_subtitle("en", "subtitles-en.vtt"),
        );
    }

    let mut served = Vec::new();
    for asset in ["edited", "delayed", "plain"] {
        let version = version_in(&fetch(&h.app, &format!("/hls/{asset}/master.m3u8")).await.2);
        let uri = format!("/hls/{asset}/subtitles/en/sub.vtt?v={version}");
        served.push(text(&fetch(&h.app, &uri).await.2));
    }

    assert!(
        served[0].contains("00:00:00.567 --> 00:00:01.567 line:90%"),
        "{}",
        served[0]
    );
    assert!(
        served[0].contains("00:00:01.567 --> 00:00:02.567\nWorld"),
        "{}",
        served[0]
    );
    for unchanged in &served[1..] {
        assert!(
            unchanged.contains("00:00.500 --> 00:01.500 line:90%"),
            "{unchanged}"
        );
    }
}
