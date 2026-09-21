//! Request handlers, grouped by resource.
mod health;
mod media;
mod playlist;

use crate::http::error::{HttpError, HttpResult};
use crate::media::TrackKey;

pub(crate) use health::{health, metrics, ready};
pub(crate) use media::{iframe_segment, init_segment, media_segment};
pub(crate) use playlist::{dash_manifest, iframe_playlist, master_playlist, media_playlist};

pub(crate) fn parse_track(track: &str) -> HttpResult<TrackKey> {
    TrackKey::parse(track).ok_or_else(|| HttpError::not_found("track does not exist"))
}
