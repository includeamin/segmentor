//! Sources of media bytes: local files and remote HTTP objects.
//!
//! [`MediaSourceKind`] is the type the rest of the pipeline reads through. Local files use
//! positioned reads on the blocking pool; remote objects use ranged HTTP requests. Both are
//! async so a slow origin never parks a thread.

mod http;
mod local;
mod metadata;

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;

use crate::error::{Error, Result};

pub(crate) use http::{
    HttpMediaSource, LocationRefresher, RemoteReader, RemoteSettings, is_public_address,
};
pub(crate) use local::LocalMediaSource;
pub(crate) use metadata::{Discovery, DroppedTail, Fragment, Metadata};

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
    /// A BLAKE3 hash of `moov` alone.
    pub(crate) moov_hash: Option<[u8; 32]>,
    /// A BLAKE3 hash of `moov` and every `moof`: everything the index is built from. For a progressive
    /// file it equals `moov_hash`. A fragmented file's `moov` is nearly the same across
    /// recordings from one encoder, so only this hash tells such files apart.
    pub(crate) metadata_hash: Option<[u8; 32]>,
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

    /// Whether `range` of the source still holds exactly `expected`.
    ///
    /// A local file is compared a window at a time, so checking a `moov` of several megabytes
    /// does not allocate (and, in a fresh process, fault in) a second copy of it. A remote
    /// object is read in one request, as more requests would cost more than the memory.
    pub(crate) async fn matches(&self, range: ByteRange, expected: Bytes) -> Result<bool> {
        const WINDOW: u64 = 256 * 1024;
        if u64::try_from(expected.len()).ok() != Some(range.length) {
            return Ok(false);
        }
        match self {
            Self::Local(source) => {
                let source = Arc::clone(source);
                tokio::task::spawn_blocking(move || {
                    let mut checked = 0u64;
                    while checked < range.length {
                        let length = WINDOW.min(range.length - checked);
                        let window = ByteRange::new(range.offset + checked, length);
                        let start = usize::try_from(checked).expect("expected is in memory");
                        let end = start + usize::try_from(length).expect("expected is in memory");
                        if source.read_range(window)?[..] != expected[start..end] {
                            return Ok(false);
                        }
                        checked += length;
                    }
                    Ok(true)
                })
                .await
                .map_err(|error| Error::Io(std::io::Error::other(error)))?
            }
            Self::Http(source) => Ok(source.read_range(range).await?[..] == expected[..]),
        }
    }

    /// Points a remote source at a re-signed URL for the same object. A no-op for local files.
    pub(crate) fn update_location(&self, url: &reqwest::Url) {
        if let Self::Http(source) = self {
            source.set_url(url.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A file a little over three comparison windows long, compared against copies that differ
    /// at a window's first byte, its last byte, and in the final partial window.
    #[tokio::test]
    async fn matches_compares_a_local_range_window_by_window() {
        let bytes = (0..800_000u32)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/source-matches.bin");
        std::fs::write(&path, &bytes).unwrap();
        let source = MediaSourceKind::Local(Arc::new(LocalMediaSource::open(&path).unwrap()));
        let range = ByteRange::new(100, 790_000);
        let expected = Bytes::copy_from_slice(&bytes[100..790_100]);

        assert!(source.matches(range, expected.clone()).await.unwrap());
        for position in [0, 262_143, 262_144, 789_999] {
            let mut changed = expected.to_vec();
            changed[position] ^= 1;
            assert!(
                !source.matches(range, Bytes::from(changed)).await.unwrap(),
                "a change at {position} went unnoticed"
            );
        }
        assert!(!source.matches(range, expected.slice(1..)).await.unwrap());
        std::fs::remove_file(path).unwrap();
    }
}
