use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("invalid byte range: offset {offset}, length {length}, source length {source_len}")]
    InvalidRange {
        offset: u64,
        length: u64,
        source_len: u64,
    },

    #[error("invalid media data: {0}")]
    InvalidMedia(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("MP4 error: {0}")]
    Mp4(#[from] ::mp4::Error),

    #[error("configuration error: {0}")]
    Configuration(String),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("logging initialization error: {0}")]
    Logging(String),

    #[error("unsupported media: {0}")]
    Unsupported(&'static str),

    #[error("not found: {0}")]
    NotFound(&'static str),

    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("upstream unavailable: {0}")]
    UpstreamUnavailable(String),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
