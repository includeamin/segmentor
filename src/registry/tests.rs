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
            "renditions",
            Answer::renditions(
                "v1",
                &[("a", "rendition-480p.mp4"), ("b", "rendition-720p.mp4")],
            )
            .with_hls_aes128(AES_KEY, AES_KEY_URI),
        ),
        (
            "clips",
            Answer::clips("v1", &[clip("h264-aac.mp4", None, None)])
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
        "adaptive",
        Answer::renditions(
            "v1",
            &[("720p", "rendition-720p.mp4"), ("360p", "h264-aac.mp4")],
        )
        .with_encryption(rotating_encryption()),
    );
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
    for id in ["adaptive", "seq"] {
        assert_eq!(
            status(&h.app, &format!("/hls/{id}/master.m3u8")).await,
            StatusCode::BAD_GATEWAY,
            "{id}"
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
