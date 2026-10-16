//! Registry, mapper, and remote-media tests against in-process mapper and origin servers.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine;
use bytes::Bytes;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

use crate::config::{
    Config, LimitsConfig, MapperConfig, RemoteMediaConfig, ResolverSettings, Secret,
};
use crate::http::{AppState, router, spawn_resolver_probe};
use crate::testutil::{
    Answer, MockMapper, MockOrigin, OriginValidator, file_location, fixture, fixtures_dir,
    http_location, tfdt,
};

fn mapper_settings(url: &str) -> MapperConfig {
    MapperConfig {
        base_url: url.to_owned(),
        bearer_token: None,
        bearer_token_file: None,
        bearer_token_reload_ms: 0,
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
    assert_ne!(ranges, Vec::<String>::new());
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
    assert_ne!(segment, Vec::<u8>::new());
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
        // Neither WebVTT nor SubRip: the media is fine and the file is not.
        (
            "subtitles-not-a-subtitle.txt",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        // SubRip, with a cue timing line that cannot be read.
        (
            "subtitles-bad-timing.srt",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        // TTML, with a time expression that cannot be read.
        ("subtitles-bad-time.ttml", StatusCode::INTERNAL_SERVER_ERROR),
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

/// A `SubRip` file is served as the `WebVTT` it is converted to, in both protocols, and its cues
/// move with the timeline exactly as a `WebVTT` file's do.
#[tokio::test]
async fn srt_subtitles_are_served_as_webvtt_and_follow_the_timeline() {
    let h = harness().await;
    for (asset, file) in [
        ("edited", "h264-aac-default-edits.mp4"),
        ("plain", "h264-aac.mp4"),
    ] {
        h.mapper.state.set(
            asset,
            Answer::file("v1", file).with_subtitle("en", "subtitles-en.srt"),
        );
    }

    let version = version_in(&fetch(&h.app, "/hls/plain/master.m3u8").await.2);
    let (status, headers, file) = fetch(
        &h.app,
        &format!("/hls/plain/subtitles/en/sub.vtt?v={version}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/vtt; charset=utf-8");
    assert_eq!(
        text(&file),
        "WEBVTT\n\n00:00:00.500 --> 00:00:01.500\nHello\n\n00:00:01.500 --> 00:00:02.500\n<i>World</i> &amp; friends\n"
    );
    // DASH serves the same bytes.
    let dash = fetch(
        &h.app,
        &format!("/dash/plain/subtitles/en/sub.vtt?v={version}"),
    )
    .await;
    assert_eq!(dash.2, file);
    assert!(
        text(&fetch(&h.app, "/dash/plain/manifest.mpd").await.2).contains("mimeType=\"text/vtt\"")
    );

    // The edit lists put everything 66.7 ms later, cues included.
    let edited_version = version_in(&fetch(&h.app, "/hls/edited/master.m3u8").await.2);
    let edited = text(
        &fetch(
            &h.app,
            &format!("/hls/edited/subtitles/en/sub.vtt?v={edited_version}"),
        )
        .await
        .2,
    );
    assert!(
        edited.contains("00:00:00.567 --> 00:00:01.567\nHello"),
        "{edited}"
    );
    assert!(
        edited.contains("00:00:01.567 --> 00:00:02.567\n<i>World</i>"),
        "{edited}"
    );

    // The version depends on the bytes served, so changing a caption, in either format, gives
    // new URLs.
    h.mapper.state.set(
        "other",
        Answer::file("v1", "h264-aac.mp4").with_subtitle("en", "subtitles-fr.vtt"),
    );
    let other = version_in(&fetch(&h.app, "/hls/other/master.m3u8").await.2);
    assert_ne!(version, other);
}

/// A TTML file is served as the `WebVTT` it is converted to, in both protocols, and its cues move
/// with the timeline exactly as a `WebVTT` file's do.
#[tokio::test]
async fn ttml_subtitles_are_served_as_webvtt_and_follow_the_timeline() {
    let h = harness().await;
    for (asset, file) in [
        ("edited", "h264-aac-default-edits.mp4"),
        ("plain", "h264-aac.mp4"),
    ] {
        h.mapper.state.set(
            asset,
            Answer::file("v1", file).with_subtitle("en", "subtitles-en.ttml"),
        );
    }

    let version = version_in(&fetch(&h.app, "/hls/plain/master.m3u8").await.2);
    let (status, headers, file) = fetch(
        &h.app,
        &format!("/hls/plain/subtitles/en/sub.vtt?v={version}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/vtt; charset=utf-8");
    assert_eq!(
        text(&file),
        "WEBVTT\n\n00:00:00.500 --> 00:00:01.500\nHello\n\n00:00:01.500 --> 00:00:02.500\n<i>World &amp; friends\nsecond line</i>\n"
    );
    let dash = fetch(
        &h.app,
        &format!("/dash/plain/subtitles/en/sub.vtt?v={version}"),
    )
    .await;
    assert_eq!(dash.2, file);

    // The edit lists put everything 66.7 ms later, cues included.
    let edited_version = version_in(&fetch(&h.app, "/hls/edited/master.m3u8").await.2);
    let edited = text(
        &fetch(
            &h.app,
            &format!("/hls/edited/subtitles/en/sub.vtt?v={edited_version}"),
        )
        .await
        .2,
    );
    assert!(
        edited.contains("00:00:00.567 --> 00:00:01.567\nHello"),
        "{edited}"
    );
    assert!(
        edited.contains("00:00:01.567 --> 00:00:02.567\n<i>World"),
        "{edited}"
    );
}

// ---------------------------------------------------------------------------------------------
// Adaptive renditions (TDD 0006 §3)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_two_rung_ladder_serves_both_renditions_with_shared_audio() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("480p", "rendition-480p.mp4"),
            ],
        ),
    );

    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    let version = version_in(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    assert_eq!(
        master.matches("#EXT-X-STREAM-INF").count(),
        2,
        "one variant per rendition: {master}"
    );
    // Sorted ascending by bandwidth: 480p (smaller) before 720p.
    let variant_480 = master.find("video-480p/index.m3u8").unwrap();
    let variant_720 = master.find("video-720p/index.m3u8").unwrap();
    assert!(variant_480 < variant_720, "{master}");
    assert!(master.contains(",AUDIO=\"audio\""), "{master}");
    // No dedicated audio rendition was listed, so the shared group comes from the first video
    // rendition that has audio: 720p, listed first in the answer.
    assert!(
        master.contains(&format!("URI=\"audio-1/index.m3u8?v={version}\"")),
        "{master}"
    );
    assert_eq!(master.matches("TYPE=AUDIO").count(), 1, "{master}");

    for rendition in ["720p", "480p"] {
        let playlist = text(
            &fetch(
                &h.app,
                &format!("/hls/movie/video-{rendition}/index.m3u8?v={version}"),
            )
            .await
            .2,
        );
        assert!(playlist.contains("init.mp4?v="), "{rendition}: {playlist}");
        assert_eq!(
            playlist.matches("#EXTINF:").count(),
            3,
            "{rendition}: {playlist}"
        );

        let init = fetch(
            &h.app,
            &format!("/hls/movie/video-{rendition}/init.mp4?v={version}"),
        )
        .await;
        assert_eq!(init.0, StatusCode::OK, "{rendition}");
        assert_eq!(&init.2[4..8], b"ftyp", "{rendition}");

        let segment = fetch(
            &h.app,
            &format!("/hls/movie/video-{rendition}/segments/0/media.m4s?v={version}"),
        )
        .await;
        assert_eq!(segment.0, StatusCode::OK, "{rendition}");
        assert_eq!(&segment.2[4..8], b"moof", "{rendition}");
    }

    // The shared audio group is reachable at audio-1 regardless of which rendition supplies it.
    let audio_segment = fetch(
        &h.app,
        &format!("/hls/movie/audio-1/segments/0/media.m4s?v={version}"),
    )
    .await;
    assert_eq!(audio_segment.0, StatusCode::OK);
    assert_eq!(&audio_segment.2[4..8], b"moof");

    // A rendition-scoped audio URL, or an unrendered video URL, do not exist.
    assert_eq!(
        status(&h.app, &format!("/hls/movie/video/index.m3u8?v={version}")).await,
        StatusCode::NOT_FOUND
    );

    let manifest = text(&fetch(&h.app, "/dash/movie/manifest.mpd").await.2);
    assert_eq!(manifest.matches("id=\"video-").count(), 2, "{manifest}");
    assert!(manifest.contains("id=\"video-720p\""), "{manifest}");
    assert!(manifest.contains("id=\"video-480p\""), "{manifest}");
    assert!(manifest.contains("id=\"audio-1\""), "{manifest}");
}

#[tokio::test]
async fn dedicated_audio_renditions_replace_each_videos_own_audio() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("480p", "rendition-480p.mp4"),
                ("audio-en", "rendition-audio-en.m4a"),
                ("audio-es", "rendition-audio-es.m4a"),
            ],
        ),
    );

    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    let version = version_in(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    assert_eq!(master.matches("TYPE=AUDIO").count(), 2, "{master}");
    assert!(master.contains("URI=\"audio-1/index.m3u8"), "{master}");
    assert!(master.contains("URI=\"audio-2/index.m3u8"), "{master}");
    assert!(master.contains("LANGUAGE=\"spa\""), "{master}");

    let audio_1 = fetch(
        &h.app,
        &format!("/hls/movie/audio-1/segments/0/media.m4s?v={version}"),
    )
    .await;
    assert_eq!(audio_1.0, StatusCode::OK);
    let audio_2 = fetch(
        &h.app,
        &format!("/hls/movie/audio-2/segments/0/media.m4s?v={version}"),
    )
    .await;
    assert_eq!(audio_2.0, StatusCode::OK);

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();
    assert_eq!(json["cache"]["assets"][0]["tracks"], 4, "2 video + 2 audio");
}

#[tokio::test]
async fn misaligned_renditions_are_refused_naming_both() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("bad", "rendition-misaligned.mp4"),
            ],
        ),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the real reason, naming both renditions, is in the asset_load_failed log line"
    );
}

#[tokio::test]
async fn a_rendition_count_over_the_limit_is_refused() {
    let h = harness_with(|config| config.limits.max_renditions = 1).await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("480p", "rendition-480p.mp4"),
            ],
        ),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn one_bad_rendition_fails_the_whole_asset() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("missing", "does-not-exist.mp4"),
            ],
        ),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the real reason, naming the rendition, is in the asset_load_failed log line"
    );
}

#[tokio::test]
async fn invalid_rendition_ids_are_rejected() {
    let h = harness().await;
    for entries in [
        // Not URL-safe.
        r#"[{"id":"720p/x","location":{"type":"file","path":"rendition-720p.mp4"}}]"#,
        // Listed twice.
        r#"[{"id":"720p","location":{"type":"file","path":"rendition-720p.mp4"}},{"id":"720p","location":{"type":"file","path":"rendition-480p.mp4"}}]"#,
    ] {
        *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
            r#"{{"asset_id":"movie","version":"v1","renditions":{entries}}}"#
        ));
        assert_eq!(
            status(&h.app, "/hls/movie/master.m3u8").await,
            StatusCode::BAD_GATEWAY,
            "{entries}"
        );
    }
}

#[tokio::test]
async fn a_mapper_answer_cannot_set_both_location_and_renditions() {
    let h = harness().await;
    *h.mapper.state.raw_body.lock().unwrap() = Some(
        r#"{"asset_id":"movie","version":"v1","location":{"type":"file","path":"rendition-720p.mp4"},"renditions":[{"id":"720p","location":{"type":"file","path":"rendition-720p.mp4"}}]}"#
            .to_owned(),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

// ---------------------------------------------------------------------------------------------
// Clipping and concatenation (TDD 0008)
// ---------------------------------------------------------------------------------------------

fn clip(
    path: &str,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
) -> (serde_json::Value, Option<u64>, Option<u64>) {
    (file_location(path), from_ms, to_ms)
}

/// Like [`clip`], with an own `encryption` override for [`Answer::clips_with_encryption`].
fn clip_encrypted(
    path: &str,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    encryption: Option<serde_json::Value>,
) -> (
    serde_json::Value,
    Option<u64>,
    Option<u64>,
    Option<serde_json::Value>,
) {
    (file_location(path), from_ms, to_ms, encryption)
}

#[tokio::test]
async fn a_single_clip_trims_one_file_and_keeps_its_urls() {
    let h = harness().await;
    h.mapper.state.set(
        "trimmed",
        Answer::clips("v1", &[clip("h264-aac.mp4", Some(1500), None)]),
    );

    let (code, _, master) = fetch(&h.app, "/hls/trimmed/master.m3u8").await;
    let version = version_in(&master);
    let playlist = text(&fetch(&h.app, "/hls/trimmed/video/index.m3u8").await.2);

    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        playlist.matches("#EXTINF:").count(),
        2,
        "from the keyframe at 1 s: {playlist}"
    );
    assert!(!playlist.contains("DISCONTINUITY"), "{playlist}");
    assert_eq!(
        status(&h.app, &format!("/hls/trimmed/video/init.mp4?v={version}")).await,
        StatusCode::OK
    );
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/trimmed/video/segments/1/media.m4s?v={version}")
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/trimmed/video/segments/2/media.m4s?v={version}")
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_clip_cut_from_a_fragmented_file_trims_the_same_way() {
    let h = harness().await;
    h.mapper.state.set(
        "trimmed",
        Answer::clips("v1", &[clip("h264-aac-fragmented.mp4", Some(1500), None)]),
    );

    let playlist = text(&fetch(&h.app, "/hls/trimmed/video/index.m3u8").await.2);

    assert_eq!(playlist.matches("#EXTINF:").count(), 2, "{playlist}");
}

#[tokio::test]
async fn a_whole_file_clip_serves_the_same_media_bytes_as_the_file() {
    let h = harness().await;
    h.mapper
        .state
        .set("plain", Answer::file("v1", "h264-aac.mp4"));
    h.mapper.state.set(
        "whole",
        Answer::clips("v1", &[clip("h264-aac.mp4", None, None)]),
    );

    let plain_version = version_in(&fetch(&h.app, "/hls/plain/master.m3u8").await.2);
    let whole_version = version_in(&fetch(&h.app, "/hls/whole/master.m3u8").await.2);

    assert_ne!(
        plain_version, whole_version,
        "a clip's window is part of its version"
    );
    for track in ["video", "audio-1"] {
        for segment in 0..3 {
            let plain = fetch(
                &h.app,
                &format!("/hls/plain/{track}/segments/{segment}/media.m4s?v={plain_version}"),
            )
            .await
            .2;
            let whole = fetch(
                &h.app,
                &format!("/hls/whole/{track}/segments/{segment}/media.m4s?v={whole_version}"),
            )
            .await
            .2;
            assert_ne!(plain, Vec::<u8>::new());
            assert_eq!(plain, whole, "{track} segment {segment}");
        }
    }
}

#[tokio::test]
async fn a_clip_window_is_part_of_the_url_version() {
    let h = harness().await;
    for (id, to_ms) in [("a", 2000), ("b", 3000), ("c", 2000)] {
        h.mapper.state.set(
            id,
            Answer::clips("v1", &[clip("h264-aac.mp4", Some(0), Some(to_ms))]),
        );
    }

    let a = version_in(&fetch(&h.app, "/hls/a/master.m3u8").await.2);
    let b = version_in(&fetch(&h.app, "/hls/b/master.m3u8").await.2);
    let c = version_in(&fetch(&h.app, "/hls/c/master.m3u8").await.2);

    assert_ne!(a, b);
    assert_eq!(
        a, c,
        "the same clips under the same mapper version give the same URLs"
    );
}

#[tokio::test]
async fn malformed_clip_answers_are_rejected() {
    let h = harness_with(|config| config.limits.max_clips = 2).await;
    let clip = r#"{"location":{"type":"file","path":"h264-aac.mp4"}}"#;
    let cases = [
        // Both a location and clips.
        format!(r#""location":{{"type":"file","path":"h264-aac.mp4"}},"clips":[{clip}]"#),
        // An empty list.
        r#""clips":[]"#.to_owned(),
        // An end that is not after the start.
        r#""clips":[{"location":{"type":"file","path":"h264-aac.mp4"},"from_ms":2000,"to_ms":2000}]"#.to_owned(),
        // A time over 2^32 - 1.
        r#""clips":[{"location":{"type":"file","path":"h264-aac.mp4"},"to_ms":4294967296}]"#.to_owned(),
        // Subtitles alongside clips.
        format!(r#""clips":[{clip}],"subtitles":[{{"language":"en","location":{{"type":"file","path":"subtitles-en.vtt"}}}}]"#),
        // More than limits.max_clips.
        format!(r#""clips":[{clip},{clip},{clip}]"#),
        // A location the path rules refuse.
        r#""clips":[{"location":{"type":"file","path":"../h264-aac.mp4"}}]"#.to_owned(),
        // A clip's own `encryption` (TDD 0009, "Different keys per clip") that fails the same
        // rules as the answer's own: a key that is not 32 hex digits.
        format!(
            r#""clips":[{{"location":{{"type":"file","path":"h264-aac.mp4"}},"encryption":{{"scheme":"cbcs","keys":[{{"key_id":"{KEY_ID}","key":"{KEY}zz"}}]}}}}]"#
        ),
    ];
    for (index, fields) in cases.iter().enumerate() {
        // A different ID per case, so no case is answered from another's cached failure.
        let id = format!("bad{index}");
        *h.mapper.state.raw_body.lock().unwrap() =
            Some(format!(r#"{{"asset_id":"{id}","version":"v1",{fields}}}"#));
        let (code, _, body) = fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await;
        assert_eq!(code, StatusCode::BAD_GATEWAY, "{fields}");
        assert!(
            !String::from_utf8_lossy(&body).contains(KEY),
            "a clip's own rejected key must not leak either: {fields}"
        );
    }
}

/// A bad per-clip `encryption` is rejected naming the clip, not silently applied or ignored.
#[tokio::test]
async fn a_bad_clip_level_encryption_names_the_clip() {
    let h = harness().await;
    *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
        r#"{{"asset_id":"movie","version":"v1","clips":[
            {{"location":{{"type":"file","path":"h264-aac.mp4"}},"to_ms":1500}},
            {{"location":{{"type":"file","path":"h264-aac.mp4"}},"from_ms":1500,
              "encryption":{{"scheme":"cbcs","keys":[{{"key_id":"{KEY_ID}","key":"{KEY}zz"}}]}}}}
        ]}}"#
    ));

    let (code, _, body) = fetch(&h.app, "/hls/movie/master.m3u8").await;

    assert_eq!(code, StatusCode::BAD_GATEWAY);
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains(KEY));
}

#[tokio::test]
async fn one_bad_clip_fails_the_whole_asset() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("does-not-exist.mp4", None, None),
            ],
        ),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the reason, naming clip 1, is in the asset_load_failed log line"
    );
}

