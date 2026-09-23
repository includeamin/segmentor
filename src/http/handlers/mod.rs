//! Request handlers, grouped by resource.
mod admin;
mod health;
mod media;
mod playlist;

use crate::http::error::{HttpError, HttpResult};
use crate::media::TrackKey;

pub(crate) use admin::status as admin_status;
pub(crate) use health::{health, metrics, ready};
pub(crate) use media::{iframe_segment, init_segment, media_segment, subtitle_file};
pub(crate) use playlist::{
    dash_manifest, iframe_playlist, master_playlist, media_playlist, subtitle_playlist,
};

/// A `{track}` URL segment, decomposed into which video rendition it names (`video-{id}`, for a
/// composite asset) and the track itself. A plain asset's `video` and the shared `audio-{n}`
/// group (never rendition-scoped, even on a composite) both carry no rendition.
pub(crate) struct RequestedTrack {
    pub(crate) rendition: Option<String>,
    pub(crate) key: TrackKey,
}

/// A rendition id as it appears in a URL: letters, digits, and hyphens, matching the mapper-side
/// validation in `resolver::mapper` so the same names round-trip.
const MAX_RENDITION_ID_BYTES: usize = 32;

pub(crate) fn parse_track(track: &str) -> HttpResult<RequestedTrack> {
    if let Some(id) = track.strip_prefix("video-") {
        return if is_rendition_id(id) {
            Ok(RequestedTrack {
                rendition: Some(id.to_owned()),
                key: TrackKey::VIDEO,
            })
        } else {
            Err(HttpError::not_found("track does not exist"))
        };
    }
    TrackKey::parse(track)
        .map(|key| RequestedTrack {
            rendition: None,
            key,
        })
        .ok_or_else(|| HttpError::not_found("track does not exist"))
}

fn is_rendition_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RENDITION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
