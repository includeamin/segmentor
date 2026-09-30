//! Introspection for a control panel: what the resolver sees and what the cache holds.
//!
//! Unlike the other endpoints, this one has no configuration-file switch of its own; it carries
//! the same trust model as `/metrics`, and `docs/operations.md` says to put both behind a
//! reverse proxy rather than expose them to viewers.
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::http::state::AppState;
use crate::registry::RegistryStatus;

#[derive(Serialize)]
struct StatusResponse {
    resolver: ResolverJson,
    cache: CacheJson,
}

#[derive(Serialize)]
struct ResolverJson {
    /// `"static"` (the configuration-file catalog) or `"mapper"`.
    kind: &'static str,
    /// Checked right now, not read from the background probe: accurate even when
    /// `readiness_probe_interval_ms` is `0`.
    healthy: bool,
    base_url: Option<String>,
    /// Asset IDs to offer: the static catalog's, or what a mapper lists at its optional
    /// `GET /v1/assets`; `null` when there is no list.
    known_assets: Option<Vec<String>>,
}

#[derive(Serialize)]
struct CacheJson {
    count: usize,
    bytes: u64,
    budget_bytes: u64,
    /// Most recently used first.
    assets: Vec<CachedAssetJson>,
}

#[derive(Serialize)]
struct CachedAssetJson {
    asset_id: String,
    version: String,
    bytes: u64,
    tracks: usize,
    subtitles: usize,
    /// One for anything but a sequence of clips.
    clips: usize,
    duration_seconds: f64,
}

impl From<RegistryStatus> for StatusResponse {
    fn from(status: RegistryStatus) -> Self {
        Self {
            resolver: ResolverJson {
                kind: status.resolver.kind,
                healthy: status.resolver.healthy,
                base_url: status.resolver.base_url,
                known_assets: status.resolver.known_assets,
            },
            cache: CacheJson {
                count: status.cache.count,
                bytes: status.cache.bytes,
                budget_bytes: status.cache.budget_bytes,
                assets: status
                    .cache
                    .assets
                    .into_iter()
                    .map(|asset| CachedAssetJson {
                        asset_id: asset.asset_id,
                        version: asset.version,
                        bytes: asset.bytes,
                        tracks: asset.tracks,
                        subtitles: asset.subtitles,
                        clips: asset.clips,
                        duration_seconds: asset.duration_seconds,
                    })
                    .collect(),
            },
        }
    }
}

pub(crate) async fn status(State(state): State<AppState>) -> Response {
    let body: StatusResponse = state.registry.status().await.into();
    let json =
        serde_json::to_vec(&body).expect("a status response of plain fields always serializes");
    (
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        json,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_response_serializes_to_the_documented_shape() {
        let response = StatusResponse {
            resolver: ResolverJson {
                kind: "mapper",
                healthy: true,
                base_url: Some("https://mapper.internal".to_owned()),
                known_assets: None,
            },
            cache: CacheJson {
                count: 1,
                bytes: 4096,
                budget_bytes: 1_000_000,
                assets: vec![CachedAssetJson {
                    asset_id: "movie".to_owned(),
                    version: "v1".to_owned(),
                    bytes: 4096,
                    tracks: 2,
                    subtitles: 1,
                    clips: 1,
                    duration_seconds: 120.5,
                }],
            },
        };

        let json: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&response).expect("response should serialize"),
        )
        .unwrap();

        assert_eq!(json["resolver"]["kind"], "mapper");
        assert_eq!(json["resolver"]["known_assets"], serde_json::Value::Null);
        assert_eq!(json["cache"]["assets"][0]["asset_id"], "movie");
        assert_eq!(json["cache"]["assets"][0]["duration_seconds"], 120.5);
    }
}
