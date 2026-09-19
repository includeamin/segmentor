//! Byte-range parsing for `Range` and `If-Range`.
use axum::body::Body;
use axum::http::header::{ACCEPT_RANGES, CONTENT_RANGE, IF_RANGE, RANGE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

use super::error::{HttpError, HttpResult};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ByteInterval {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

impl ByteInterval {
    pub(crate) fn overlap(self, start: u64, end: u64) -> Option<Self> {
        let overlap = Self {
            start: self.start.max(start),
            end: self.end.min(end),
        };
        (overlap.start < overlap.end).then_some(overlap)
    }
}

/// Parses a single `bytes=` range, including suffix ranges (`bytes=-500`).
///
/// A present `If-Range` that does not equal the current strong `ETag` means the client's cached
/// copy is stale, so the range is ignored and the full representation is sent. Multi-range
/// requests are rejected.
pub(crate) fn requested_range(
    headers: &HeaderMap,
    total: u64,
    etag: &HeaderValue,
) -> std::result::Result<Option<ByteInterval>, ()> {
    let Some(value) = headers.get(RANGE) else {
        return Ok(None);
    };
    if headers
        .get(IF_RANGE)
        .is_some_and(|condition| condition != etag)
    {
        return Ok(None);
    }
    let value = value.to_str().map_err(|_| ())?;
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 || total == 0 {
            return Err(());
        }
        (total.saturating_sub(suffix), total)
    } else {
        let start = start.parse::<u64>().map_err(|_| ())?;
        let end = if end.is_empty() {
            total
        } else {
            end.parse::<u64>()
                .map_err(|_| ())?
                .checked_add(1)
                .ok_or(())?
                .min(total)
        };
        (start, end)
    };
    if start >= end || start >= total {
        return Err(());
    }
    Ok(Some(ByteInterval { start, end }))
}

pub(crate) fn range_not_satisfiable(total: u64) -> HttpResult<Response> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(CONTENT_RANGE, format!("bytes */{total}"))
        .header(ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .body(Body::empty())
        .map_err(|error| HttpError::internal(error.to_string()))
}