#[tokio::test]
async fn a_window_past_the_end_of_its_file_fails_the_asset() {
    let h = harness().await;
    h.mapper.state.set(
        "movie",
        Answer::clips("v1", &[clip("h264-aac.mp4", Some(5000), None)]),
    );

    assert_eq!(
        status(&h.app, "/hls/movie/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn a_sequence_plays_its_clips_back_to_back() {
    let h = harness().await;
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("rendition-720p.mp4", None, None),
                clip("h264-aac.mp4", Some(1500), None),
            ],
        ),
    );

    let master = text(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);
    let version = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);

    assert_eq!(master.matches("#EXT-X-STREAM-INF").count(), 1, "{master}");
    assert!(master.contains("RESOLUTION=640x360"), "{master}");
    assert!(!master.contains("I-FRAME"), "{master}");
    for track in ["video", "audio-1"] {
        let playlist = text(
            &fetch(&h.app, &format!("/hls/seq/{track}/index.m3u8"))
                .await
                .2,
        );
        assert_eq!(playlist.matches("#EXTINF:").count(), 5, "3 + 2: {playlist}");
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY").count(),
            1,
            "{playlist}"
        );
        assert!(
            playlist.contains(&format!("#EXT-X-MAP:URI=\"clips/1/init.mp4?v={version}\"")),
            "{playlist}"
        );
    }
    let init = fetch(
        &h.app,
        &format!("/hls/seq/video/clips/1/init.mp4?v={version}"),
    )
    .await;
    assert_eq!(init.0, StatusCode::OK);
    assert_eq!(&init.2[4..8], b"ftyp");
    assert_eq!(
        status(
            &h.app,
            &format!("/dash/seq/audio-1/clips/0/init.mp4?v={version}")
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status(&h.app, &format!("/hls/seq/video/init.mp4?v={version}")).await,
        StatusCode::NOT_FOUND,
        "a sequence has no plain init segment"
    );
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/seq/video/clips/2/init.mp4?v={version}")
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let segment = fetch(
        &h.app,
        &format!("/hls/seq/video/segments/4/media.m4s?v={version}"),
    )
    .await;
    assert_eq!(segment.0, StatusCode::OK);
    assert_eq!(&segment.2[4..8], b"moof");
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/seq/video/segments/5/media.m4s?v={version}")
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let manifest = text(&fetch(&h.app, "/dash/seq/manifest.mpd").await.2);
    assert_eq!(manifest.matches("<Period ").count(), 2, "{manifest}");
    assert!(manifest.contains("startNumber=\"3\""), "{manifest}");
}

#[tokio::test]
async fn a_clip_init_segment_answers_conditional_and_range_requests() {
    let h = harness().await;
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("h264-aac.mp4", None, None),
            ],
        ),
    );
    let version = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);
    let uri = format!("/hls/seq/video/clips/1/init.mp4?v={version}");
    let (_, headers, whole) = fetch(&h.app, &uri).await;

    let conditional = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header(
                    axum::http::header::IF_NONE_MATCH,
                    headers[axum::http::header::ETAG].clone(),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ranged = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header(axum::http::header::RANGE, "bytes=0-7")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(ranged.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], &whole[..8]);
}

#[tokio::test]
async fn the_status_endpoint_counts_a_sequences_clips() {
    let h = harness().await;
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("h264-aac.mp4", None, None),
            ],
        ),
    );
    assert_eq!(status(&h.app, "/hls/seq/master.m3u8").await, StatusCode::OK);

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();

    assert_eq!(json["cache"]["assets"][0]["clips"], 2);
}

#[tokio::test]
async fn a_rotated_signature_reaches_every_clip_cut_from_that_file() {
    let (h, origin) = signed_harness(|_| {}).await;
    let answer = |signature: &str| {
        let url = signed(&origin, signature);
        short_ttl(Answer::clips(
            "v1",
            &[
                (http_location(&url), None, None),
                (http_location(&url), Some(1500), None),
            ],
        ))
    };
    h.mapper.state.set("seq", answer("1"));
    let version = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);

    // The mapper re-signs the same object and the old signature stops working.
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper.state.set("seq", answer("2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let before = origin.state.queries.lock().unwrap().len();

    // Segment 0 is the first clip's; segment 3 is the second's.
    for segment in [0, 3] {
        assert_eq!(
            status(
                &h.app,
                &format!("/hls/seq/video/segments/{segment}/media.m4s?v={version}")
            )
            .await,
            StatusCode::OK,
            "segment {segment}"
        );
    }

    let used = origin.state.queries.lock().unwrap()[before..].to_vec();
    assert!(
        !used.is_empty() && used.iter().all(|query| query == "sig=2"),
        "{used:?}"
    );
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"),
        "the asset must not reload"
    );
}

/// Two renditions of two clips each: the first second of each file, then the rest.
fn two_clip_ladder(version: &str) -> Answer {
    Answer::renditions_with_clips(
        version,
        &[
            (
                "720p",
                &[
                    clip("rendition-720p.mp4", None, Some(1000)),
                    clip("rendition-720p.mp4", Some(1000), None),
                ],
            ),
            (
                "480p",
                &[
                    clip("rendition-480p.mp4", None, Some(1000)),
                    clip("rendition-480p.mp4", Some(1000), None),
                ],
            ),
        ],
    )
}

#[tokio::test]
async fn an_adaptive_clips_answer_serves_hls_and_dash() {
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));

    let master = text(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    assert_eq!(master.matches("#EXT-X-STREAM-INF").count(), 2, "{master}");
    for track in ["video-720p", "video-480p", "audio-1"] {
        let playlist = text(
            &fetch(&h.app, &format!("/hls/grid/{track}/index.m3u8"))
                .await
                .2,
        );
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY").count(),
            1,
            "{track}: {playlist}"
        );
        assert_eq!(
            playlist.matches("#EXT-X-MAP").count(),
            2,
            "{track}: {playlist}"
        );
    }
    let manifest = text(&fetch(&h.app, "/dash/grid/manifest.mpd").await.2);
    assert_eq!(manifest.matches("<Period ").count(), 2, "{manifest}");
}

#[tokio::test]
async fn each_rendition_and_clip_has_its_own_init_segment() {
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    let found = |path: &str| {
        let url = format!("{path}?v={version}");
        let app = h.app.clone();
        async move { status(&app, &url).await }
    };

    assert_eq!(
        found("/hls/grid/video-720p/clips/1/init.mp4").await,
        StatusCode::OK
    );
    assert_eq!(
        found("/dash/grid/audio-1/clips/0/init.mp4").await,
        StatusCode::OK
    );
    for missing in [
        "/hls/grid/video-720p/init.mp4",
        "/hls/grid/video-720p/clips/9/init.mp4",
        "/hls/grid/video-nope/clips/0/init.mp4",
    ] {
        assert_eq!(found(missing).await, StatusCode::NOT_FOUND, "{missing}");
    }
}

#[tokio::test]
async fn segments_are_numbered_across_clips_in_every_rendition() {
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    let mut counts = Vec::new();
    for rendition in ["720p", "480p"] {
        let playlist = text(
            &fetch(&h.app, &format!("/hls/grid/video-{rendition}/index.m3u8"))
                .await
                .2,
        );
        let count = playlist.matches("#EXTINF:").count();
        counts.push(count);
        let segment = |number: usize| {
            format!("/hls/grid/video-{rendition}/segments/{number}/media.m4s?v={version}")
        };
        assert_eq!(
            status(&h.app, &segment(count - 1)).await,
            StatusCode::OK,
            "{rendition}"
        );
        assert_eq!(
            status(&h.app, &segment(count)).await,
            StatusCode::NOT_FOUND,
            "{rendition}"
        );
    }
    assert_eq!(
        counts[0], counts[1],
        "every rendition has the same segments"
    );
    assert!(counts[0] >= 2, "two clips make at least two segments");
}

#[tokio::test]
async fn a_rotated_signature_reaches_every_pair_cut_from_the_file() {
    let (h, origin) = signed_harness(|_| {}).await;
    let answer = |signature: &str| {
        let url = signed(&origin, signature);
        let clips = [
            (http_location(&url), None, Some(1500)),
            (http_location(&url), Some(1500), None),
        ];
        short_ttl(Answer::renditions_with_clips(
            "v1",
            &[("a", &clips), ("b", &clips)],
        ))
    };
    h.mapper.state.set("grid", answer("1"));
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);
    let playlist = text(&fetch(&h.app, "/hls/grid/video-a/index.m3u8").await.2);
    let last = playlist.matches("#EXTINF:").count() - 1;

    // The mapper re-signs the same object and the old signature stops working.
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper.state.set("grid", answer("2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let before = origin.state.queries.lock().unwrap().len();

    for rendition in ["a", "b"] {
        for segment in [0, last] {
            assert_eq!(
                status(
                    &h.app,
                    &format!(
                        "/hls/grid/video-{rendition}/segments/{segment}/media.m4s?v={version}"
                    )
                )
                .await,
                StatusCode::OK,
                "{rendition} segment {segment}"
            );
        }
    }

    let used = origin.state.queries.lock().unwrap()[before..].to_vec();
    assert!(
        !used.is_empty() && used.iter().all(|query| query == "sig=2"),
        "{used:?}"
    );
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 1"),
        "the asset must not reload"
    );
}

#[tokio::test]
async fn one_bad_file_fails_the_whole_asset_naming_rendition_and_clip() {
    let h = harness().await;
    h.mapper.state.set(
        "grid",
        Answer::renditions_with_clips(
            "v1",
            &[
                (
                    "720p",
                    &[
                        clip("rendition-720p.mp4", None, Some(1000)),
                        clip("does-not-exist.mp4", Some(1000), None),
                    ],
                ),
                (
                    "480p",
                    &[
                        clip("rendition-480p.mp4", None, Some(1000)),
                        clip("rendition-480p.mp4", Some(1000), None),
                    ],
                ),
            ],
        ),
    );

    assert_eq!(
        status(&h.app, "/hls/grid/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the reason, naming rendition `720p` clip 1, is in the asset_load_failed log line"
    );
}

#[tokio::test]
async fn the_status_endpoint_counts_renditions_and_clips() {
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));
    assert_eq!(
        status(&h.app, "/hls/grid/master.m3u8").await,
        StatusCode::OK
    );

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();

    assert_eq!(json["cache"]["assets"][0]["clips"], 2);
}

#[tokio::test]
async fn a_ladder_of_one_clip_each_serves_as_a_trimmed_ladder() {
    let h = harness().await;
    h.mapper.state.set(
        "grid",
        Answer::renditions_with_clips(
            "v1",
            &[
                ("720p", &[clip("rendition-720p.mp4", None, Some(2000))]),
                ("480p", &[clip("rendition-480p.mp4", None, Some(2000))]),
            ],
        ),
    );
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    let playlist = text(&fetch(&h.app, "/hls/grid/video-720p/index.m3u8").await.2);

    assert!(!playlist.contains("#EXT-X-DISCONTINUITY"), "{playlist}");
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/grid/video-720p/init.mp4?v={version}")
        )
        .await,
        StatusCode::OK,
        "a trimmed ladder keeps the plain init URL"
    );
}

#[tokio::test]
async fn max_renditions_applies_to_renditions_that_are_sequences() {
    let h = harness_with(|config| config.limits.max_renditions = 1).await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));

    assert_eq!(
        status(&h.app, "/hls/grid/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "two renditions are more than limits.max_renditions (1)"
    );
}

#[tokio::test]
async fn a_rotated_signature_reaches_a_trimmed_ladder() {
    let (h, origin) = signed_harness(|_| {}).await;
    let answer = |signature: &str| {
        let url = signed(&origin, signature);
        let clips = [(http_location(&url), None, Some(2000))];
        short_ttl(Answer::renditions_with_clips(
            "v1",
            &[("a", &clips), ("b", &clips)],
        ))
    };
    h.mapper.state.set("grid", answer("1"));
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    // The mapper re-signs the same object and the old signature stops working.
    *origin.state.required_query.lock().unwrap() = Some("sig=2".to_owned());
    h.mapper.state.set("grid", answer("2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let before = origin.state.queries.lock().unwrap().len();

    assert_eq!(
        status(
            &h.app,
            &format!("/hls/grid/video-a/segments/0/media.m4s?v={version}")
        )
        .await,
        StatusCode::OK
    );

    let used = origin.state.queries.lock().unwrap()[before..].to_vec();
    assert!(
        !used.is_empty() && used.iter().all(|query| query == "sig=2"),
        "the rotation must reach the asset before a read is rejected: {used:?}"
    );
}

#[tokio::test]
async fn changing_a_rendition_clips_window_under_the_same_version_reloads_the_asset() {
    let h = harness().await;
    let answer = |first_to_ms: u64| {
        let mut answer = Answer::renditions_with_clips(
            "v1",
            &[
                (
                    "720p",
                    &[
                        clip("rendition-720p.mp4", None, Some(first_to_ms)),
                        clip("rendition-720p.mp4", Some(2000), None),
                    ],
                ),
                (
                    "480p",
                    &[
                        clip("rendition-480p.mp4", None, Some(first_to_ms)),
                        clip("rendition-480p.mp4", Some(2000), None),
                    ],
                ),
            ],
        );
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    h.mapper.state.set("grid", answer(1000));
    let before = text(&fetch(&h.app, "/hls/grid/video-720p/index.m3u8").await.2);

    h.mapper.state.set("grid", answer(2000));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after = text(&fetch(&h.app, "/hls/grid/video-720p/index.m3u8").await.2);

    assert_ne!(before, after, "the new window must be served");
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 2"),
        "reloaded"
    );
}

#[test]
fn a_file_named_by_several_pairs_gets_one_entry() {
    use crate::registry::distinct_locations;
    use crate::resolver::{
        AssetLocation, ClipLocation, LocationKey, RenditionLocation, RenditionSource,
    };

    let location = |path: &str| AssetLocation::File(std::path::PathBuf::from(path));
    let clip_at = |path: &str| ClipLocation {
        location: location(path),
        window: crate::clip::ClipWindow {
            from_ms: 0,
            to_ms: None,
        },
        encryption: None,
        tracks: None,
    };
    let rendition = |id: &str, paths: &[&str]| RenditionLocation {
        id: id.to_owned(),
        source: RenditionSource::Clips(paths.iter().map(|path| clip_at(path)).collect()),
        tracks: None,
        audio: None,
    };
    let renditions = [
        rendition("720p", &["ad.mp4", "movie-720.mp4"]),
        rendition("480p", &["ad.mp4", "movie-480.mp4"]),
    ];

    let (files, indices) = distinct_locations(&renditions);

    assert_eq!(files.len(), 3, "the shared ad is one entry");
    assert_eq!(files[0].0, LocationKey::RenditionClip("720p".to_owned(), 0));
    assert_eq!(indices, vec![vec![0, 1], vec![0, 2]]);
}

/// Reassembles each clip from its init segment and media segments under `base`, and returns how
/// many video frames `ffmpeg` decodes from each, failing on any decode error.
async fn decode_each(
    app: &Router,
    base: &str,
    clips: &[(String, Vec<String>)],
    directory: &std::path::Path,
    key: Option<&str>,
) -> Vec<u64> {
    std::fs::create_dir_all(directory).unwrap();
    let mut counts = Vec::new();
    for (position, (init, segments)) in clips.iter().enumerate() {
        let mut bytes = fetch(app, &format!("{base}{init}")).await.2.to_vec();
        for segment in segments {
            bytes.extend_from_slice(&fetch(app, &format!("{base}{segment}")).await.2);
        }
        let path = directory.join(format!("clip-{position}.mp4"));
        std::fs::write(&path, bytes).unwrap();
        let mut probe = std::process::Command::new("ffprobe");
        probe.args(["-v", "error", "-count_frames", "-select_streams", "v:0"]);
        probe.args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]);
        let mut ffmpeg = std::process::Command::new("ffmpeg");
        ffmpeg.args(["-v", "error"]);
        if let Some(key) = key {
            probe.args(["-decryption_key", key]);
            ffmpeg.args(["-decryption_key", key]);
        }
        let count = probe.arg("-i").arg(&path).output().unwrap();
        let decode = ffmpeg
            .arg("-i")
            .arg(&path)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            decode.stderr.is_empty(),
            "clip {position}: {}",
            String::from_utf8_lossy(&decode.stderr)
        );
        counts.push(
            String::from_utf8_lossy(&count.stdout)
                .trim()
                .parse()
                .unwrap(),
        );
    }
    counts
}

/// `ffmpeg` cannot judge a mixed sequence as one stream: its HLS demuxer keeps the first init
/// segment's decoder settings across an `EXT-X-MAP` change, and its DASH demuxer plays a single
/// Period (TDD 0008, "Testing"). So each clip is followed from the served playlist and manifest to
/// its own init and media segments, reassembled, and decoded on its own.
#[tokio::test]
async fn every_clip_of_a_sequence_decodes_from_the_served_hls_and_dash() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("rendition-720p.mp4", None, None),
                clip("h264-aac.mp4", Some(1500), None),
            ],
        ),
    );
    let version = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/sequence-decode");

    // HLS: each EXT-X-MAP names a clip's init segment; the URIs after it are that clip's.
    let playlist = text(&fetch(&h.app, "/hls/seq/video/index.m3u8").await.2);
    let mut clips: Vec<(String, Vec<String>)> = Vec::new();
    for line in playlist.lines() {
        if let Some(uri) = line
            .strip_prefix("#EXT-X-MAP:URI=\"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            clips.push((uri.to_owned(), Vec::new()));
        } else if !line.is_empty() && !line.starts_with('#') {
            clips
                .last_mut()
                .expect("a map before any segment")
                .1
                .push(line.to_owned());
        }
    }
    let frames = decode_each(
        &h.app,
        "/hls/seq/video/",
        &clips,
        &directory.join("hls"),
        None,
    )
    .await;
    assert_eq!(
        frames,
        [90, 90, 60],
        "whole, whole, and from the keyframe at 1 s"
    );

    // DASH: each Period's video template names its clip's init segment, first number, and
    // offset; its timeline says how many segments follow.
    let manifest = text(&fetch(&h.app, "/dash/seq/manifest.mpd").await.2);
    let mut clips = Vec::new();
    for (position, period) in manifest.split("<Period ").skip(1).enumerate() {
        let video = period
            .split("contentType=\"video\"")
            .nth(1)
            .expect("a video adaptation set");
        let attribute = |name: &str| -> String {
            video
                .split(&format!("{name}=\""))
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("the attribute is present")
                .to_owned()
        };
        let first: u32 = attribute("startNumber").parse().unwrap();
        let offset: u64 = attribute("presentationTimeOffset").parse().unwrap();
        let count = u32::try_from(
            video
                .split("</SegmentTimeline>")
                .next()
                .unwrap()
                .matches("<S ")
                .count(),
        )
        .unwrap();
        let segments = (first..first + count)
            .map(|number| format!("segments/{number}/media.m4s?v={version}"))
            .collect::<Vec<_>>();
        // Each Period starts where its media does: the first fragment's decode time is the offset.
        let fragment = fetch(&h.app, &format!("/dash/seq/video/{}", segments[0]))
            .await
            .2;
        assert_eq!(tfdt(&fragment), offset, "period {position}");
        clips.push((format!("clips/{position}/init.mp4?v={version}"), segments));
    }
    let frames = decode_each(
        &h.app,
        "/dash/seq/video/",
        &clips,
        &directory.join("dash"),
        None,
    )
    .await;
    assert_eq!(frames, [90, 90, 60]);
}

/// The clips of an HLS media playlist: each `EXT-X-MAP` names a clip's init segment, and the
/// segment URIs after it are that clip's.
fn playlist_clips(playlist: &str) -> Vec<(String, Vec<String>)> {
    let mut clips: Vec<(String, Vec<String>)> = Vec::new();
    for line in playlist.lines() {
        if let Some(uri) = line
            .strip_prefix("#EXT-X-MAP:URI=\"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            clips.push((uri.to_owned(), Vec::new()));
        } else if !line.is_empty() && !line.starts_with('#') {
            clips
                .last_mut()
                .expect("a map before any segment")
                .1
                .push(line.to_owned());
        }
    }
    clips
}

/// One video adaptation set of one DASH period, as an adaptive sequence's manifest lists it.
struct DashVideo {
    id: String,
    timescale: u64,
    start_number: u32,
    offset: u64,
    /// The summed duration of the period's segments, in ticks.
    ticks: u64,
}

