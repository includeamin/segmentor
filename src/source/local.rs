use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use bytes::Bytes;

use super::{ByteRange, MediaSource, SourceIdentity};
use crate::error::{Error, Result};

#[derive(Debug)]
pub(crate) struct LocalMediaSource {
    file: File,
    identity: SourceIdentity,
}

impl LocalMediaSource {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let canonical_path = path.as_ref().canonicalize()?;
        let file = File::open(&canonical_path)?;
        let metadata = file.metadata()?;
        let identity = SourceIdentity {
            canonical_path,
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        };

        Ok(Self { file, identity })
    }

    pub(crate) fn open_file(&self) -> Result<File> {
        Ok(File::open(&self.identity.canonical_path)?)
    }
}

impl MediaSource for LocalMediaSource {
    fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    fn len(&self) -> u64 {
        self.identity.length
    }

    fn read_range(&self, range: ByteRange) -> Result<Bytes> {
        let end = range.end().ok_or(Error::InvalidRange {
            offset: range.offset,
            length: range.length,
            source_len: self.len(),
        })?;
        let length = usize::try_from(range.length).map_err(|_| Error::InvalidRange {
            offset: range.offset,
            length: range.length,
            source_len: self.len(),
        })?;

        if end > self.len() {
            return Err(Error::InvalidRange {
                offset: range.offset,
                length: range.length,
                source_len: self.len(),
            });
        }

        let mut bytes = vec![0; length];
        self.file.read_exact_at(&mut bytes, range.offset)?;
        Ok(Bytes::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4")
    }

    #[test]
    fn reads_a_positioned_range() {
        let source = LocalMediaSource::open(fixture()).expect("fixture should open");

        let bytes = source
            .read_range(ByteRange::new(4, 4))
            .expect("range should be readable");

        assert_eq!(&bytes[..], b"ftyp");
    }

    #[test]
    fn rejects_a_range_past_the_source() {
        let source = LocalMediaSource::open(fixture()).expect("fixture should open");

        let error = source
            .read_range(ByteRange::new(source.len(), 1))
            .expect_err("out-of-bounds range should fail");

        assert!(matches!(error, Error::InvalidRange { .. }));
    }
}
