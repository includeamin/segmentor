use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use bytes::Bytes;

use super::{ByteRange, Origin, SourceIdentity};
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
            origin: Origin::Local {
                canonical_path,
                device: metadata.dev(),
                inode: metadata.ino(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
            },
            length: metadata.len(),
            moov_sha256: None,
            metadata_sha256: None,
        };

        Ok(Self { file, identity })
    }

    pub(crate) fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    pub(crate) fn len(&self) -> u64 {
        self.identity.length
    }

    pub(crate) fn verify_unchanged(&self) -> Result<()> {
        let metadata = self.file.metadata()?;
        let Origin::Local {
            device,
            inode,
            modified_seconds,
            modified_nanoseconds,
            ..
        } = &self.identity.origin
        else {
            unreachable!("a local source always has a local origin");
        };
        let unchanged = metadata.dev() == *device
            && metadata.ino() == *inode
            && metadata.len() == self.identity.length
            && metadata.mtime() == *modified_seconds
            && metadata.mtime_nsec() == *modified_nanoseconds;
        if unchanged {
            Ok(())
        } else {
            Err(Error::InvalidMedia(
                "source changed while it was being parsed".to_owned(),
            ))
        }
    }

    /// Blocking positioned read; async callers go through `MediaSourceKind::read_range`.
    pub(crate) fn read_range(&self, range: ByteRange) -> Result<Bytes> {
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