/// Each period's start in milliseconds and its video adaptation sets.
fn dash_periods(manifest: &str) -> Vec<(u64, Vec<DashVideo>)> {
    manifest
        .split("<Period ")
        .skip(1)
        .map(|period| {
            let seconds = period
                .split("start=\"PT")
                .nth(1)
                .and_then(|rest| rest.split("S\"").next())
                .expect("a period start");
            let (whole, millis) = seconds.split_once('.').expect("seconds with milliseconds");
            let start = whole.parse::<u64>().unwrap() * 1000 + millis.parse::<u64>().unwrap();
            let sets = period
                .split("contentType=\"video\"")
                .skip(1)
                .map(|set| {
                    let attribute = |name: &str| -> String {
                        set.split(&format!("{name}=\""))
                            .nth(1)
                            .and_then(|rest| rest.split('"').next())
                            .expect("the attribute is present")
                            .to_owned()
                    };
                    let timeline = set.split("</SegmentTimeline>").next().unwrap();
                    DashVideo {
                        id: attribute("Representation id"),
                        timescale: attribute("timescale").parse().unwrap(),
                        start_number: attribute("startNumber").parse().unwrap(),
                        offset: attribute("presentationTimeOffset").parse().unwrap(),
                        ticks: timeline
                            .split(" d=\"")
                            .skip(1)
                            .map(|rest| rest.split('"').next().unwrap().parse::<u64>().unwrap())
                            .sum(),
                    }
                })
                .collect();
            (start, sets)
        })
        .collect()
}

/// `ffmpeg` cannot judge a sequence as one stream (see the test above), so each rendition is
/// followed clip by clip, and what is decoded from each clip must match the same cuts of that
/// rendition's file served as a plain sequence.
#[tokio::test]
async fn every_rendition_of_an_adaptive_sequence_decodes_clip_by_clip() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));
    for (name, file) in [
        ("720p", "rendition-720p.mp4"),
        ("480p", "rendition-480p.mp4"),
    ] {
        h.mapper.state.set(
            &format!("plain-{name}"),
            Answer::clips(
                "v1",
                &[clip(file, None, Some(1000)), clip(file, Some(1000), None)],
            ),
        );
    }
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/adaptive-decode");
    let decode = |asset: &str, track: &str| {
        let (app, asset, track) = (h.app.clone(), asset.to_owned(), track.to_owned());
        let directory = directory.join(format!("{asset}-{track}"));
        async move {
            let playlist = text(
                &fetch(&app, &format!("/hls/{asset}/{track}/index.m3u8"))
                    .await
                    .2,
            );
            decode_each(
                &app,
                &format!("/hls/{asset}/{track}/"),
                &playlist_clips(&playlist),
                &directory,
                None,
            )
            .await
        }
    };

    for rendition in ["720p", "480p"] {
        let reference = decode(&format!("plain-{rendition}"), "video").await;
        assert_eq!(reference.len(), 2);
        assert!(reference.iter().all(|&frames| frames > 0), "{reference:?}");
        let frames = decode("grid", &format!("video-{rendition}")).await;
        assert_eq!(
            frames, reference,
            "{rendition} decodes with the frame counts of the same cuts served as a plain sequence"
        );
    }
}

/// What makes switching quality safe at a clip boundary: every rendition's period starts at the
/// same time, its first fragment starts where the period says it does, and the period starts
/// where the one before ended. This also settles TDD 0016's question about the shared timeline.
#[tokio::test]
async fn every_rendition_starts_each_period_at_the_same_time_and_continues_the_last() {
    let h = harness().await;
    h.mapper.state.set("grid", two_clip_ladder("v1"));
    let version = version_in(&fetch(&h.app, "/hls/grid/master.m3u8").await.2);

    let periods = dash_periods(&text(&fetch(&h.app, "/dash/grid/manifest.mpd").await.2));

    assert_eq!(periods.len(), 2);
    for (position, (start_ms, sets)) in periods.iter().enumerate() {
        assert_eq!(sets.len(), 2, "period {position}");
        for set in sets {
            let offset_ms = set.offset * 1000 / set.timescale;
            assert!(
                offset_ms.abs_diff(*start_ms) <= 1,
                "{} period {position}: offset {offset_ms} ms, period starts {start_ms} ms",
                set.id
            );
            let fragment = fetch(
                &h.app,
                &format!(
                    "/dash/grid/{}/segments/{}/media.m4s?v={version}",
                    set.id, set.start_number
                ),
            )
            .await
            .2;
            assert_eq!(tfdt(&fragment), set.offset, "{} period {position}", set.id);
        }
    }
    for set in &periods[0].1 {
        let ends_ms = set.ticks * 1000 / set.timescale;
        assert!(
            ends_ms.abs_diff(periods[1].0) <= 40,
            "{}: the first period ends at {ends_ms} ms, the second starts at {} ms",
            set.id,
            periods[1].0
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The optional asset listing (`GET /v1/assets`)
// ---------------------------------------------------------------------------------------------

async fn known_assets(app: &Router) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_slice(&fetch(app, "/admin/status").await.2).unwrap();
    json["resolver"]["known_assets"].clone()
}

#[tokio::test]
async fn admin_status_lists_what_the_mapper_lists_sorted_and_checked() {
    let h = harness().await;
    *h.mapper.state.listing.lock().unwrap() =
        Some(r#"{"assets":["preroll","movie","movie","../etc","","has space"]}"#.to_owned());

    assert_eq!(
        known_assets(&h.app).await,
        serde_json::json!(["movie", "preroll"]),
        "sorted, once each, and only IDs a request could name"
    );
}

#[tokio::test]
async fn a_malformed_listing_is_no_listing() {
    let h = harness().await;
    *h.mapper.state.listing.lock().unwrap() = Some(r#"{"assets":"movie"}"#.to_owned());

    assert_eq!(known_assets(&h.app).await, serde_json::Value::Null);
}

#[tokio::test]
async fn a_listing_is_capped_at_max_assets() {
    let h = harness_with(|config| config.limits.max_assets = 2).await;
    *h.mapper.state.listing.lock().unwrap() = Some(r#"{"assets":["c","b","a"]}"#.to_owned());

    assert_eq!(known_assets(&h.app).await, serde_json::json!(["a", "b"]));
}

// ---------------------------------------------------------------------------------------------
// Encryption (TDD 0009)
// ---------------------------------------------------------------------------------------------

const KEY_ID: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "00112233445566778899aabbccddeeff";

fn clear_key_encryption(key: &str) -> serde_json::Value {
    serde_json::json!({
        "scheme": "cbcs",
        "keys": [{ "key_id": KEY_ID, "key": key }],
        "systems": [{ "system_id": "e2719d58-a985-b3c9-781a-b030af78d30e", "license_url": "https://l.example.net/ck" }],
    })
}

#[tokio::test]
async fn an_encrypted_asset_serves_protected_init_and_fragments() {
    let h = harness().await;
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    let version = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);

    let init = fetch(&h.app, &format!("/hls/drm/video/init.mp4?v={version}"))
        .await
        .2;
    let segment = fetch(
        &h.app,
        &format!("/hls/drm/video/segments/1/media.m4s?v={version}"),
    )
    .await;
    let audio = fetch(&h.app, &format!("/dash/drm/audio-1/init.mp4?v={version}"))
        .await
        .2;

    assert!(init.windows(4).any(|w| w == b"encv") && init.windows(4).any(|w| w == b"tenc"));
    assert!(audio.windows(4).any(|w| w == b"enca"));
    assert_eq!(segment.0, StatusCode::OK);
    assert!(segment.2.windows(4).any(|w| w == b"senc"));
    let playlist = text(&fetch(&h.app, "/hls/drm/video/index.m3u8").await.2);
    assert!(
        playlist.contains(r#"KEYFORMAT="org.w3.clearkey""#),
        "{playlist}"
    );
    assert!(metric(&h.state, "vod_encrypted_segments_total 1"));
}

#[tokio::test]
async fn range_and_head_requests_work_on_an_encrypted_segment() {
    let h = harness().await;
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    let version = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);
    let uri = format!("/hls/drm/video/segments/0/media.m4s?v={version}");
    let whole = fetch(&h.app, &uri).await.2;

    let ranged = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header(axum::http::header::RANGE, "bytes=8-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let head = h
        .app
        .clone()
        .oneshot(Request::head(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &to_bytes(ranged.into_body(), usize::MAX).await.unwrap()[..],
        &whole[8..16]
    );
    assert_eq!(
        head.headers()[axum::http::header::CONTENT_LENGTH],
        whole.len().to_string().as_str()
    );
    assert_eq!(
        fetch(&h.app, &uri).await.2,
        whole,
        "deterministic: the same bytes again"
    );
}

/// An encrypted segment is streamed in `stream_chunk_bytes` pieces like a clear one, so the idle
/// timeout applies to it, and its job slot is back once the response finishes.
#[tokio::test]
async fn an_encrypted_segment_streams_in_chunks_and_frees_its_job_slot() {
    use tokio_stream::StreamExt as _;
    let h = harness_with(|config| {
        config.limits.stream_chunk_bytes = 1024;
        config.limits.max_segment_jobs = 2;
    })
    .await;
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    let version = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);
    let uri = format!("/hls/drm/video/segments/0/media.m4s?v={version}");

    let response = h
        .app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let (mut chunks, mut total) = (0usize, 0usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        assert!(chunk.len() <= 1024, "a chunk of {} bytes", chunk.len());
        chunks += 1;
        total += chunk.len();
    }

    assert!(total > 1024, "the fixture segment is larger than one chunk");
    assert!(chunks > 1, "{chunks} chunk(s) for {total} bytes");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(h.state.segment_jobs.available_permits(), 2);
}

/// A client that stops reading an encrypted segment is cut off after the idle timeout, and the
/// segment's job slot is released with it.
#[tokio::test]
async fn a_stalled_encrypted_response_is_dropped_and_frees_its_job_slot() {
    let h = harness_with(|config| {
        config.limits.stream_chunk_bytes = 64;
        config.limits.response_idle_timeout_ms = 50;
        config.limits.max_segment_jobs = 1;
    })
    .await;
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    let version = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);
    let uri = format!("/hls/drm/video/segments/0/media.m4s?v={version}");

    let stalled = h
        .app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stalled.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(metric(&h.state, "vod_segment_stream_aborts_idle_total 1"));
    assert_eq!(h.state.segment_jobs.available_permits(), 1);
    assert_eq!(status(&h.app, &uri).await, StatusCode::OK);
    drop(stalled);
}

#[tokio::test]
async fn an_unsupported_codec_with_encryption_fails_the_asset() {
    let h = harness().await;
    // VP9 and Opus have no encryption rules here yet (HEVC does: see the HEVC test below).
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "vp9-opus.mp4").with_encryption(clear_key_encryption(KEY)),
    );

    assert_eq!(
        status(&h.app, "/hls/drm/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn the_key_changes_the_url_version() {
    let h = harness().await;
    h.mapper.state.set(
        "a",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    h.mapper.state.set(
        "b",
        Answer::file("v1", "h264-aac.mp4")
            .with_encryption(clear_key_encryption("ffeeddccbbaa99887766554433221100")),
    );
    h.mapper.state.set("c", Answer::file("v1", "h264-aac.mp4"));

    let version = |id: &'static str| {
        let app = h.app.clone();
        async move { version_in(&fetch(&app, &format!("/hls/{id}/master.m3u8")).await.2) }
    };
    let (a, b, c) = (version("a").await, version("b").await, version("c").await);
    assert!(a != b && a != c && b != c);
}

#[tokio::test]
async fn rekeying_under_the_same_version_reloads_with_the_new_key() {
    let h = harness().await;
    let answer = |key: &str| {
        let mut answer =
            Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(key));
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    h.mapper.state.set("drm", answer(KEY));
    let before = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);

    h.mapper
        .state
        .set("drm", answer("ffeeddccbbaa99887766554433221100"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after = version_in(&fetch(&h.app, "/hls/drm/master.m3u8").await.2);

    assert_ne!(before, after, "new key, new URLs");
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 2"),
        "reloaded with the new key"
    );
}

#[tokio::test]
async fn a_rejected_encryption_answer_never_echoes_the_key() {
    let h = harness().await;
    *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
        r#"{{"asset_id":"drm","version":"v1","location":{{"type":"file","path":"h264-aac.mp4"}},"encryption":{{"scheme":"cbcs","keys":[{{"key_id":"{KEY_ID}","key":"{KEY}zz"}}]}}}}"#
    ));

    let (code, _, body) = fetch(&h.app, "/hls/drm/master.m3u8").await;

    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(!String::from_utf8_lossy(&body).contains(KEY));
}

#[tokio::test]
async fn a_mistyped_encryption_answer_is_a_bad_gateway() {
    let h = harness().await;
    *h.mapper.state.raw_body.lock().unwrap() = Some(format!(
        r#"{{"asset_id":"drm","version":"v1","location":{{"type":"file","path":"h264-aac.mp4"}},"encryption":{{"scheme":"cbcs","keys":"{KEY}"}}}}"#
    ));

    let (code, _, body) = fetch(&h.app, "/hls/drm/master.m3u8").await;

    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(!String::from_utf8_lossy(&body).contains(KEY));
}

#[tokio::test]
async fn admin_status_reports_key_ids_never_keys() {
    let h = harness().await;
    h.mapper.state.set(
        "drm",
        Answer::file("v1", "h264-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    assert_eq!(status(&h.app, "/hls/drm/master.m3u8").await, StatusCode::OK);

    let body = fetch(&h.app, "/admin/status").await.2;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["cache"]["assets"][0]["encrypted"], true);
    assert_eq!(
        json["cache"]["assets"][0]["key_ids"][0],
        "01234567-89ab-cdef-0123-456789abcdef"
    );
    assert!(!String::from_utf8_lossy(&body).contains(KEY));
}

// ---------------------------------------------------------------------------------------------
// Different keys per clip (TDD 0009, "Deferred" -> implemented)
// ---------------------------------------------------------------------------------------------

const KEY_ID_B: &str = "fedcba9876543210fedcba9876543210";
const KEY_B: &str = "ffeeddccbbaa99887766554433221100";

fn clear_key_encryption_b() -> serde_json::Value {
    serde_json::json!({
        "scheme": "cbcs",
        "keys": [{ "key_id": KEY_ID_B, "key": KEY_B }],
        "systems": [{ "system_id": "e2719d58-a985-b3c9-781a-b030af78d30e", "license_url": "https://l.example.net/ck-b" }],
    })
}

/// A clear pre-roll clip, then an encrypted one, exactly TDD 0009's "Different keys per clip"
/// motivating example. No asset-level `encryption` is set; clip 1 carries its own.
#[tokio::test]
async fn a_clear_clip_can_precede_an_encrypted_one() {
    let h = harness().await;
    h.mapper.state.set(
        "mixed",
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted("h264-aac.mp4", None, Some(1500), None),
                clip_encrypted(
                    "h264-aac.mp4",
                    Some(1500),
                    None,
                    Some(clear_key_encryption(KEY)),
                ),
            ],
        ),
    );
    let version = version_in(&fetch(&h.app, "/hls/mixed/master.m3u8").await.2);

    let playlist = text(&fetch(&h.app, "/hls/mixed/video/index.m3u8").await.2);
    let map0 = playlist
        .find("clips/0/init.mp4")
        .expect("clip 0's EXT-X-MAP");
    let map1 = playlist
        .find("clips/1/init.mp4")
        .expect("clip 1's EXT-X-MAP");
    assert!(
        playlist[..map0].find("#EXT-X-KEY:").is_none(),
        "clip 0 is clear, so no key precedes it: {playlist}"
    );
    let key1 = playlist[map0..map1]
        .find("#EXT-X-KEY:")
        .expect("a key line between clip 0's MAP and clip 1's");
    assert!(
        playlist[map0..map0 + key1].contains("#EXT-X-DISCONTINUITY"),
        "the key line follows the discontinuity: {playlist}"
    );
    assert!(
        playlist[map0..map1].contains(r#"KEYFORMAT="org.w3.clearkey""#),
        "{playlist}"
    );

    // Clip 0's segment is untouched; clip 1's is protected.
    let clear_segment = fetch(
        &h.app,
        &format!("/hls/mixed/video/segments/0/media.m4s?v={version}"),
    )
    .await
    .2;
    let encrypted_segment = fetch(
        &h.app,
        &format!("/hls/mixed/video/segments/2/media.m4s?v={version}"),
    )
    .await
    .2;
    assert!(!clear_segment.windows(4).any(|w| w == b"senc"));
    assert!(encrypted_segment.windows(4).any(|w| w == b"senc"));

    // DASH: only clip 1's Period is protected, but the namespaces cover the whole MPD.
    let manifest = text(&fetch(&h.app, "/dash/mixed/manifest.mpd").await.2);
    assert!(manifest.contains("xmlns:cenc="), "{manifest}");
    let period0 = &manifest[manifest.find("<Period id=\"clip-0\"").unwrap()
        ..manifest.find("<Period id=\"clip-1\"").unwrap()];
    let period1 = &manifest[manifest.find("<Period id=\"clip-1\"").unwrap()..];
    assert!(!period0.contains("ContentProtection"), "{period0}");
    assert!(period1.contains("ContentProtection"), "{period1}");

    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();
    assert_eq!(json["cache"]["assets"][0]["encrypted"], true);
    assert_eq!(
        json["cache"]["assets"][0]["key_ids"][0], "01234567-89ab-cdef-0123-456789abcdef",
        "status reports clip 1's key, the only one in use: {json}"
    );
}

/// The reverse transition: an encrypted clip followed by a clear one needs an explicit
/// `METHOD=NONE` line, or a player would keep decrypting with the first clip's key (RFC 8216).
#[tokio::test]
async fn an_encrypted_clip_followed_by_a_clear_one_turns_the_key_off() {
    let h = harness().await;
    h.mapper.state.set(
        "mixed",
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted(
                    "h264-aac.mp4",
                    None,
                    Some(1500),
                    Some(clear_key_encryption(KEY)),
                ),
                clip_encrypted("h264-aac.mp4", Some(1500), None, None),
            ],
        ),
    );

    let playlist = text(&fetch(&h.app, "/hls/mixed/video/index.m3u8").await.2);

    let map0 = playlist.find("clips/0/init.mp4").unwrap();
    let map1 = playlist.find("clips/1/init.mp4").unwrap();
    assert!(
        playlist[..map0].contains("#EXT-X-KEY:METHOD=SAMPLE-AES"),
        "clip 0 starts encrypted: {playlist}"
    );
    assert!(
        playlist[map0..map1].contains("#EXT-X-KEY:METHOD=NONE\n"),
        "clip 1 turns the key off: {playlist}"
    );
}

/// Two clips, each its own key: the master playlist's `EXT-X-SESSION-KEY` lines cover both, and
/// `/admin/status` lists both key IDs.
#[tokio::test]
async fn a_sequence_can_use_a_different_key_for_each_clip() {
    let h = harness().await;
    h.mapper.state.set(
        "mixed",
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted(
                    "h264-aac.mp4",
                    None,
                    Some(1500),
                    Some(clear_key_encryption(KEY)),
                ),
                clip_encrypted(
                    "h264-aac.mp4",
                    Some(1500),
                    None,
                    Some(clear_key_encryption_b()),
                ),
            ],
        ),
    );

    let master = text(&fetch(&h.app, "/hls/mixed/master.m3u8").await.2);
    assert_eq!(
        master.matches("#EXT-X-SESSION-KEY:").count(),
        2,
        "one session key per distinct clip key: {master}"
    );
    assert!(master.contains("https://l.example.net/ck\""), "{master}");
    assert!(master.contains("https://l.example.net/ck-b\""), "{master}");

    let media = text(&fetch(&h.app, "/hls/mixed/video/index.m3u8").await.2);
    let map0 = media.find("clips/0/init.mp4").unwrap();
    let map1 = media.find("clips/1/init.mp4").unwrap();
    assert!(
        media[..map0].contains("https://l.example.net/ck\""),
        "{media}"
    );
    assert!(
        media[map0..map1].contains("https://l.example.net/ck-b\""),
        "clip 1's own key line replaces clip 0's: {media}"
    );

    assert_eq!(
        status(&h.app, "/hls/mixed/master.m3u8").await,
        StatusCode::OK
    );
    let json: serde_json::Value =
        serde_json::from_slice(&fetch(&h.app, "/admin/status").await.2).unwrap();
    let key_ids = json["cache"]["assets"][0]["key_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(key_ids.len(), 2, "{key_ids:?}");
    assert!(key_ids.contains(&"01234567-89ab-cdef-0123-456789abcdef"));
    assert!(key_ids.contains(&"fedcba98-7654-3210-fedc-ba9876543210"));
}

