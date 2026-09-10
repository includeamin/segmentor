mod local;

use std::path::PathBuf;

use bytes::Bytes;

use crate::error::Result;

pub(crate) use local::LocalMediaSource;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) canonical_path: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) length: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: i64,
    pub(crate) moov_sha256: Option<[u8; 32]>,
}

pub(crate) trait MediaSource {
    fn identity(&self) -> &SourceIdentity;
    fn len(&self) -> u64;
    fn read_range(&self, range: ByteRange) -> Result<Bytes>;
}
