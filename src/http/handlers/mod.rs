//! Request handlers, grouped by resource.
mod health;
mod media;
mod playlist;

use crate::http::error::{HttpError, HttpResult};
use crate::media::TrackKind;

pub(crate) use health::{health, metrics, ready};
pub(crate) use media::{init_segment, media_segment};
pub(crate) use playlist::{dash_manifest, master_playlist, media_playlist};

pub(crate) fn parse_track(track: &str) -> HttpResult<TrackKind> {
    match track {
        "audio" => Ok(TrackKind::Audio),
        "video" => Ok(TrackKind::Video),
        _ => Err(HttpError::not_found("track does not exist")),
    }
}