/// Changing only the second clip's key changes the whole sequence's version (`clip::version_of`
/// hashes each clip's own encryption), and the mapper is never asked to echo a key back.
#[tokio::test]
async fn rekeying_one_clip_changes_the_sequence_version() {
    let h = harness().await;
    let make = |key: &str| {
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted("h264-aac.mp4", None, Some(1500), None),
                clip_encrypted(
                    "h264-aac.mp4",
                    Some(1500),
                    None,
                    Some(clear_key_encryption(key)),
                ),
            ],
        )
    };
    h.mapper.state.set("a", make(KEY));
    h.mapper.state.set("b", make(KEY_B));

    let version_a = version_in(&fetch(&h.app, "/hls/a/master.m3u8").await.2);
    let version_b = version_in(&fetch(&h.app, "/hls/b/master.m3u8").await.2);

    assert_ne!(version_a, version_b);
}

/// Independent decryption of a mixed sequence: the clear clip plays with no key, the encrypted
/// one only with its own, and a differently keyed clip is refused both the wrong key and the
/// other clip's key (TDD 0009, "Testing").
#[tokio::test]
async fn ffmpeg_decrypts_a_mixed_sequence_clip_by_clip() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set(
        "mixed",
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted("h264-aac.mp4", None, Some(1500), None),
                clip_encrypted(
                    "h264-aac.mp4",
                    Some(1500),
                    None,
                    Some(clear_key_encryption(KEY)),
                ),
                clip_encrypted(
                    "h264-aac.mp4",
                    None,
                    Some(1500),
                    Some(clear_key_encryption_b()),
                ),
            ],
        ),
    );
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mixed-key-decode");
    std::fs::create_dir_all(&directory).unwrap();

    // Each clip's init and media URIs, as the playlist actually names them: segment numbers run
    // across the whole sequence, not per clip (see `every_clip_of_a_sequence_decodes_...`).
    let playlist = text(&fetch(&h.app, "/hls/mixed/video/index.m3u8").await.2);
    let mut clips: Vec<(String, Vec<String>)> = Vec::new();
    for line in playlist.lines() {
        if let Some(uri) = line
            .strip_prefix("#EXT-X-MAP:URI=\"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            clips.push((uri.to_owned(), Vec::new()));
        } else if !line.is_empty() && !line.starts_with('#') {
            clips
                .last_mut()
                .expect("a map before any segment")
                .1
                .push(line.to_owned());
        }
    }
    assert_eq!(clips.len(), 3);

    let keys: [Option<&str>; 3] = [None, Some(KEY), Some(KEY_B)];
    for (position, ((init, segments), key)) in clips.iter().zip(keys).enumerate() {
        let mut bytes = fetch(&h.app, &format!("/hls/mixed/video/{init}"))
            .await
            .2
            .to_vec();
        for segment in segments {
            bytes.extend_from_slice(
                &fetch(&h.app, &format!("/hls/mixed/video/{segment}"))
                    .await
                    .2,
            );
        }
        let path = directory.join(format!("clip-{position}.mp4"));
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            decode_errors(&path, key).is_empty(),
            "clip {position} with its own key"
        );
        if let Some(wrong) = keys.iter().find(|candidate| **candidate != key).copied() {
            let errors = decode_errors(&path, wrong);
            assert!(
                key.is_none() || !errors.is_empty(),
                "clip {position} must not decode cleanly under a different clip's key"
            );
        }
    }
}

/// `FFmpeg`'s error output decoding `path`, optionally with a decryption key.
fn decode_errors(path: &std::path::Path, key: Option<&str>) -> String {
    let mut command = std::process::Command::new("ffmpeg");
    command.args(["-v", "error"]);
    if let Some(key) = key {
        command.args(["-decryption_key", key]);
    }
    let output = command
        .arg("-i")
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// An independent implementation (`FFmpeg`'s `cbcs` decryptor) must recover every frame with the
/// right key, and must not with a wrong one (TDD 0009, "Testing").
#[tokio::test]
async fn ffmpeg_decrypts_every_encrypted_track_we_serve() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    let encryption = clear_key_encryption(KEY);
    h.mapper.state.set(
        "one",
        Answer::file("v1", "h264-aac.mp4").with_encryption(encryption.clone()),
    );
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("rendition-720p.mp4", None, None),
                clip("h264-aac.mp4", Some(1500), None),
            ],
        )
        .with_encryption(encryption),
    );
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/drm-decode");

    // A single file: video and audio, each reassembled from its init and segments.
    let version = version_in(&fetch(&h.app, "/hls/one/master.m3u8").await.2);
    for (track, expected) in [("video", 90u64)] {
        let segments = (0..3)
            .map(|n| format!("segments/{n}/media.m4s?v={version}"))
            .collect();
        let clips = vec![(format!("init.mp4?v={version}"), segments)];
        let frames = decode_each(
            &h.app,
            &format!("/hls/one/{track}/"),
            &clips,
            &directory.join("one"),
            Some(KEY),
        )
        .await;
        assert_eq!(frames, [expected]);
    }
    let audio_path = {
        let mut bytes = fetch(&h.app, &format!("/hls/one/audio-1/init.mp4?v={version}"))
            .await
            .2
            .to_vec();
        for n in 0..3 {
            bytes.extend_from_slice(
                &fetch(
                    &h.app,
                    &format!("/hls/one/audio-1/segments/{n}/media.m4s?v={version}"),
                )
                .await
                .2,
            );
        }
        let path = directory.join("one-audio.mp4");
        std::fs::write(&path, bytes).unwrap();
        path
    };
    assert_eq!(
        decode_errors(&audio_path, Some(KEY)),
        "",
        "audio decrypts cleanly"
    );
    assert_ne!(
        decode_errors(&audio_path, Some("ffeeddccbbaa99887766554433221100")),
        "",
        "a wrong key does not"
    );

    // A sequence: every clip's video, from the served playlist, as in the clips decode test.
    let playlist = text(&fetch(&h.app, "/hls/seq/video/index.m3u8").await.2);
    let mut clips: Vec<(String, Vec<String>)> = Vec::new();
    for line in playlist.lines() {
        if let Some(uri) = line
            .strip_prefix("#EXT-X-MAP:URI=\"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            clips.push((uri.to_owned(), Vec::new()));
        } else if !line.is_empty() && !line.starts_with('#') {
            clips.last_mut().unwrap().1.push(line.to_owned());
        }
    }
    let frames = decode_each(
        &h.app,
        "/hls/seq/video/",
        &clips,
        &directory.join("seq"),
        Some(KEY),
    )
    .await;
    assert_eq!(frames, [90, 90, 60]);

    // Without the key, the video does not decode cleanly: it really is encrypted.
    let video = directory.join("one").join("clip-0.mp4");
    assert_ne!(
        decode_errors(&video, None),
        "",
        "clear decoding of encrypted video fails"
    );
}

/// `FFmpeg`'s count of the video frames in `path`.
fn video_frames(path: &std::path::Path) -> u64 {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}

/// HEVC is encrypted in the `cbcs` pattern like H.264 (TDD 0009): `FFmpeg`'s independent
/// decryptor recovers every frame with the right key, and cannot decode it with the wrong key or
/// none.
#[tokio::test]
async fn ffmpeg_decrypts_encrypted_hevc() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set(
        "hevc",
        Answer::file("v1", "hevc-aac.mp4").with_encryption(clear_key_encryption(KEY)),
    );
    let master = fetch(&h.app, "/hls/hevc/master.m3u8").await;
    assert_eq!(master.0, StatusCode::OK, "{}", text(&master.2));
    let version = version_in(&master.2);
    let playlist = text(&fetch(&h.app, "/hls/hevc/video/index.m3u8").await.2);
    let segments = playlist
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!segments.is_empty(), "{playlist}");
    let clips = vec![(format!("init.mp4?v={version}"), segments)];
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/drm-hevc");

    let frames = decode_each(&h.app, "/hls/hevc/video/", &clips, &directory, Some(KEY)).await;

    let source =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hevc-aac.mp4");
    assert_eq!(frames, [video_frames(&source)]);
    let encrypted = directory.join("clip-0.mp4");
    assert_eq!(decode_errors(&encrypted, Some(KEY)), "", "right key");
    assert_ne!(decode_errors(&encrypted, None), "", "no key");
    assert_ne!(
        decode_errors(&encrypted, Some("ffeeddccbbaa99887766554433221100")),
        "",
        "wrong key"
    );
    // The init segment declares what a player needs to find the key.
    let init = fetch(&h.app, &format!("/hls/hevc/video/init.mp4?v={version}"))
        .await
        .2;
    for name in [&b"encv"[..], b"hvc1", b"sinf", b"cbcs", b"tenc"] {
        assert!(
            init.windows(4).any(|window| window == name),
            "{}",
            String::from_utf8_lossy(name)
        );
    }
}

// ---- Whole-segment HLS AES-128 (TDD 0012) ----

const AES_KEY: &str = "2b7e151628aed2a6abf7158809cf4f3c";
const AES_KEY_URI: &str = "https://keys.example.com/k1";

/// Decrypts one AES-128 segment with `openssl`, the way a player does: the media sequence number
/// is the IV. `None` when `openssl` is not installed.
fn openssl_decrypt(key: &str, segment_index: u32, encrypted: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let iv = format!("{segment_index:032x}");
    let mut child = Command::new("openssl")
        .args(["enc", "-d", "-aes-128-cbc", "-K", key, "-iv", &iv])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(encrypted).ok()?;
    let output = child.wait_with_output().ok()?;
    output.status.success().then_some(output.stdout)
}

async fn aes128_harness() -> Harness {
    let h = harness().await;
    h.mapper.state.set(
        "aes",
        Answer::file("v1", "h264-aac.mp4").with_hls_aes128(AES_KEY, AES_KEY_URI),
    );
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac.mp4"));
    h
}

#[tokio::test]
async fn aes128_playlists_name_the_key_after_the_init_segment_which_stays_clear() {
    let h = aes128_harness().await;

    let master = fetch(&h.app, "/hls/aes/master.m3u8").await;
    assert_eq!(master.0, StatusCode::OK);
    assert!(
        !text(&master.2).contains("I-FRAME"),
        "I-frame fragments are not encrypted whole, so none are offered"
    );
    let version = version_in(&master.2);
    for track in ["muxed", "video", "audio-1"] {
        let playlist = text(
            &fetch(&h.app, &format!("/hls/aes/{track}/index.m3u8"))
                .await
                .2,
        );
        let lines = playlist.lines().collect::<Vec<_>>();
        let map = lines
            .iter()
            .position(|l| l.starts_with("#EXT-X-MAP"))
            .unwrap();
        let key = lines
            .iter()
            .position(|l| l.starts_with("#EXT-X-KEY"))
            .unwrap();
        assert!(map < key, "the init segment is sent clear: {playlist}");
        assert_eq!(
            lines[key],
            format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{AES_KEY_URI}\""),
            "no IV: each segment's sequence number is its IV"
        );
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }
    let init = fetch(&h.app, &format!("/hls/aes/video/init.mp4?v={version}")).await;
    assert_eq!(&init.2[4..8], b"ftyp");
    // The key is part of the URL version: the same media under another key has other URLs.
    let clear = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    assert_ne!(version, clear);
    h.mapper.state.set(
        "other",
        Answer::file("v1", "h264-aac.mp4").with_hls_aes128(AES_KEY, "https://keys.example.com/k2"),
    );
    assert_ne!(
        version,
        version_in(&fetch(&h.app, "/hls/other/master.m3u8").await.2)
    );
}

