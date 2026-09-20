//! The part of a file the MP4 parser needs, held in memory.
//!
//! Parsing needs the `moov` box and nothing else: every sample's location and timing lives in
//! its tables, and the media payload is only ever read later, one segment at a time. `Metadata`
//! finds `moov` by walking the top-level box headers with small reads, then fetches it whole.
//! That makes parsing independent of where the file lives: a remote origin costs a handful of
//! small range requests instead of a download.

use bytes::Bytes;

use super::{ByteRange, MediaSourceKind};
use crate::error::{Error, Result};

/// A well-formed file has a handful of top-level boxes. Refusing more keeps a hostile file from
/// turning metadata discovery into millions of round trips.
const MAX_TOP_LEVEL_BOXES: usize = 4096;

/// The `moov` box of a source, with where it came from.
#[derive(Debug)]
pub(crate) struct Metadata {
    len: u64,
    moov: ByteRange,
    moov_bytes: Bytes,
}

impl Metadata {
    /// Walks the top-level boxes with small reads and fetches `moov` whole.
    pub(crate) async fn fetch(source: &MediaSourceKind, max_metadata_bytes: u64) -> Result<Self> {
        let len = source.len();
        let mut moov = None;
        let mut offset = 0u64;
        let mut boxes = 0usize;
        while offset < len {
            boxes += 1;
            if boxes > MAX_TOP_LEVEL_BOXES {
                return Err(invalid("too many top-level MP4 boxes"));
            }
            let header = source.read_range(ByteRange::new(offset, 8)).await?;
            let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
            let name: [u8; 4] = header[4..8].try_into().expect("four bytes");
            let (size, header_size) = match size32 {
                1 => {
                    let extended = source.read_range(ByteRange::new(offset + 8, 8)).await?;
                    let size = u64::from_be_bytes(extended[..8].try_into().expect("eight bytes"));
                    (size, 16)
                }
                0 => (len - offset, 8),
                size => (u64::from(size), 8),
            };
            if size < header_size || offset.checked_add(size).is_none_or(|end| end > len) {
                return Err(invalid("invalid top-level MP4 box size"));
            }
            if &name == b"moov" {
                if size > max_metadata_bytes {
                    return Err(invalid("moov exceeds configured metadata limit"));
                }
                moov = Some(ByteRange::new(offset, size));
            }
            offset += size;
        }
        let moov = moov.ok_or_else(|| invalid("missing moov box"))?;
        let moov_bytes = source.read_range(moov).await?;
        Ok(Self {
            len,
            moov,
            moov_bytes,
        })
    }

    /// Wraps a `moov` box built or altered in memory, for tests that corrupt real metadata.
    #[cfg(test)]
    pub(crate) fn from_moov(len: u64, moov_bytes: Vec<u8>) -> Self {
        Self {
            len,
            moov: ByteRange::new(0, moov_bytes.len() as u64),
            moov_bytes: Bytes::from(moov_bytes),
        }
    }

    pub(crate) const fn len(&self) -> u64 {
        self.len
    }

    /// Where the `moov` box sits in the source.
    pub(crate) const fn moov_range(&self) -> ByteRange {
        self.moov
    }

    /// The complete `moov` box, header included.
    pub(crate) fn moov_bytes(&self) -> &[u8] {
        &self.moov_bytes
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidMedia(message.to_owned())
}
