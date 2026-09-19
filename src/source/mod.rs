//! Sources of media bytes: local files and remote HTTP objects.
//!
//! [`MediaSourceKind`] is the type the rest of the pipeline reads through. Local files use
//! positioned reads on the blocking pool; remote objects use ranged HTTP requests. Both are
//! async so a slow origin never parks a thread.

mod http;
mod local;
mod sparse;

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;

use crate::error::{Error, Result};

pub(crate) use http::{HttpMediaSource, RemoteReader, RemoteSettings, is_public_address};
pub(crate) use local::LocalMediaSource;
pub(crate) use sparse::SparseFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

impl ByteRange {
    pub(crate) const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    pub(crate) fn end(self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// Where the bytes came from, in enough detail to notice that they changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    Local {
        canonical_path: PathBuf,
        device: u64,
        inode: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    },
    Remote {
        /// The URL without its query string, which may carry credentials.
        url: String,
        /// The `ETag` or `Last-Modified` value that reads are conditioned on.
        validator: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) origin: Origin,
    pub(crate) length: u64,
    pub(crate) moov_sha256: Option<[u8; 32]>,
}

/// A local file or a remote object.
#[derive(Debug, Clone)]
pub(crate) enum MediaSourceKind {
    Local(Arc<LocalMediaSource>),
    Http(Arc<HttpMediaSource>),
}

impl MediaSourceKind {
    pub(crate) fn identity(&self) -> &SourceIdentity {
        match self {
            Self::Local(source) => source.identity(),
            Self::Http(source) => source.identity(),
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.identity().length
    }

    /// Reads exactly `range`, or fails.
    pub(crate) async fn read_range(&self, range: ByteRange) -> Result<Bytes> {
        match self {
            Self::Local(source) => {
                let source = Arc::clone(source);
                tokio::task::spawn_blocking(move || source.read_range(range))
                    .await
                    .map_err(|error| Error::Io(std::io::Error::other(error)))?
            }
            Self::Http(source) => source.read_range(range).await,
        }
    }

    /// Fails if the underlying object is no longer the one that was opened.
    pub(crate) async fn verify_unchanged(&self) -> Result<()> {
        match self {
            Self::Local(source) => source.verify_unchanged(),
            Self::Http(source) => source.verify_unchanged().await,
        }
    }
}