#[tokio::test]
async fn aes128_segments_decrypt_to_exactly_the_clear_segments() {
    if std::process::Command::new("openssl")
        .arg("version")
        .output()
        .is_err()
    {
        eprintln!("skipping: openssl is not installed");
        return;
    }
    let h = aes128_harness().await;
    let version = version_in(&fetch(&h.app, "/hls/aes/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    for track in ["muxed", "video", "audio-1"] {
        let playlist = text(
            &fetch(&h.app, &format!("/hls/aes/{track}/index.m3u8"))
                .await
                .2,
        );
        let segments = playlist.matches("#EXTINF").count();
        assert!(segments >= 2, "{track}");
        for index in 0..segments {
            let encrypted = fetch(
                &h.app,
                &format!("/hls/aes/{track}/segments/{index}/media.m4s?v={version}"),
            )
            .await;
            assert_eq!(encrypted.0, StatusCode::OK, "{track} {index}");
            assert_eq!(encrypted.2.len() % 16, 0, "{track} {index}");
            // Not a fragment any more: the `moof` is inside the encryption.
            assert_ne!(&encrypted.2[4..8], b"moof", "{track} {index}");

            let clear = fetch(
                &h.app,
                &format!("/hls/clear/{track}/segments/{index}/media.m4s?v={clear_version}"),
            )
            .await;
            let decrypted = openssl_decrypt(AES_KEY, u32::try_from(index).unwrap(), &encrypted.2)
                .unwrap_or_else(|| panic!("{track} {index} decrypts and unpads"));
            assert_eq!(decrypted, clear.2.to_vec(), "{track} {index}");
            assert_eq!(&decrypted[4..8], b"moof");
        }
    }
}

#[tokio::test]
async fn aes128_segments_support_head_and_ranges() {
    let h = aes128_harness().await;
    let version = version_in(&fetch(&h.app, "/hls/aes/master.m3u8").await.2);
    let uri = format!("/hls/aes/video/segments/0/media.m4s?v={version}");
    let full = fetch(&h.app, &uri).await;

    let head = h
        .app
        .clone()
        .oneshot(Request::head(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        head.headers()[axum::http::header::CONTENT_LENGTH],
        full.2.len().to_string()
    );

    let ranged = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header(axum::http::header::RANGE, "bytes=16-47")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(ranged.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, full.2.slice(16..48));
}

#[tokio::test]
async fn aes128_assets_are_hls_only() {
    let h = aes128_harness().await;
    let version = version_in(&fetch(&h.app, "/hls/aes/master.m3u8").await.2);
    for uri in [
        "/dash/aes/manifest.mpd".to_owned(),
        format!("/dash/aes/video/init.mp4?v={version}"),
        format!("/dash/aes/video/segments/0/media.m4s?v={version}"),
        "/hls/aes/video/iframes.m3u8".to_owned(),
        format!("/hls/aes/video/iframes/0/media.m4s?v={version}"),
    ] {
        assert_eq!(status(&h.app, &uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    // The clear asset next to it still has all of them.
    assert_eq!(
        status(&h.app, "/dash/clear/manifest.mpd").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn aes128_answers_the_mapper_may_not_give() {
    let h = harness().await;
    let encryption = clear_key_encryption(KEY);
    let cases = [
        (
            "both",
            Answer::file("v1", "h264-aac.mp4")
                .with_hls_aes128(AES_KEY, AES_KEY_URI)
                .with_encryption(encryption.clone()),
        ),
        (
            "clip-encryption",
            Answer::clips_with_encryption(
                "v1",
                &[clip_encrypted(
                    "h264-aac.mp4",
                    None,
                    None,
                    Some(clear_key_encryption(KEY)),
                )],
            )
            .with_hls_aes128(AES_KEY, AES_KEY_URI),
        ),
        (
            "short-key",
            Answer::file("v1", "h264-aac.mp4").with_hls_aes128("abcd", AES_KEY_URI),
        ),
        (
            "quote-in-uri",
            Answer::file("v1", "h264-aac.mp4").with_hls_aes128(AES_KEY, "https://k/\"x"),
        ),
    ];
    for (name, answer) in cases {
        h.mapper.state.set(name, answer);
        let response = fetch(&h.app, &format!("/hls/{name}/master.m3u8")).await;
        assert!(
            response.0.is_server_error() || response.0.is_client_error(),
            "{name}: {}",
            response.0
        );
        assert!(
            !text(&response.2).contains(AES_KEY),
            "{name}: the key is never echoed"
        );
    }
}

/// An independent HLS client, `FFmpeg`, fetches the key from its URI, decrypts every segment, and
/// decodes the stream: the video and the audio of the muxed stream, and the separate renditions.
#[tokio::test]
async fn ffmpeg_plays_an_aes128_stream_and_cannot_with_another_key() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let data_uri = |key: &str| {
        let bytes = (0..16)
            .map(|at| u8::from_str_radix(&key[at * 2..at * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>();
        format!(
            "data:text/plain;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    };
    let h = harness().await;
    h.mapper.state.set(
        "aes",
        Answer::file("v1", "h264-aac.mp4").with_hls_aes128(AES_KEY, &data_uri(AES_KEY)),
    );
    // The same stream, whose playlists point at a different key than the one that encrypted it.
    h.mapper.state.set(
        "wrong",
        Answer::file("v1", "h264-aac.mp4")
            .with_hls_aes128(AES_KEY, &data_uri("00112233445566778899aabbccddeeff")),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = h.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let frames = |path: String| {
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("ffprobe")
                .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
                .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
                // The test's keys are `data:` URIs, which FFmpeg opens only when told to.
                .args(["-allowed_extensions", "ALL"])
                .arg(format!("http://{address}{path}"))
                .output()
                .unwrap()
        })
    };
    for path in ["/hls/aes/master.m3u8", "/hls/aes/video/index.m3u8"] {
        let output = frames(path.to_owned()).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "", "{path}");
        // ffprobe lists an HLS program's stream once more under the program.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.lines().next(), Some("90"), "{path}");
    }
    let wrong = frames("/hls/wrong/video/index.m3u8".to_owned())
        .await
        .unwrap();
    // Garbage in, so no 90 decoded frames out, whatever FFmpeg says about it.
    assert_ne!(
        String::from_utf8_lossy(&wrong.stdout).lines().next(),
        Some("90"),
        "a wrong key does not decrypt"
    );
    server.abort();
}

/// A rotated token file is picked up without a restart: the mapper starts requiring the new
/// token, the file is rewritten, and resolution recovers. While the file is unreadable the last
/// good token keeps working.
#[tokio::test]
async fn a_rotated_token_file_is_picked_up_without_a_restart() {
    let directory = crate::testutil::ScratchDir::new("token-rotation");
    let file = directory.path().join("token");
    std::fs::write(&file, "first\n").unwrap();
    let mapper = MockMapper::start().await;
    *mapper.state.required_token.lock().unwrap() = Some("first".to_owned());
    for id in ["a", "b", "c", "d"] {
        mapper.state.set(id, Answer::file("v1", "h264-aac.mp4"));
    }
    let config = config_for(&mapper, |config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.bearer_token = Some(crate::config::read_token_file(&file).unwrap());
        settings.bearer_token_file = Some(file.clone());
        settings.bearer_token_reload_ms = 50;
    });
    let app = router(AppState::new(&config).unwrap());
    assert_eq!(status(&app, "/hls/a/master.m3u8").await, StatusCode::OK);

    // The mapper rotates its token and the operator rewrites the file.
    *mapper.state.required_token.lock().unwrap() = Some("second".to_owned());
    std::fs::write(&file, "second\n").unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(status(&app, "/hls/b/master.m3u8").await, StatusCode::OK);
    assert_eq!(
        mapper
            .state
            .seen_tokens
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .as_deref(),
        Some("Bearer second")
    );

    // A file that is briefly missing or empty during a rotation does not break resolution.
    std::fs::write(&file, "").unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(status(&app, "/hls/c/master.m3u8").await, StatusCode::OK);
    std::fs::remove_file(&file).unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(status(&app, "/hls/d/master.m3u8").await, StatusCode::OK);
}

/// With reloading off, the token read at startup is the one used for good.
#[tokio::test]
async fn a_token_file_is_not_re_read_when_reloading_is_off() {
    let directory = crate::testutil::ScratchDir::new("token-no-reload");
    let file = directory.path().join("token");
    std::fs::write(&file, "first").unwrap();
    let mapper = MockMapper::start().await;
    *mapper.state.required_token.lock().unwrap() = Some("second".to_owned());
    mapper.state.set("a", Answer::file("v1", "h264-aac.mp4"));
    let config = config_for(&mapper, |config| {
        let ResolverSettings::Http(settings) = &mut config.resolver else {
            unreachable!()
        };
        settings.bearer_token = Some(crate::config::read_token_file(&file).unwrap());
        settings.bearer_token_file = Some(file.clone());
        settings.bearer_token_reload_ms = 0;
    });
    let app = router(AppState::new(&config).unwrap());
    std::fs::write(&file, "second").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        status(&app, "/hls/a/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

// ---- Key rotation within an asset (TDD 0013) ----

/// A version 0 `pssh` box for `system` holding `data`, as base64.
fn pssh_b64(system: &[u8; 16], data: &[u8]) -> String {
    use base64::Engine;
    let mut boxed = u32::try_from(32 + data.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    boxed.extend_from_slice(b"pssh");
    boxed.extend_from_slice(&[0; 4]);
    boxed.extend_from_slice(system);
    boxed.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    boxed.extend_from_slice(data);
    base64::engine::general_purpose::STANDARD.encode(boxed)
}

const WIDEVINE_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];
const WIDEVINE_UUID: &str = "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";

fn period(start_ms: u64, key_id: &str, key: &str, tag: &str) -> serde_json::Value {
    serde_json::json!({
        "start_ms": start_ms,
        "keys": [{ "key_id": key_id, "key": key }],
        "systems": [{ "system_id": WIDEVINE_UUID, "pssh": pssh_b64(&WIDEVINE_ID, tag.as_bytes()) }],
    })
}

/// The 1 s fixture's three segments: clear, then `KEY`, then `KEY_B`.
fn rotating_encryption() -> serde_json::Value {
    serde_json::json!({
        "scheme": "cbcs",
        "periods": [
            { "start_ms": 0, "clear": true },
            period(1000, KEY_ID, KEY, "period-one"),
            period(2000, KEY_ID_B, KEY_B, "period-two"),
        ],
    })
}

/// The child box types of `bytes`, or of the payload at `path` of nested box names.
fn child_boxes<'a>(mut bytes: &'a [u8], path: &[&[u8; 4]]) -> Vec<(&'a [u8], &'a [u8])> {
    fn walk(bytes: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut found = Vec::new();
        let mut at = 0;
        while at + 8 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            assert!(size >= 8 && at + size <= bytes.len(), "a well-formed box");
            found.push((&bytes[at + 4..at + 8], &bytes[at + 8..at + size]));
            at += size;
        }
        found
    }
    for name in path {
        bytes = walk(bytes)
            .into_iter()
            .find(|(kind, _)| kind == name)
            .unwrap_or_else(|| panic!("no {}", String::from_utf8_lossy(*name)))
            .1;
    }
    walk(bytes)
}

fn box_names(boxes: &[(&[u8], &[u8])]) -> Vec<String> {
    boxes
        .iter()
        .map(|(kind, _)| String::from_utf8_lossy(kind).into_owned())
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one asset, every box checked in turn"
)]
async fn fragments_say_which_key_protects_them() {
    let h = harness().await;
    h.mapper.state.set(
        "rot",
        Answer::file("v1", "h264-aac.mp4").with_encryption(rotating_encryption()),
    );
    let version = version_in(&fetch(&h.app, "/hls/rot/master.m3u8").await.2);
    let segment = |n: u32| {
        let h = &h;
        let version = version.clone();
        async move {
            fetch(
                &h.app,
                &format!("/hls/rot/video/segments/{n}/media.m4s?v={version}"),
            )
            .await
            .2
        }
    };

    // The init segment declares the first *encrypted* period, though the asset starts clear.
    let init = fetch(&h.app, &format!("/hls/rot/video/init.mp4?v={version}"))
        .await
        .2;
    let kid: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&KEY_ID[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    let tenc = init.windows(4).position(|w| w == b"tenc").expect("tenc");
    assert_eq!(
        &init[tenc + 4 + 4 + 4..tenc + 4 + 4 + 4 + 16],
        kid.as_slice()
    );

    let clear = segment(0).await;
    let first = segment(1).await;
    let second = segment(2).await;
    let kid_b: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&KEY_ID_B[i * 2..i * 2 + 2], 16).unwrap())
        .collect();

    // moof children: mfhd, then pssh boxes of the period, then the traf.
    let moof = |bytes: &Bytes| {
        child_boxes(bytes, &[b"moof"])
            .into_iter()
            .map(|(k, p)| (k.to_vec(), p.to_vec()))
            .collect::<Vec<_>>()
    };
    let names = |parts: &[(Vec<u8>, Vec<u8>)]| {
        parts
            .iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&moof(&clear)), ["mfhd", "traf"]);
    assert_eq!(names(&moof(&first)), ["mfhd", "pssh", "traf"]);
    assert_eq!(names(&moof(&second)), ["mfhd", "pssh", "traf"]);
    let pssh_data = |parts: &[(Vec<u8>, Vec<u8>)]| {
        let payload = &parts.iter().find(|(k, _)| k == b"pssh").unwrap().1;
        String::from_utf8_lossy(&payload[4 + 16 + 4..]).into_owned()
    };
    assert_eq!(pssh_data(&moof(&first)), "period-one");
    assert_eq!(pssh_data(&moof(&second)), "period-two");

    let traf = |bytes: &Bytes| box_names(&child_boxes(bytes, &[b"moof", b"traf"]));
    // Clear: a `seig` group and no encryption boxes. Under the init segment's own key: no group.
    // Under another key: a group naming it.
    assert_eq!(traf(&clear), ["tfhd", "tfdt", "trun", "sgpd", "sbgp"]);
    assert_eq!(
        traf(&first),
        ["tfhd", "tfdt", "trun", "senc", "saiz", "saio"]
    );
    assert_eq!(
        traf(&second),
        [
            "tfhd", "tfdt", "trun", "senc", "saiz", "saio", "sgpd", "sbgp"
        ]
    );

    let group = |bytes: &Bytes| {
        let parts = child_boxes(bytes, &[b"moof", b"traf"]);
        let description = parts
            .iter()
            .find(|(k, _)| *k == b"sgpd")
            .unwrap()
            .1
            .to_vec();
        let mapping = parts
            .iter()
            .find(|(k, _)| *k == b"sbgp")
            .unwrap()
            .1
            .to_vec();
        (description, mapping)
    };
    // sgpd: version/flags 4, type 4, default_length 4, entry_count 4, then the entry:
    // reserved, pattern, isProtected, ivsize, kid, [constant iv size, constant iv].
    let (description, mapping) = group(&second);
    assert_eq!(description[0], 1, "version 1");
    assert_eq!(&description[4..8], b"seig");
    assert_eq!(
        u32::from_be_bytes(description[8..12].try_into().unwrap()),
        37
    );
    let entry = &description[16..];
    assert_eq!(
        &entry[..4],
        [0, 0x19, 1, 0],
        "1:9 pattern, protected, constant IV"
    );
    assert_eq!(&entry[4..20], kid_b.as_slice());
    assert_eq!(entry[20], 16);
    assert_eq!(
        &entry[21..37],
        derived_iv_of(KEY_ID_B).as_slice(),
        "the constant IV TDD 0009 derives"
    );
    assert_eq!(&mapping[4..8], b"seig");
    assert_eq!(
        u32::from_be_bytes(mapping[12..16].try_into().unwrap()),
        30,
        "every sample"
    );
    assert_eq!(
        u32::from_be_bytes(mapping[16..20].try_into().unwrap()),
        0x0001_0001
    );
    let (description, _) = group(&clear);
    assert_eq!(
        u32::from_be_bytes(description[8..12].try_into().unwrap()),
        20
    );
    assert_eq!(&description[16..20], [0, 0, 0, 0], "not protected");

    // The first period's samples are the source's own bytes; the later ones are not.
    assert_ne!(first.len(), 0);
    assert_ne!(clear[clear.len() - 64..], first[first.len() - 64..]);
}

#[tokio::test]
async fn hls_names_each_periods_key_before_its_first_segment() {
    use base64::Engine;
    let h = harness().await;
    h.mapper.state.set(
        "rot",
        Answer::file("v1", "h264-aac.mp4").with_encryption(rotating_encryption()),
    );
    let master = text(&fetch(&h.app, "/hls/rot/master.m3u8").await.2);
    let playlist = text(&fetch(&h.app, "/hls/rot/video/index.m3u8").await.2);
    let lines: Vec<&str> = playlist.lines().collect();

    assert_eq!(
        lines.iter().filter(|l| l.starts_with("#EXT-X-MAP")).count(),
        1
    );
    assert!(!playlist.contains("DISCONTINUITY"), "{playlist}");
    let position = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    let key_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("#EXT-X-KEY"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(key_lines.len(), 2, "{playlist}");
    // The asset starts clear, so no key precedes the first segment, and each later period's
    // key is the line right before the EXTINF of its first segment.
    assert!(key_lines[0] > position("segments/0/"), "{playlist}");
    assert_eq!(key_lines[0] + 1, position("segments/1/") - 1, "{playlist}");
    assert_eq!(key_lines[1] + 1, position("segments/2/") - 1, "{playlist}");
    // Each key line carries its own period's `pssh`.
    let uri = |line: &str| {
        line.split("base64,")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    let decode = |line: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(uri(line))
            .unwrap()
    };
    assert!(decode(lines[key_lines[0]]).ends_with(b"period-one"));
    assert!(decode(lines[key_lines[1]]).ends_with(b"period-two"));
    assert!(lines[key_lines[0]].contains(&format!("KEYID=0x{KEY_ID}")));
    assert!(lines[key_lines[1]].contains(&format!("KEYID=0x{KEY_ID_B}")));
    // The master offers every period's licence ahead of time.
    assert_eq!(master.matches("#EXT-X-SESSION-KEY").count(), 2, "{master}");
    // No I-frame stream: a keyframe's fragment would need its period's key.
    assert!(!master.contains("I-FRAME"), "{master}");
    assert_eq!(
        status(&h.app, "/hls/rot/video/iframes.m3u8").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_period_can_end_the_encryption_with_method_none() {
    let h = harness().await;
    h.mapper.state.set(
        "tail",
        Answer::file("v1", "h264-aac.mp4").with_encryption(serde_json::json!({
            "scheme": "cbcs",
            "periods": [
                period(0, KEY_ID, KEY, "head"),
                { "start_ms": 1000, "clear": true },
            ],
        })),
    );
    let playlist = text(&fetch(&h.app, "/hls/tail/video/index.m3u8").await.2);
    let lines: Vec<&str> = playlist.lines().collect();
    let none = lines
        .iter()
        .position(|l| *l == "#EXT-X-KEY:METHOD=NONE")
        .expect(&playlist);
    let one = lines
        .iter()
        .position(|l| l.contains("segments/1/"))
        .unwrap();
    assert_eq!(none + 2, one, "{playlist}");
    // The opening key is before the map, as for any encrypted asset.
    let first_key = lines
        .iter()
        .position(|l| l.starts_with("#EXT-X-KEY:METHOD=SAMPLE-AES"))
        .unwrap();
    assert!(
        first_key
            < lines
                .iter()
                .position(|l| l.starts_with("#EXT-X-MAP"))
                .unwrap()
    );
}

#[tokio::test]
async fn dash_declares_the_first_encrypted_period_in_one_period() {
    let h = harness().await;
    h.mapper.state.set(
        "rot",
        Answer::file("v1", "h264-aac.mp4").with_encryption(rotating_encryption()),
    );
    let manifest = text(&fetch(&h.app, "/dash/rot/manifest.mpd").await.2);
    assert_eq!(manifest.matches("<Period").count(), 1, "{manifest}");
    assert!(
        manifest.contains("cenc:default_KID=\"01234567-89ab-cdef-0123-456789abcdef\""),
        "{manifest}"
    );
    assert!(!manifest.contains("fedcba98-7654"), "{manifest}");
}

#[tokio::test]
async fn re_keying_any_period_gives_new_urls() {
    let h = harness().await;
    // Three assets whose timelines differ in exactly one respect from the first.
    let base = rotating_encryption();
    let mut rekeyed = rotating_encryption();
    rekeyed["periods"][2] = period(2000, KEY_ID_B, KEY, "period-two");
    let mut moved = rotating_encryption();
    moved["periods"][2]["start_ms"] = serde_json::json!(2500);
    let mut versions = Vec::new();
    for (id, encryption) in [("a", base), ("b", rekeyed), ("c", moved)] {
        h.mapper.state.set(
            id,
            Answer::file("v1", "h264-aac.mp4").with_encryption(encryption),
        );
        versions.push(version_in(
            &fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2,
        ));
    }
    assert_ne!(versions[0], versions[1], "a re-keyed period");
    assert_ne!(versions[0], versions[2], "a moved period");
    assert_ne!(versions[1], versions[2]);
}

#[tokio::test]
async fn clear_lead_is_the_same_asset_as_the_explicit_period_list() {
    let h = harness().await;
    h.mapper.state.set(
        "lead",
        Answer::file("v1", "h264-aac.mp4").with_encryption(serde_json::json!({
            "scheme": "cbcs",
            "clear_lead_ms": 1000,
            "keys": [{ "key_id": KEY_ID, "key": KEY }],
            "systems": [{ "system_id": WIDEVINE_UUID, "pssh": pssh_b64(&WIDEVINE_ID, b"period-one") }],
        })),
    );
    h.mapper.state.set(
        "explicit",
        Answer::file("v1", "h264-aac.mp4").with_encryption(serde_json::json!({
            "scheme": "cbcs",
            "periods": [{ "start_ms": 0, "clear": true }, period(1000, KEY_ID, KEY, "period-one")],
        })),
    );
    let lead = text(&fetch(&h.app, "/hls/lead/video/index.m3u8").await.2);
    let explicit = text(&fetch(&h.app, "/hls/explicit/video/index.m3u8").await.2);
    assert_eq!(lead, explicit);
    assert!(lead.contains("EXT-X-KEY"));
}

#[tokio::test]
async fn periods_are_refused_where_they_are_not_supported() {
    let h = harness().await;
    h.mapper.state.set(
        "seq",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac.mp4", None, None),
                clip("h264-aac.mp4", None, None),
            ],
        )
        .with_encryption(rotating_encryption()),
    );
    h.mapper.state.set(
        "clip-own",
        Answer::clips_with_encryption(
            "v1",
            &[
                clip_encrypted("h264-aac.mp4", None, None, Some(rotating_encryption())),
                clip_encrypted("h264-aac.mp4", None, None, None),
            ],
        ),
    );
    for id in ["seq", "clip-own"] {
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
        );
    }
}

/// The URIs of every media playlist a master names: video variants and `EXT-X-MEDIA` renditions.
fn media_playlists_of(master: &str) -> Vec<String> {
    master
        .lines()
        .filter_map(|line| {
            line.strip_prefix("#EXT-X-MEDIA:")
                .and_then(|rest| rest.split("URI=\"").nth(1))
                .and_then(|rest| rest.split('"').next())
                .or_else(|| (!line.starts_with('#') && !line.is_empty()).then_some(line))
        })
        .map(|uri| uri.split('?').next().unwrap().to_owned())
        .collect()
}

/// Stage 2 of TDD 0013: one period list, applied to every rendition, each with one init segment.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one asset, every playlist and fragment checked in turn"
)]
async fn an_adaptive_asset_rotates_keys_in_every_rendition() {
    let h = harness().await;
    h.mapper.state.set(
        "abr",
        Answer::renditions(
            "v1",
            &[
                ("a", "rendition-480p.mp4"),
                ("b", "rendition-720p.mp4"),
                ("audio-en", "rendition-audio-en.m4a"),
            ],
        )
        .with_encryption(rotating_encryption()),
    );
    h.mapper.state.set(
        "single",
        Answer::file("v1", "h264-aac.mp4").with_encryption(rotating_encryption()),
    );

    let (code, _, body) = fetch(&h.app, "/hls/abr/master.m3u8").await;
    assert_eq!(code, StatusCode::OK);
    let master = text(&body);
    let version = version_in(&body);
    assert!(!master.contains("I-FRAME"), "{master}");
    // The session keys are the same union a single file with this period list gets.
    let session_keys = |m: &str| {
        m.lines()
            .filter(|l| l.starts_with("#EXT-X-SESSION-KEY"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let single_master = text(&fetch(&h.app, "/hls/single/master.m3u8").await.2);
    assert!(!session_keys(&master).is_empty(), "{master}");
    assert_eq!(session_keys(&master), session_keys(&single_master));

    let playlists = media_playlists_of(&master);
    assert_eq!(
        playlists.len(),
        3,
        "two video variants and one audio\n{master}"
    );
    for playlist in &playlists {
        let body = text(&fetch(&h.app, &format!("/hls/abr/{playlist}")).await.2);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("#EXT-X-MAP")).count(),
            1,
            "{playlist}\n{body}"
        );
        assert!(!body.contains("DISCONTINUITY"), "{playlist}\n{body}");
        let keys: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("#EXT-X-KEY"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(keys.len(), 2, "{playlist}\n{body}");
        assert!(
            lines[keys[0]].contains(&format!("KEYID=0x{KEY_ID}")),
            "{playlist}: first key\n{body}"
        );
        assert!(
            lines[keys[1]].contains(&format!("KEYID=0x{KEY_ID_B}")),
            "{playlist}: second key\n{body}"
        );
        // A clear lead: no key before the first segment.
        let first_segment = lines
            .iter()
            .position(|l| !l.starts_with('#') && !l.is_empty())
            .unwrap();
        assert!(keys[0] > first_segment, "{playlist}\n{body}");
        if playlist.starts_with("video-") {
            // Video renditions share boundaries: 1000 ms is segment 1, 2000 ms segment 2.
            let at = |n: u32| {
                lines
                    .iter()
                    .position(|l| l.contains(&format!("segments/{n}/")))
                    .unwrap()
            };
            assert_eq!(keys[0] + 2, at(1), "{playlist}\n{body}");
            assert_eq!(keys[1] + 2, at(2), "{playlist}\n{body}");
        }
    }

    // Each video rendition: segment 0 is declared clear, segment 2 names KEY_ID_B in `seig` and
    // carries period two's `pssh`.
    for rendition in ["video-a", "video-b"] {
        let segment = |n: u32| {
            let h = &h;
            let version = version.clone();
            async move {
                let (code, _, bytes) = fetch(
                    &h.app,
                    &format!("/hls/abr/{rendition}/segments/{n}/media.m4s?v={version}"),
                )
                .await;
                assert_eq!(code, StatusCode::OK, "{rendition} {n}");
                bytes
            }
        };
        let clear = segment(0).await;
        let second = segment(2).await;
        assert_eq!(
            box_names(&child_boxes(&clear, &[b"moof", b"traf"])),
            ["tfhd", "tfdt", "trun", "sgpd", "sbgp"],
            "{rendition}"
        );
        let traf = child_boxes(&second, &[b"moof", b"traf"]);
        assert_eq!(
            box_names(&traf),
            [
                "tfhd", "tfdt", "trun", "senc", "saiz", "saio", "sgpd", "sbgp"
            ],
            "{rendition}"
        );
        let sgpd = traf.iter().find(|(k, _)| *k == b"sgpd").unwrap().1;
        assert_eq!(
            &sgpd[16 + 4..16 + 20],
            hex_bytes(KEY_ID_B).as_slice(),
            "{rendition}"
        );
        let moof = child_boxes(&second, &[b"moof"]);
        let pssh = moof.iter().find(|(k, _)| *k == b"pssh").unwrap().1;
        assert_eq!(&pssh[4 + 16 + 4..], b"period-two", "{rendition}");
    }

    // No I-frame playlist; DASH has one Period declaring the first encrypted period.
    assert_eq!(
        status(&h.app, "/hls/abr/video/iframes.m3u8").await,
        StatusCode::NOT_FOUND
    );
    let manifest = text(&fetch(&h.app, "/dash/abr/manifest.mpd").await.2);
    assert_eq!(manifest.matches("<Period").count(), 1, "{manifest}");
    assert!(
        manifest.contains("cenc:default_KID=\"01234567-89ab-cdef-0123-456789abcdef\""),
        "{manifest}"
    );
    assert!(!manifest.contains("fedcba98-7654"), "{manifest}");
}

#[tokio::test]
async fn re_keying_a_period_of_an_adaptive_asset_gives_new_urls() {
    let h = harness().await;
    let renditions = [("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")];
    let mut rekeyed = rotating_encryption();
    rekeyed["periods"][2] = period(2000, KEY_ID_B, KEY, "period-two");
    h.mapper.state.set(
        "base",
        Answer::renditions("v1", &renditions).with_encryption(rotating_encryption()),
    );
    h.mapper.state.set(
        "rekeyed",
        Answer::renditions("v1", &renditions).with_encryption(rekeyed),
    );
    let base = version_in(&fetch(&h.app, "/hls/base/master.m3u8").await.2);
    let other = version_in(&fetch(&h.app, "/hls/rekeyed/master.m3u8").await.2);
    assert_ne!(base, other);
}

/// `FFmpeg` decodes a rendition's period-one segment with that period's key and not with period
/// two's, so the rendition is encrypted under the schedule, not only signalled.
#[tokio::test]
async fn ffmpeg_decrypts_a_renditions_period_under_its_own_key() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffmpeg is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set(
        "abr",
        Answer::renditions(
            "v1",
            &[("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")],
        )
        .with_encryption(rotating_encryption()),
    );
    let version = version_in(&fetch(&h.app, "/hls/abr/master.m3u8").await.2);
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/key-rotation-abr");
    std::fs::create_dir_all(&directory).unwrap();
    for rendition in ["video-a", "video-b"] {
        // The init segment's `tenc` names period one, so period one's segment is judged against it.
        let mut bytes = fetch(
            &h.app,
            &format!("/hls/abr/{rendition}/init.mp4?v={version}"),
        )
        .await
        .2
        .to_vec();
        bytes.extend_from_slice(
            &fetch(
                &h.app,
                &format!("/hls/abr/{rendition}/segments/1/media.m4s?v={version}"),
            )
            .await
            .2,
        );
        let path = directory.join(format!("{rendition}-segment-1.mp4"));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            decode_errors(&path, Some(KEY)),
            "",
            "{rendition} under its own key"
        );
        assert_ne!(
            decode_errors(&path, Some(KEY_B)),
            "",
            "{rendition} under period two's key"
        );
    }
}

/// The IV TDD 0009 derives when the mapper gives none, from the formula alone.
fn derived_iv_of(key_id_hex: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"segmentor cbcs iv");
    hasher.update(hex_bytes(key_id_hex));
    hasher.finalize()[..16].to_vec()
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// An independent implementation recovers every frame, period by period, with that period's key
/// and not with another's.
#[tokio::test]
async fn ffmpeg_decrypts_each_period_under_its_own_key() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let h = harness().await;
    h.mapper.state.set(
        "rot",
        Answer::file("v1", "h264-aac.mp4").with_encryption(rotating_encryption()),
    );
    let version = version_in(&fetch(&h.app, "/hls/rot/master.m3u8").await.2);
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/key-rotation");
    std::fs::create_dir_all(&directory).unwrap();
    let init = fetch(&h.app, &format!("/hls/rot/video/init.mp4?v={version}"))
        .await
        .2;
    // FFmpeg takes a fragment's key and IV from `tenc` and ignores `seig` groups' IVs, so a later
    // period is judged against an init segment whose `tenc` names that period's key and IV: the
    // encrypted bytes are then checked by FFmpeg, and the `seig` entry by the box test above.
    let init_for = |key_id: &str| {
        let mut bytes = init.to_vec();
        let at = bytes.windows(4).position(|w| w == b"tenc").unwrap() + 4 + 8;
        bytes[at..at + 16].copy_from_slice(&hex_bytes(key_id));
        bytes[at + 17..at + 33].copy_from_slice(&derived_iv_of(key_id));
        bytes
    };
    for (segment, own, other) in [
        (0u32, None, Some(KEY)),
        (1, Some(KEY), Some(KEY_B)),
        (2, Some(KEY_B), Some(KEY)),
    ] {
        let mut bytes = if segment == 2 {
            init_for(KEY_ID_B)
        } else {
            init.to_vec()
        };
        bytes.extend_from_slice(
            &fetch(
                &h.app,
                &format!("/hls/rot/video/segments/{segment}/media.m4s?v={version}"),
            )
            .await
            .2,
        );
        let path = directory.join(format!("segment-{segment}.mp4"));
        std::fs::write(&path, bytes).unwrap();

        let frames = |key: Option<&str>| {
            let mut probe = std::process::Command::new("ffprobe");
            probe.args(["-v", "error", "-count_frames", "-select_streams", "v:0"]);
            probe.args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]);
            if let Some(key) = key {
                probe.args(["-decryption_key", key]);
            }
            String::from_utf8_lossy(&probe.arg(&path).output().unwrap().stdout)
                .trim_matches([',', ' ', '\n'])
                .to_owned()
        };
        assert_eq!(
            decode_errors(&path, own),
            "",
            "segment {segment} under its own key"
        );
        assert_eq!(frames(own), "30", "segment {segment}");
        if segment != 0 {
            assert_ne!(
                decode_errors(&path, other),
                "",
                "segment {segment} under another period's key"
            );
        }
    }
}

/// A period begins at the first segment that starts at or after its `start_ms`: one asked for
/// mid-segment waits for the next segment, one past the end never starts, and two that land on one
/// segment cannot both be honoured.
#[tokio::test]
async fn periods_begin_on_segment_boundaries() {
    let h = harness().await;
    let timeline = |periods: Vec<serde_json::Value>| serde_json::json!({ "scheme": "cbcs", "periods": periods });
    h.mapper.state.set(
        "late",
        Answer::file("v1", "h264-aac.mp4").with_encryption(timeline(vec![
            serde_json::json!({ "start_ms": 0, "clear": true }),
            period(1500, KEY_ID, KEY, "mid"),
            period(60_000, KEY_ID_B, KEY_B, "never"),
        ])),
    );
    h.mapper.state.set(
        "crowded",
        Answer::file("v1", "h264-aac.mp4").with_encryption(timeline(vec![
            serde_json::json!({ "start_ms": 0, "clear": true }),
            period(1200, KEY_ID, KEY, "one"),
            period(1800, KEY_ID_B, KEY_B, "two"),
        ])),
    );

    let playlist = text(&fetch(&h.app, "/hls/late/video/index.m3u8").await.2);
    let lines: Vec<&str> = playlist.lines().collect();
    let key = lines
        .iter()
        .position(|l| l.starts_with("#EXT-X-KEY"))
        .expect(&playlist);
    let two = lines
        .iter()
        .position(|l| l.contains("segments/2/"))
        .unwrap();
    assert_eq!(
        key + 2,
        two,
        "1500 ms starts at the segment at 2000 ms\n{playlist}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("#EXT-X-KEY")).count(),
        1,
        "the period past the end never starts"
    );
    assert_eq!(
        status(&h.app, "/hls/crowded/video/index.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

/// Every segment URI of `playlist` (the lines that are not tags), in order. The media sequence
/// number of the Nth is N, whatever discontinuities lie between, because the playlist starts at 0.
fn segment_uris(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Fetches `uri` relative to the HLS directory `base` of `id`, under that asset's own version.
async fn fetch_segment(h: &Harness, id: &str, base: &str, uri: &str, version: &str) -> Bytes {
    let path = uri.split('?').next().unwrap();
    let (status, _, bytes) = fetch(&h.app, &format!("/hls/{id}/{base}{path}?v={version}")).await;
    assert_eq!(status, StatusCode::OK, "{id}/{base}{path}");
    bytes
}

/// Every segment of every media playlist in `playlists`, decrypted with OpenSSL under its media
/// sequence number as the IV, must equal the same segment of the clear twin.
async fn assert_segments_decrypt_to_the_clear_twin(
    h: &Harness,
    encrypted: &str,
    clear: &str,
    playlists: &[&str],
) -> usize {
    let version = version_in(
        &fetch(&h.app, &format!("/hls/{encrypted}/master.m3u8"))
            .await
            .2,
    );
    let clear_version = version_in(&fetch(&h.app, &format!("/hls/{clear}/master.m3u8")).await.2);
    let mut checked = 0;
    for playlist in playlists {
        let (base, _) = playlist
            .rsplit_once('/')
            .map_or(("", *playlist), |(b, f)| (b, f));
        let base = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        let text = text(
            &fetch(&h.app, &format!("/hls/{encrypted}/{playlist}"))
                .await
                .2,
        );
        for (index, uri) in segment_uris(&text).iter().enumerate() {
            let sealed = fetch_segment(h, encrypted, &base, uri, &version).await;
            let plain = fetch_segment(h, clear, &base, uri, &clear_version).await;
            assert_eq!(sealed.len() % 16, 0, "{playlist} {index}");
            let opened = openssl_decrypt(AES_KEY, u32::try_from(index).unwrap(), &sealed)
                .unwrap_or_else(|| panic!("{playlist} {index} decrypts and unpads"));
            assert_eq!(opened, plain.to_vec(), "{playlist} {index}");
            assert_eq!(&opened[4..8], b"moof");
            checked += 1;
        }
    }
    checked
}

fn openssl_available() -> bool {
    std::process::Command::new("openssl")
        .arg("version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn an_adaptive_asset_encrypts_every_rendition_under_one_key() {
    let h = harness().await;
    let renditions = [("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")];
    h.mapper.state.set(
        "aes",
        Answer::renditions("v1", &renditions).with_hls_aes128(AES_KEY, AES_KEY_URI),
    );
    h.mapper
        .state
        .set("clear", Answer::renditions("v1", &renditions));

    let master = fetch(&h.app, "/hls/aes/master.m3u8").await;
    assert_eq!(master.0, StatusCode::OK);
    let master_text = text(&master.2);
    assert!(!master_text.contains("I-FRAME"), "{master_text}");
    let version = version_in(&master.2);
    assert_ne!(
        version,
        version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2)
    );

    let mut playlists = Vec::new();
    for line in master_text.lines() {
        let uri = line
            .strip_prefix("#EXT-X-MEDIA:")
            .and_then(|rest| rest.split("URI=\"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .or_else(|| (!line.starts_with('#') && !line.is_empty()).then_some(line));
        if let Some(uri) = uri {
            playlists.push(uri.split('?').next().unwrap().to_owned());
        }
    }
    assert!(playlists.len() >= 2, "{master_text}");
    for playlist in &playlists {
        let body = text(&fetch(&h.app, &format!("/hls/aes/{playlist}")).await.2);
        let lines = body.lines().collect::<Vec<_>>();
        let map = lines
            .iter()
            .position(|l| l.starts_with("#EXT-X-MAP"))
            .unwrap();
        let key = lines
            .iter()
            .position(|l| l.starts_with("#EXT-X-KEY"))
            .unwrap();
        assert!(
            map < key,
            "{playlist}: the init segment stays clear\n{body}"
        );
        assert_eq!(
            lines[key],
            format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{AES_KEY_URI}\""),
            "{playlist}"
        );
    }
    for uri in [
        "/dash/aes/manifest.mpd".to_owned(),
        format!("/dash/aes/video-a/init.mp4?v={version}"),
        "/hls/aes/video/iframes.m3u8".to_owned(),
    ] {
        assert_eq!(status(&h.app, &uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(
        status(&h.app, "/dash/clear/manifest.mpd").await,
        StatusCode::OK
    );

    if openssl_available() {
        let refs = playlists.iter().map(String::as_str).collect::<Vec<_>>();
        let checked = assert_segments_decrypt_to_the_clear_twin(&h, "aes", "clear", &refs).await;
        assert!(checked >= 4, "{checked} segments checked");
    }
}

/// A sequence numbers its segments across clips, so the IV of a segment is its global number, not
/// its place in its own clip; and each clip's init segment is sent clear by switching the key off
/// for its `EXT-X-MAP` and on again after it.
#[tokio::test]
async fn a_sequence_encrypts_segments_under_their_global_media_sequence_number() {
    let h = harness().await;
    let clips = [
        clip("h264-aac.mp4", None, None),
        clip("rendition-720p.mp4", None, None),
        clip("h264-aac.mp4", Some(1500), None),
    ];
    h.mapper.state.set(
        "aes",
        Answer::clips("v1", &clips).with_hls_aes128(AES_KEY, AES_KEY_URI),
    );
    h.mapper.state.set("clear", Answer::clips("v1", &clips));

    let master = fetch(&h.app, "/hls/aes/master.m3u8").await;
    assert_eq!(master.0, StatusCode::OK);
    let playlist = text(&fetch(&h.app, "/hls/aes/video/index.m3u8").await.2);
    let key_line = format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{AES_KEY_URI}\"");
    let lines = playlist.lines().collect::<Vec<_>>();
    let maps = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("#EXT-X-MAP"))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    assert_eq!(maps.len(), 3, "{playlist}");
    for (position, &map) in maps.iter().enumerate() {
        assert_eq!(
            lines[map + 1],
            key_line,
            "clip {position}: key right after its map\n{playlist}"
        );
        if position > 0 {
            assert_eq!(
                lines[map - 1],
                "#EXT-X-KEY:METHOD=NONE",
                "clip {position}\n{playlist}"
            );
            assert_eq!(
                lines[map - 2],
                "#EXT-X-DISCONTINUITY",
                "clip {position}\n{playlist}"
            );
        } else {
            assert!(
                lines[..map].iter().all(|l| !l.starts_with("#EXT-X-KEY")),
                "{playlist}"
            );
        }
    }
    assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    let version = version_in(&master.2);
    assert_eq!(
        status(&h.app, "/dash/aes/manifest.mpd").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&h.app, "/dash/clear/manifest.mpd").await,
        StatusCode::OK
    );
    assert_ne!(
        version,
        version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2)
    );
    // The init segments stay clear.
    let init = fetch(
        &h.app,
        &format!("/hls/aes/video/clips/1/init.mp4?v={version}"),
    )
    .await;
    assert_eq!(&init.2[4..8], b"ftyp");

    if openssl_available() {
        let checked = assert_segments_decrypt_to_the_clear_twin(
            &h,
            "aes",
            "clear",
            &["video/index.m3u8", "audio-1/index.m3u8"],
        )
        .await;
        assert!(checked >= 6, "{checked} segments checked");
    }
}

/// `FFmpeg`'s HLS client plays an adaptive `AES-128` asset to the same frame count as its clear
/// twin, and plays nothing from the same stream behind another key.
#[tokio::test]
async fn ffmpeg_plays_an_adaptive_aes128_asset() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let data_uri = |key: &str| {
        let bytes = (0..16)
            .map(|at| u8::from_str_radix(&key[at * 2..at * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>();
        format!(
            "data:text/plain;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    };
    let renditions = [("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")];
    let h = harness().await;
    h.mapper.state.set(
        "aes",
        Answer::renditions("v1", &renditions).with_hls_aes128(AES_KEY, &data_uri(AES_KEY)),
    );
    h.mapper.state.set(
        "wrong",
        Answer::renditions("v1", &renditions)
            .with_hls_aes128(AES_KEY, &data_uri("00112233445566778899aabbccddeeff")),
    );
    h.mapper
        .state
        .set("clear", Answer::renditions("v1", &renditions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = h.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let frames = |path: String| {
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
                .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
                .args(["-allowed_extensions", "ALL"])
                .arg(format!("http://{address}{path}"))
                .output()
                .unwrap();
            (
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(',')
                    .to_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        })
    };
    let (expected, clear_errors) = frames("/hls/clear/master.m3u8".to_owned()).await.unwrap();
    assert_ne!(expected, "");
    let (played, errors) = frames("/hls/aes/master.m3u8".to_owned()).await.unwrap();
    // The clear twin has the same noise from probing the second variant; encryption adds none.
    // (FFmpeg prints a context pointer in each message, so those are masked.)
    let masked = |text: &str| {
        text.lines()
            .map(|line| line.split_once("] ").map_or(line, |(_, rest)| rest))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(played, expected);
    assert_eq!(masked(&errors), masked(&clear_errors));
    let (garbage, _) = frames("/hls/wrong/master.m3u8".to_owned()).await.unwrap();
    assert_ne!(garbage, expected, "a wrong key does not decrypt");
    server.abort();
}

// ---- Encrypted source files (TDD 0014) ----

/// The progressive fixtures protected by `tests/fixtures/protect-fixture.py`, one per scheme.
const PROTECTED_SOURCES: [(&str, &str); 4] = [
    ("cenc", "h264-aac-cenc.mp4"),
    ("cens", "h264-aac-cens.mp4"),
    ("cbc1", "h264-aac-cbc1.mp4"),
    ("cbcs", "h264-aac-cbcs.mp4"),
];

/// The key all the protected fixtures are encrypted under.
fn source_keys() -> serde_json::Value {
    serde_json::json!({ "keys": [{ "key_id": KEY_ID, "key": KEY }] })
}

/// `playlist` with every `?v=` URL version removed, so two assets' playlists can be compared.
fn without_version(playlist: &str) -> String {
    let mut out = String::new();
    let mut rest = playlist;
    while let Some(at) = rest.find("?v=") {
        out.push_str(&rest[..at]);
        rest = rest[at + 3..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

/// The media data of a fragment: what follows its `moof` and the header of its `mdat`.
fn mdat_payload(segment: &[u8]) -> Vec<u8> {
    let moof = u32::from_be_bytes(segment[..4].try_into().unwrap()) as usize;
    assert_eq!(&segment[4..8], b"moof");
    assert_eq!(&segment[moof + 4..moof + 8], b"mdat");
    segment[moof + 8..].to_vec()
}

/// Every init and media segment of `track`, in order, of the HLS asset `id` at `version`.
async fn track_segments(h: &Harness, id: &str, track: &str, version: &str) -> Vec<Bytes> {
    let mut segments = Vec::new();
    for uri in [format!("/hls/{id}/{track}/init.mp4?v={version}")] {
        segments.push(fetch(&h.app, &uri).await.2);
    }
    let playlist = text(
        &fetch(&h.app, &format!("/hls/{id}/{track}/index.m3u8"))
            .await
            .2,
    );
    for line in playlist
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let path = line.split('?').next().unwrap();
        let (code, _, bytes) =
            fetch(&h.app, &format!("/hls/{id}/{track}/{path}?v={version}")).await;
        assert_eq!(code, StatusCode::OK, "{id} {track} {path}");
        segments.push(bytes);
    }
    segments
}

/// For each of the four schemes, the protected file is served exactly as the clear file it was
/// made from: the same playlists, the same init segments, and the same bytes in every segment.
#[tokio::test]
async fn a_protected_source_is_served_exactly_as_its_clear_twin() {
    // A decrypted source is not muxed yet, so the clear twin is served without muxed audio too.
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac.mp4"));
    for (scheme, file) in PROTECTED_SOURCES {
        h.mapper.state.set(
            &format!("src-{scheme}"),
            Answer::file("v1", file).with_source_encryption(source_keys()),
        );
    }
    let clear_master = fetch(&h.app, "/hls/clear/master.m3u8").await.2;
    let clear_version = version_in(&clear_master);
    let clear_dash = text(&fetch(&h.app, "/dash/clear/manifest.mpd").await.2);

    for (scheme, _) in PROTECTED_SOURCES {
        let id = format!("src-{scheme}");
        let master = fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2;
        let version = version_in(&master);
        assert_ne!(
            version, clear_version,
            "{scheme}: a key is part of the version"
        );
        // The one thing the clear twin has more: an I-frame stream.
        let streams = |master: &Bytes| {
            without_version(&text(master))
                .lines()
                .filter(|line| !line.starts_with("#EXT-X-I-FRAME-STREAM-INF"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            streams(&master),
            streams(&clear_master),
            "{scheme}: master playlist"
        );
        let master_text = text(&master);
        assert!(!master_text.contains("EXT-X-KEY") && !master_text.contains("SESSION-KEY"));
        assert!(
            !master_text.contains("I-FRAME"),
            "{scheme}: no I-frame stream yet"
        );
        let dash = text(&fetch(&h.app, &format!("/dash/{id}/manifest.mpd")).await.2);
        assert_eq!(
            without_version(&dash),
            without_version(&clear_dash),
            "{scheme}: DASH"
        );
        assert!(!dash.contains("ContentProtection"), "{scheme}");

        for track in ["video", "audio-1"] {
            let playlist = |id: &str| {
                let h = &h;
                let uri = format!("/hls/{id}/{track}/index.m3u8");
                async move { without_version(&text(&fetch(&h.app, &uri).await.2)) }
            };
            assert_eq!(
                playlist(&id).await,
                playlist("clear").await,
                "{scheme} {track}: playlist"
            );
            let served = track_segments(&h, &id, track, &version).await;
            let clear = track_segments(&h, "clear", track, &clear_version).await;
            assert_eq!(served.len(), clear.len(), "{scheme} {track}");
            assert!(
                served.len() >= 4,
                "{scheme} {track}: an init segment and three segments"
            );
            for (index, (served, clear)) in served.iter().zip(&clear).enumerate() {
                assert_eq!(served, clear, "{scheme} {track} part {index}");
            }
        }
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/video/iframes.m3u8")).await,
            StatusCode::NOT_FOUND,
            "{scheme}"
        );
    }
}

/// Stage 2 of TDD 0014: a fragmented source keeps its IVs in a `senc` in every `traf`. Served
/// decrypted, it is the clear fragmented file, byte for byte, under every scheme.
#[tokio::test]
async fn a_protected_fragmented_source_is_served_exactly_as_its_clear_twin() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac-fragmented-cmaf.mp4"));
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    for (scheme, _) in PROTECTED_SOURCES {
        let id = format!("frag-{scheme}");
        h.mapper.state.set(
            &id,
            Answer::file("v1", &format!("h264-aac-fragmented-cmaf-{scheme}.mp4"))
                .with_source_encryption(source_keys()),
        );
        let version = version_in(&fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2);
        for track in ["video", "audio-1"] {
            let served = track_segments(&h, &id, track, &version).await;
            let clear = track_segments(&h, "clear", track, &clear_version).await;
            assert!(
                served.len() >= 4,
                "{scheme} {track}: an init segment and the fragments"
            );
            assert_eq!(served, clear, "{scheme} {track}");
        }
    }
}

/// An encoder's own fragmented file, video only: every fragment's `senc` lists that fragment's
/// samples, and the samples are the clear file's.
#[tokio::test]
async fn a_real_encoders_fragmented_cenc_video_is_decrypted() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac.mp4"));
    h.mapper.state.set(
        "real",
        Answer::file("v1", "h264-video-fragmented-cenc-ffmpeg.mp4")
            .with_source_encryption(source_keys()),
    );
    let version = version_in(&fetch(&h.app, "/hls/real/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    let payload = |parts: &[Bytes]| -> Vec<u8> {
        parts[1..]
            .iter()
            .flat_map(|part| mdat_payload(part))
            .collect()
    };
    let served = track_segments(&h, "real", "video", &version).await;
    let clear = track_segments(&h, "clear", "video", &clear_version).await;
    assert_eq!(
        payload(&served),
        payload(&clear),
        "every video sample, decrypted"
    );
}

/// The same encoder's file with audio: from the second fragment on, each audio `senc` lists every
/// sample so far, not the fragment's own. Guessing which entries are the fragment's would decrypt
/// with the wrong IVs, so the file is refused (and the load fails, not the one track).
#[tokio::test]
async fn a_senc_that_lists_the_samples_of_every_earlier_fragment_is_refused() {
    let h = harness().await;
    h.mapper.state.set(
        "real",
        Answer::file("v1", "h264-aac-fragmented-cenc-ffmpeg.mp4")
            .with_source_encryption(source_keys()),
    );
    assert_eq!(
        status(&h.app, "/hls/real/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

/// A file with edit lists drops its leading audio samples and shifts its timeline, and the
/// per-sample IVs and subsample maps must follow the samples that remain.
#[tokio::test]
async fn edit_lists_do_not_misalign_the_ivs_of_the_samples_that_remain() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac-default-edits.mp4"));
    h.mapper.state.set(
        "edits",
        Answer::file("v1", "h264-aac-default-edits-cenc.mp4").with_source_encryption(source_keys()),
    );
    let version = version_in(&fetch(&h.app, "/hls/edits/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    for track in ["video", "audio-1"] {
        let served = track_segments(&h, "edits", track, &version).await;
        let clear = track_segments(&h, "clear", track, &clear_version).await;
        assert_eq!(served, clear, "{track}");
    }
}

/// HEVC hides its codec string in the `hvcC` of an entry that `sinf` renamed: the playlists must
/// still say `hvc1`/`hev1` with the profile and level, as the clear file's do, and the segments
/// must be the clear file's.
#[tokio::test]
async fn a_protected_hevc_source_is_served_as_the_clear_twin_with_the_same_codec_string() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "hevc-aac.mp4"));
    h.mapper.state.set(
        "src",
        Answer::file("v1", "hevc-aac-cenc.mp4").with_source_encryption(source_keys()),
    );
    let master = text(&fetch(&h.app, "/hls/src/master.m3u8").await.2);
    let clear_master = text(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    assert!(!master.contains("encv"), "{master}");
    let codecs = |master: &str| {
        master
            .lines()
            // The clear twin has an I-frame stream too; a decrypted source does not yet.
            .filter(|line| !line.starts_with("#EXT-X-I-FRAME-STREAM-INF"))
            .filter_map(|line| line.split("CODECS=\"").nth(1))
            .map(|rest| rest.split('"').next().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(codecs(&master), codecs(&clear_master));
    assert_ne!(codecs(&master), Vec::<String>::new());
    let dash = text(&fetch(&h.app, "/dash/src/manifest.mpd").await.2);
    assert!(!dash.contains("encv"), "{dash}");

    let version = version_in(&fetch(&h.app, "/hls/src/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    for track in ["video", "audio-1"] {
        let served = track_segments(&h, "src", track, &version).await;
        let clear = track_segments(&h, "clear", track, &clear_version).await;
        assert_eq!(served, clear, "{track}");
    }
}

/// A real encoder's file: its `senc`, `saio`, and `saiz`, its own timeline and edit lists. Its
/// samples are the clear file's, so the media data served is the clear twin's, and `FFmpeg`
/// decodes a segment with no key.
#[tokio::test]
async fn a_real_encoders_cenc_file_is_decrypted() {
    let h = harness().await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac.mp4"));
    h.mapper.state.set(
        "real",
        Answer::file("v1", "h264-aac-cenc-ffmpeg.mp4").with_source_encryption(source_keys()),
    );
    let version = version_in(&fetch(&h.app, "/hls/real/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    for track in ["video", "audio-1"] {
        let served = track_segments(&h, "real", track, &version).await;
        let clear = track_segments(&h, "clear", track, &clear_version).await;
        let payload = |parts: &[Bytes]| -> Vec<u8> {
            parts[1..]
                .iter()
                .flat_map(|part| mdat_payload(part))
                .collect()
        };
        assert_eq!(
            payload(&served),
            payload(&clear),
            "{track}: every sample, decrypted"
        );
    }
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping the FFmpeg decode: ffmpeg is not installed");
        return;
    }
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/encrypted-source");
    std::fs::create_dir_all(&directory).unwrap();
    let mut bytes = fetch(&h.app, &format!("/hls/real/video/init.mp4?v={version}"))
        .await
        .2
        .to_vec();
    bytes.extend_from_slice(
        &fetch(
            &h.app,
            &format!("/hls/real/video/segments/1/media.m4s?v={version}"),
        )
        .await
        .2,
    );
    let path = directory.join("real-video-segment-1.mp4");
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(decode_errors(&path, None), "", "decodes with no key");
}

/// Decrypting and then encrypting under a new key is what encrypting the clear file under that
/// key gives, byte for byte: so the decrypted samples are right and the new init segment replaces
/// the source's `sinf`.
#[tokio::test]
async fn a_decrypted_source_can_be_encrypted_again() {
    let h = harness().await;
    let output =
        serde_json::json!({ "scheme": "cbcs", "keys": [{ "key_id": KEY_ID_B, "key": KEY_B }] });
    h.mapper.state.set(
        "twin",
        Answer::file("v1", "h264-aac.mp4").with_encryption(output.clone()),
    );
    h.mapper.state.set(
        "aes-twin",
        Answer::file("v1", "h264-aac.mp4").with_hls_aes128(AES_KEY, AES_KEY_URI),
    );
    let twin_version = version_in(&fetch(&h.app, "/hls/twin/master.m3u8").await.2);
    let aes_version = version_in(&fetch(&h.app, "/hls/aes-twin/master.m3u8").await.2);
    for (scheme, file) in PROTECTED_SOURCES {
        let id = format!("again-{scheme}");
        h.mapper.state.set(
            &id,
            Answer::file("v1", file)
                .with_source_encryption(source_keys())
                .with_encryption(output.clone()),
        );
        let aes_id = format!("aes-{scheme}");
        h.mapper.state.set(
            &aes_id,
            Answer::file("v1", file)
                .with_source_encryption(source_keys())
                .with_hls_aes128(AES_KEY, AES_KEY_URI),
        );
        let version = version_in(&fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2);
        let aes = version_in(&fetch(&h.app, &format!("/hls/{aes_id}/master.m3u8")).await.2);
        for track in ["video", "audio-1"] {
            let served = track_segments(&h, &id, track, &version).await;
            let twin = track_segments(&h, "twin", track, &twin_version).await;
            assert_eq!(served, twin, "{scheme} {track}: cbcs under a new key");
            if track == "video" {
                // The source's own `sinf` is replaced, not kept beside the new one.
                let sinf = served[0].windows(4).filter(|w| *w == b"sinf").count();
                assert_eq!(sinf, 1, "{scheme}");
                assert!(
                    !served[0]
                        .windows(4)
                        .any(|w| w == b"cenc" || w == b"cens" || w == b"cbc1")
                );
            }
            let served = track_segments(&h, &aes_id, track, &aes).await;
            let twin = track_segments(&h, "aes-twin", track, &aes_version).await;
            assert_eq!(served, twin, "{scheme} {track}: whole-segment AES-128");
        }
    }
}

#[tokio::test]
async fn a_protected_source_that_cannot_be_decrypted_fails_to_load_without_naming_a_key() {
    let h = harness().await;
    let flipped = KEY.replacen("00", "01", 1);
    let cases = [
        ("no-keys", Answer::file("v1", "h264-aac-cenc.mp4")),
        (
            "other-key-id",
            Answer::file("v1", "h264-aac-cenc.mp4").with_source_encryption(
                serde_json::json!({ "keys": [{ "key_id": KEY_ID_B, "key": KEY }] }),
            ),
        ),
        (
            "wrong-key",
            Answer::file("v1", "h264-aac-cenc-whole-video.mp4").with_source_encryption(
                serde_json::json!({ "keys": [{ "key_id": KEY_ID, "key": flipped }] }),
            ),
        ),
    ];
    for (id, answer) in cases {
        h.mapper.state.set(id, answer);
        for uri in [
            format!("/hls/{id}/master.m3u8"),
            format!("/dash/{id}/manifest.mpd"),
        ] {
            let (code, _, body) = fetch(&h.app, &uri).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
            let body = text(&body);
            assert!(
                !body.contains(KEY) && !body.contains(&flipped),
                "{uri}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn admin_status_reports_source_key_ids_never_keys() {
    let h = harness().await;
    h.mapper.state.set(
        "src",
        Answer::file("v1", "h264-aac-cenc.mp4").with_source_encryption(source_keys()),
    );
    h.mapper
        .state
        .set("plain", Answer::file("v1", "h264-aac.mp4"));
    assert_eq!(status(&h.app, "/hls/src/master.m3u8").await, StatusCode::OK);
    assert_eq!(
        status(&h.app, "/hls/plain/master.m3u8").await,
        StatusCode::OK
    );

    let body = fetch(&h.app, "/admin/status").await.2;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let assets = json["cache"]["assets"].as_array().unwrap();
    let find = |id: &str| assets.iter().find(|asset| asset["asset_id"] == id).unwrap();
    assert_eq!(
        find("src")["source_key_ids"],
        serde_json::json!(["01234567-89ab-cdef-0123-456789abcdef"])
    );
    assert_eq!(find("src")["encrypted"], false, "it is served clear");
    assert_eq!(find("plain")["source_key_ids"], serde_json::json!([]));
    assert!(!String::from_utf8_lossy(&body).contains(KEY));
}

#[tokio::test]
async fn source_encryption_is_refused_where_it_is_not_supported_yet() {
    let h = harness().await;
    let renditions = Answer::renditions(
        "v1",
        &[("a", "h264-aac-cenc.mp4"), ("b", "h264-aac-cenc.mp4")],
    )
    .with_source_encryption(source_keys());
    h.mapper.state.set("renditions", renditions);
    h.mapper.state.set(
        "clips",
        Answer::clips("v1", &[clip("h264-aac-cenc.mp4", None, None)])
            .with_source_encryption(source_keys()),
    );
    h.mapper.state.set(
        "passthrough",
        Answer::file("v1", "h264-aac-cenc.mp4")
            .with_source_encryption(serde_json::json!({ "passthrough": true })),
    );
    h.mapper.state.set(
        "empty",
        Answer::file("v1", "h264-aac.mp4").with_source_encryption(serde_json::json!({})),
    );
    for id in ["renditions", "clips", "passthrough", "empty"] {
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
        );
    }
}

#[tokio::test]
async fn re_keying_the_source_gives_new_urls_and_the_order_of_keys_does_not() {
    let h = harness().await;
    let other = serde_json::json!({ "key_id": KEY_ID_B, "key": KEY_B });
    let right = serde_json::json!({ "key_id": KEY_ID, "key": KEY });
    let answers = [
        ("one", vec![right.clone()]),
        ("two", vec![right.clone(), other.clone()]),
        ("two-reordered", vec![other, right]),
    ];
    let mut versions = Vec::new();
    for (id, keys) in answers {
        h.mapper.state.set(
            id,
            Answer::file("v1", "h264-aac-cenc.mp4")
                .with_source_encryption(serde_json::json!({ "keys": keys })),
        );
        versions.push(version_in(
            &fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2,
        ));
    }
    assert_ne!(
        versions[0], versions[1],
        "another key entry is a different answer"
    );
    assert_eq!(versions[1], versions[2], "the order of the entries is not");
}

/// `HEAD`, a byte range, and the `ETag` on a decrypted segment: it is read and decrypted to learn
/// its length, then answered like any other segment.
#[tokio::test]
async fn a_decrypted_segment_answers_head_ranges_and_etags() {
    let h = harness().await;
    h.mapper.state.set(
        "src",
        Answer::file("v1", "h264-aac-cbcs.mp4").with_source_encryption(source_keys()),
    );
    let version = version_in(&fetch(&h.app, "/hls/src/master.m3u8").await.2);
    let uri = format!("/hls/src/video/segments/1/media.m4s?v={version}");
    let (code, headers, whole) = fetch(&h.app, &uri).await;
    assert_eq!(code, StatusCode::OK);
    let etag = headers.get("etag").expect("an ETag").clone();

    let head = h
        .app
        .clone()
        .oneshot(Request::head(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], whole.len().to_string());

    let ranged = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header("range", "bytes=100-299")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    let ranged = to_bytes(ranged.into_body(), usize::MAX).await.unwrap();
    assert_eq!(ranged[..], whole[100..300]);

    let conditional = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
}

// ---- Track selection (TDD 0015) ----

/// The `EXT-X-MEDIA` audio lines of a master playlist.
fn audio_lines(master: &str) -> Vec<String> {
    master
        .lines()
        .filter(|line| line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO"))
        .map(str::to_owned)
        .collect()
}

/// `h264-aac-two-audio.mp4` has a video track, then English audio, then Spanish audio.
/// `tracks` picks among them with nginx-vod-module's syntax; what is picked is served exactly as
/// the same track is in the whole file, and the audio left is numbered `audio-1`.
#[tokio::test]
async fn tracks_choose_which_tracks_of_a_file_are_served() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("full", Answer::file("v1", "h264-aac-two-audio.mp4"));
    for (id, tracks) in [
        ("first", "v1-a1"),
        ("second", "v1-a2"),
        ("video", "v1"),
        ("backwards", "a2-v1"),
        ("all", "v0-a0"),
        ("missing", "v1-a9"),
    ] {
        h.mapper.state.set(
            id,
            Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks(tracks),
        );
    }
    let version_of = |id: &'static str| {
        let h = &h;
        async move { version_in(&fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2) }
    };
    let master_of = |id: &'static str| {
        let h = &h;
        async move { text(&fetch(&h.app, &format!("/hls/{id}/master.m3u8")).await.2) }
    };

    let full_master = master_of("full").await;
    assert_eq!(audio_lines(&full_master).len(), 2, "{full_master}");
    let (full_version, second_version) = (version_of("full").await, version_of("second").await);
    assert_ne!(
        full_version, second_version,
        "a selection is part of the version"
    );

    // The second audio track, alone, is the shared audio group's only member.
    let second_master = master_of("second").await;
    let lines = audio_lines(&second_master);
    assert_eq!(lines.len(), 1, "{second_master}");
    assert!(
        lines[0].contains("LANGUAGE=\"spa\"") && lines[0].contains("audio-1/"),
        "{lines:?}"
    );
    let full = |track: &'static str| track_segments(&h, "full", track, &full_version);
    let second = |track: &'static str| track_segments(&h, "second", track, &second_version);
    assert_eq!(
        second("video").await,
        full("video").await,
        "the video is untouched"
    );
    assert_eq!(
        second("audio-1").await,
        full("audio-2").await,
        "the Spanish track, byte for byte, now called audio-1"
    );

    // The first audio track. Each selection is other media than the others, not only than the
    // whole file.
    let first_version = version_of("first").await;
    assert_ne!(first_version, second_version);
    assert_ne!(first_version, full_version);
    assert_ne!(version_of("video").await, first_version);
    let first = track_segments(&h, "first", "audio-1", &first_version).await;
    assert_eq!(first, full("audio-1").await);

    // Video only: no audio at all.
    let video_master = master_of("video").await;
    assert_eq!(
        audio_lines(&video_master),
        Vec::<String>::new(),
        "{video_master}"
    );

    // The spelling does not matter, and v0-a0 is the whole file.
    assert_eq!(version_of("backwards").await, second_version);
    assert_eq!(audio_lines(&master_of("all").await).len(), 2);

    // A selector that matches no audio still serves the video.
    assert_eq!(
        audio_lines(&master_of("missing").await),
        Vec::<String>::new()
    );
}

/// The DASH manifest follows the selection too.
#[tokio::test]
async fn the_dash_manifest_lists_only_the_selected_tracks() {
    let h = harness().await;
    h.mapper.state.set(
        "second",
        Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks("v1-a2"),
    );
    let manifest = text(&fetch(&h.app, "/dash/second/manifest.mpd").await.2);
    assert_eq!(
        manifest.matches("contentType=\"audio\"").count(),
        1,
        "{manifest}"
    );
    assert!(
        manifest.contains("lang=\"spa\"") && !manifest.contains("lang=\"eng\""),
        "{manifest}"
    );
    assert_eq!(
        manifest.matches("contentType=\"video\"").count(),
        1,
        "{manifest}"
    );
}

#[tokio::test]
async fn a_selection_that_leaves_nothing_fails_the_asset_and_a_bad_one_is_a_bad_answer() {
    let h = harness().await;
    h.mapper.state.set(
        "nothing",
        Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks("a9"),
    );
    assert_eq!(
        status(&h.app, "/hls/nothing/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    for (id, tracks) in [
        ("empty", ""),
        ("word", "video"),
        ("twice", "a1-a2"),
        ("big", "a999"),
    ] {
        h.mapper.state.set(
            id,
            Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks(tracks),
        );
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
        );
    }
    // On the answer as a whole, tracks belongs to a single file, not to renditions or clips.
    h.mapper.state.set(
        "renditions",
        Answer::renditions(
            "v1",
            &[("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")],
        )
        .with_tracks("v1"),
    );
    assert_eq!(
        status(&h.app, "/hls/renditions/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

/// Changing the selection under an unchanged mapper `version` reloads the asset, as re-keying does.
#[tokio::test]
async fn changing_the_selection_under_the_same_version_reloads_the_asset() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    let answer = |tracks: &str| {
        let mut answer = Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks(tracks);
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    h.mapper.state.set("movie", answer("v1-a1"));
    let before = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    assert!(before.contains("LANGUAGE=\"eng\""), "{before}");

    h.mapper.state.set("movie", answer("v1-a2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);

    assert!(
        after.contains("LANGUAGE=\"spa\"") && !after.contains("LANGUAGE=\"eng\""),
        "{after}"
    );
    assert!(
        metric(&h.state, "vod_asset_loads_total{outcome=\"ok\"} 2"),
        "reloaded"
    );
}

/// A selection applies to a protected source too: the track it drops is not served, and the one
/// it keeps is decrypted.
#[tokio::test]
async fn a_selection_is_applied_before_a_protected_source_is_decrypted() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("clear", Answer::file("v1", "h264-aac.mp4"));
    h.mapper.state.set(
        "src",
        Answer::file("v1", "h264-aac-cenc.mp4")
            .with_source_encryption(source_keys())
            .with_tracks("v1"),
    );
    let version = version_in(&fetch(&h.app, "/hls/src/master.m3u8").await.2);
    let clear_version = version_in(&fetch(&h.app, "/hls/clear/master.m3u8").await.2);
    assert_eq!(
        audio_lines(&text(&fetch(&h.app, "/hls/src/master.m3u8").await.2)),
        Vec::<String>::new()
    );
    assert_eq!(
        track_segments(&h, "src", "video", &version).await,
        track_segments(&h, "clear", "video", &clear_version).await
    );
}

/// A rendition can serve only some tracks of its file, as the answer as a whole can.
#[tokio::test]
async fn a_rendition_can_serve_only_some_tracks_of_its_file() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper
        .state
        .set("full", Answer::file("v1", "h264-aac-two-audio.mp4"));
    h.mapper.state.set(
        "ladder",
        Answer::renditions("v1", &[("main", "h264-aac-two-audio.mp4")]).with_rendition_field(
            "main",
            "tracks",
            serde_json::json!("v1-a2"),
        ),
    );
    let full_version = version_in(&fetch(&h.app, "/hls/full/master.m3u8").await.2);
    let master = text(&fetch(&h.app, "/hls/ladder/master.m3u8").await.2);
    let version = version_in(&master.clone().into());
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 1, "{master}");
    assert!(lines[0].contains("LANGUAGE=\"spa\""), "{lines:?}");

    assert_eq!(
        track_segments(&h, "ladder", "video-main", &version).await,
        track_segments(&h, "full", "video", &full_version).await
    );
    assert_eq!(
        track_segments(&h, "ladder", "audio-1", &version).await,
        track_segments(&h, "full", "audio-2", &full_version).await,
        "the Spanish track of the file, now the group's only audio"
    );
}

/// The usual ladder: video from one file, and the audio, alone, from a track of another.
#[tokio::test]
async fn one_audio_track_of_a_file_can_be_a_renditions_whole_contribution() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("es", "h264-aac-two-audio.mp4"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("es", "tracks", serde_json::json!("a2")),
    );
    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 1, "{master}");
    assert!(lines[0].contains("LANGUAGE=\"spa\""), "{lines:?}");
    let version = version_in(&master.into());
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/movie/audio-1/segments/0/media.m4s?v={version}")
        )
        .await,
        StatusCode::OK
    );
    let dash = text(&fetch(&h.app, "/dash/movie/manifest.mpd").await.2);
    assert!(
        dash.contains("lang=\"spa\"") && !dash.contains("lang=\"eng\""),
        "{dash}"
    );
}

#[tokio::test]
async fn a_renditions_bad_selection_names_the_rendition() {
    let h = harness().await;
    h.mapper.state.set(
        "nothing",
        Answer::renditions(
            "v1",
            &[("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")],
        )
        .with_rendition_field("b", "tracks", serde_json::json!("a9")),
    );
    assert_eq!(
        status(&h.app, "/hls/nothing/master.m3u8").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    h.mapper.state.set(
        "malformed",
        Answer::renditions("v1", &[("a", "rendition-480p.mp4")]).with_rendition_field(
            "a",
            "tracks",
            serde_json::json!("x1"),
        ),
    );
    assert_eq!(
        status(&h.app, "/hls/malformed/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

/// A clip can pick tracks too. One clip of the whole file is the file served alone.
#[tokio::test]
async fn a_clip_can_serve_only_some_tracks_of_its_file() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "second",
        Answer::file("v1", "h264-aac-two-audio.mp4").with_tracks("v1-a2"),
    );
    h.mapper.state.set(
        "one",
        Answer::clips("v1", &[clip("h264-aac-two-audio.mp4", None, None)]).with_clip_field(
            0,
            "tracks",
            serde_json::json!("v1-a2"),
        ),
    );
    h.mapper.state.set(
        "two",
        Answer::clips(
            "v1",
            &[
                clip("h264-aac-two-audio.mp4", Some(0), Some(1000)),
                clip("h264-aac-two-audio.mp4", Some(1000), Some(2000)),
            ],
        )
        .with_clip_field(0, "tracks", serde_json::json!("v1-a2"))
        .with_clip_field(1, "tracks", serde_json::json!("v1-a2")),
    );
    let second_version = version_in(&fetch(&h.app, "/hls/second/master.m3u8").await.2);
    let one_version = version_in(&fetch(&h.app, "/hls/one/master.m3u8").await.2);
    for track in ["video", "audio-1"] {
        assert_eq!(
            track_segments(&h, "one", track, &one_version).await[1..],
            track_segments(&h, "second", track, &second_version).await[1..],
            "{track}: the same media segments"
        );
    }
    let master = text(&fetch(&h.app, "/hls/two/master.m3u8").await.2);
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 1, "{master}");
    assert!(lines[0].contains("LANGUAGE=\"spa\""), "{lines:?}");
}

#[tokio::test]
async fn changing_a_members_selection_under_the_same_version_reloads_the_asset() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    let ladder = |tracks: &str| {
        let mut answer = Answer::renditions("v1", &[("main", "h264-aac-two-audio.mp4")])
            .with_rendition_field("main", "tracks", serde_json::json!(tracks));
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.set("ladder", ladder("v1-a1"));
    let before = text(&fetch(&h.app, "/hls/ladder/master.m3u8").await.2);
    assert!(before.contains("LANGUAGE=\"eng\""), "{before}");
    h.mapper.state.set("ladder", ladder("v1-a2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after = text(&fetch(&h.app, "/hls/ladder/master.m3u8").await.2);
    assert!(
        after.contains("LANGUAGE=\"spa\"") && !after.contains("LANGUAGE=\"eng\""),
        "{after}"
    );

    let sequence = |tracks: &str| {
        let mut answer = Answer::clips("v1", &[clip("h264-aac-two-audio.mp4", None, None)])
            .with_clip_field(0, "tracks", serde_json::json!(tracks));
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.set("seq", sequence("v1-a1"));
    let before = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);
    h.mapper.state.set("seq", sequence("v1-a2"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after = version_in(&fetch(&h.app, "/hls/seq/master.m3u8").await.2);
    assert_ne!(before, after, "another selection, other URLs");
}

// ---- Rendition language, label and default (TDD 0015) ----

/// A ladder with a video rendition and two dedicated audio renditions, the second named in
/// Spanish and made the default.
fn named_audio_ladder() -> Answer {
    Answer::renditions(
        "v1",
        &[
            ("hi", "rendition-720p.mp4"),
            ("en", "rendition-audio-en.m4a"),
            ("es", "rendition-audio-es.m4a"),
        ],
    )
    .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
    .with_rendition_field("en", "language", serde_json::json!("eng"))
    .with_rendition_field("en", "label", serde_json::json!("English"))
    .with_rendition_field("es", "language", serde_json::json!("spa"))
    .with_rendition_field("es", "label", serde_json::json!("Español"))
    .with_rendition_field("es", "default", serde_json::json!(true))
}

#[tokio::test]
async fn a_renditions_audio_can_be_named_and_made_the_default() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set("movie", named_audio_ladder());
    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 2, "{master}");
    // The order of the group is the order of the renditions; the default is the flagged one.
    assert!(
        lines[0].contains("NAME=\"English\"")
            && lines[0].contains("LANGUAGE=\"eng\"")
            && lines[0].contains("DEFAULT=NO"),
        "{lines:?}"
    );
    assert!(
        lines[1].contains("NAME=\"Español\"")
            && lines[1].contains("LANGUAGE=\"spa\"")
            && lines[1].contains("DEFAULT=YES"),
        "{lines:?}"
    );
    let dash = text(&fetch(&h.app, "/dash/movie/manifest.mpd").await.2);
    assert!(
        dash.contains("<Label>English</Label>") && dash.contains("<Label>Español</Label>"),
        "{dash}"
    );
    assert!(
        dash.contains("lang=\"eng\"") && dash.contains("lang=\"spa\""),
        "{dash}"
    );
}

/// Without a flag the first audio is the default, as it always was.
#[tokio::test]
async fn the_first_audio_is_the_default_when_none_is_flagged() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
                ("es", "rendition-audio-es.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("es", "label", serde_json::json!("Español")),
    );
    let lines = audio_lines(&text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2));
    assert!(lines[0].contains("DEFAULT=YES"), "{lines:?}");
    assert!(
        lines[1].contains("DEFAULT=NO") && lines[1].contains("NAME=\"Español\""),
        "{lines:?}"
    );
}

/// A language replaces what the file says, for the audio a video rendition brings with it.
#[tokio::test]
async fn a_language_replaces_the_one_in_the_file() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "movie",
        Answer::renditions("v1", &[("main", "h264-aac-two-audio.mp4")])
            .with_rendition_field("main", "tracks", serde_json::json!("v1-a1"))
            .with_rendition_field("main", "language", serde_json::json!("fra")),
    );
    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 1, "{master}");
    assert!(
        lines[0].contains("LANGUAGE=\"fra\"") && !lines[0].contains("eng"),
        "{lines:?}"
    );
}

/// Quotes and markup in a label cannot break out of the playlist or the manifest.
#[tokio::test]
async fn a_label_is_escaped_where_it_is_written() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "movie",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!("R&D \"cut\" <1>")),
    );
    let master = text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2);
    assert!(master.contains("NAME=\"R&D 'cut' <1>\""), "{master}");
    let dash = text(&fetch(&h.app, "/dash/movie/manifest.mpd").await.2);
    assert!(
        dash.contains("<Label>R&amp;D &quot;cut&quot; &lt;1&gt;</Label>"),
        "{dash}"
    );
}

/// A name changes the playlists and nothing in the media, but it is other output, so other URLs.
#[tokio::test]
async fn overrides_change_the_playlists_and_the_version_and_never_the_media() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "plain",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1")),
    );
    h.mapper.state.set(
        "named",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!("English")),
    );
    h.mapper.state.set(
        "renamed",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!("Anglais")),
    );
    h.mapper.state.set(
        "flagged",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!("English"))
        .with_rendition_field("en", "default", serde_json::json!(true)),
    );
    h.mapper.state.set(
        "spoken",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!("English"))
        .with_rendition_field("en", "language", serde_json::json!("fra")),
    );
    let plain = version_in(&fetch(&h.app, "/hls/plain/master.m3u8").await.2);
    let spoken = version_in(&fetch(&h.app, "/hls/spoken/master.m3u8").await.2);
    let named = version_in(&fetch(&h.app, "/hls/named/master.m3u8").await.2);
    assert_ne!(named, spoken, "only the language differs");
    let renamed = version_in(&fetch(&h.app, "/hls/renamed/master.m3u8").await.2);
    let flagged = version_in(&fetch(&h.app, "/hls/flagged/master.m3u8").await.2);
    assert_ne!(plain, named);
    assert_ne!(named, renamed);
    assert_ne!(named, flagged, "only the default flag differs");
    for track in ["video-hi", "audio-1"] {
        assert_eq!(
            track_segments(&h, "named", track, &named).await,
            track_segments(&h, "plain", track, &plain).await,
            "{track}"
        );
    }
}

#[tokio::test]
async fn bad_overrides_are_a_bad_answer() {
    let h = harness().await;
    let cases = [
        ("short", "language", serde_json::json!("en")),
        ("upper", "language", serde_json::json!("ENG")),
        ("long", "language", serde_json::json!("english")),
        ("digit", "language", serde_json::json!("en1")),
        ("empty", "label", serde_json::json!("")),
        ("huge", "label", serde_json::json!("x".repeat(129))),
        ("control", "label", serde_json::json!("a\nb")),
        ("kind", "default", serde_json::json!("yes")),
    ];
    for (id, field, value) in cases {
        h.mapper.state.set(
            id,
            Answer::renditions(
                "v1",
                &[
                    ("hi", "rendition-720p.mp4"),
                    ("en", "rendition-audio-en.m4a"),
                ],
            )
            .with_rendition_field("en", field, value),
        );
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
        );
    }
    // Two defaults cannot both be the default.
    h.mapper.state.set(
        "two",
        Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
                ("es", "rendition-audio-es.m4a"),
            ],
        )
        .with_rendition_field("en", "default", serde_json::json!(true))
        .with_rendition_field("es", "default", serde_json::json!(true)),
    );
    assert_eq!(
        status(&h.app, "/hls/two/master.m3u8").await,
        StatusCode::BAD_GATEWAY
    );
}

/// An override needs exactly one audio track to apply to.
#[tokio::test]
async fn an_override_with_no_audio_track_or_several_fails_the_asset() {
    let h = harness().await;
    h.mapper.state.set(
        "none",
        Answer::renditions("v1", &[("hi", "rendition-720p.mp4")])
            .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
            .with_rendition_field("hi", "language", serde_json::json!("eng")),
    );
    h.mapper.state.set(
        "several",
        Answer::renditions("v1", &[("main", "h264-aac-two-audio.mp4")])
            .with_rendition_field("main", "tracks", serde_json::json!("v1-a0"))
            .with_rendition_field("main", "label", serde_json::json!("Movie")),
    );
    for id in ["none", "several"] {
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{id}"
        );
    }
}

#[tokio::test]
async fn changing_an_override_under_the_same_version_reloads_the_asset() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.always_full.store(true, Ordering::SeqCst);
    let ladder = |label: &str| {
        let mut answer = Answer::renditions(
            "v1",
            &[
                ("hi", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
            ],
        )
        .with_rendition_field("hi", "tracks", serde_json::json!("v1"))
        .with_rendition_field("en", "label", serde_json::json!(label));
        answer.ttl_seconds = Some(0);
        answer
    };
    h.mapper.state.set("movie", ladder("English"));
    assert!(text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2).contains("NAME=\"English\""));
    h.mapper.state.set("movie", ladder("Anglais"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(text(&fetch(&h.app, "/hls/movie/master.m3u8").await.2).contains("NAME=\"Anglais\""));
}

/// A clip is cut on the tracks it serves, not on the ones it drops: a second audio track that
/// ends after a second must not stop a window at 2.2 seconds, which the video and the first
/// audio cover.
#[tokio::test]
async fn a_clip_is_cut_on_the_tracks_it_serves() {
    let h = harness_with(|config| config.hls_mux_audio = false).await;
    h.mapper.state.set(
        "late",
        Answer::clips(
            "v1",
            &[clip(
                "h264-aac-short-second-audio.mp4",
                Some(2200),
                Some(2900),
            )],
        )
        .with_clip_field(0, "tracks", serde_json::json!("v1-a1")),
    );
    let (code, _, master) = fetch(&h.app, "/hls/late/master.m3u8").await;
    assert_eq!(code, StatusCode::OK, "{}", text(&master));
    let master = text(&master);
    let lines = audio_lines(&master);
    assert_eq!(lines.len(), 1, "{master}");
    assert!(lines[0].contains("LANGUAGE=\"eng\""), "{lines:?}");
    let version = version_in(&master.into());
    assert_eq!(
        status(
            &h.app,
            &format!("/hls/late/audio-1/segments/0/media.m4s?v={version}")
        )
        .await,
        StatusCode::OK
    );
}

/// An XML 1.0 non-character cannot be written into the manifest, so it cannot be in a label.
#[tokio::test]
async fn a_label_that_cannot_be_written_into_xml_is_a_bad_answer() {
    let h = harness().await;
    for (id, label) in [("fffe", "a\u{fffe}b"), ("ffff", "a\u{ffff}b")] {
        h.mapper.state.set(
            id,
            Answer::renditions(
                "v1",
                &[
                    ("hi", "rendition-720p.mp4"),
                    ("en", "rendition-audio-en.m4a"),
                ],
            )
            .with_rendition_field("en", "label", serde_json::json!(label)),
        );
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
        );
    }
}

/// The audio group is numbered `audio-1`, `audio-2`, ... across the audio renditions, but each
/// audio rendition holds its track under its own key, so the init segment of the second one has to
/// be looked up under the key it has inside, as its segments are.
#[tokio::test]
async fn the_init_segment_of_a_second_audio_rendition_is_served() {
    let h = harness().await;
    h.mapper.state.set(
        "languages",
        Answer::renditions(
            "v1",
            &[
                ("720p", "rendition-720p.mp4"),
                ("en", "rendition-audio-en.m4a"),
                ("es", "rendition-audio-es.m4a"),
            ],
        ),
    );
    let version = version_in(&fetch(&h.app, "/hls/languages/master.m3u8").await.2);

    for protocol in ["hls", "dash"] {
        for audio in ["audio-1", "audio-2"] {
            let init = fetch(
                &h.app,
                &format!("/{protocol}/languages/{audio}/init.mp4?v={version}"),
            )
            .await;
            assert_eq!(init.0, StatusCode::OK, "{protocol} {audio} init");
            assert_eq!(&init.2[4..8], b"ftyp", "{protocol} {audio} init");
        }
        let segment = status(
            &h.app,
            &format!("/{protocol}/languages/audio-2/segments/0/media.m4s?v={version}"),
        )
        .await;
        assert_eq!(segment, StatusCode::OK, "{protocol} audio-2 segment");
    }
}
