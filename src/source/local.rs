use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use bytes::Bytes;

use super::{ByteRange, MediaSource, SourceIdentity};
use crate::error::{Error, Result};

const PARSER_BUFFER_BYTES: usize = 256 * 1024;

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
            moov_sha256: None,
        };

        Ok(Self { file, identity })
    }

    /// A buffered handle positioned at the start, for the `mp4` crate.
    ///
    /// The crate reads every sample-table entry with a separate `read`, so an unbuffered file
    /// costs one system call per four bytes: seconds for an hour of media. The buffer turns
    /// that into a handful of reads. Seeking past `mdat` simply discards it.
    pub(crate) fn parser_file(&self) -> Result<BufReader<File>> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(BufReader::with_capacity(PARSER_BUFFER_BYTES, file))
    }

    pub(crate) fn verify_unchanged(&self) -> Result<()> {
        let metadata = self.file.metadata()?;
        let unchanged = metadata.dev() == self.identity.device
            && metadata.ino() == self.identity.inode
            && metadata.len() == self.identity.length
            && metadata.mtime() == self.identity.modified_seconds
            && metadata.mtime_nsec() == self.identity.modified_nanoseconds;
        if unchanged {
            Ok(())
        } else {
            Err(Error::InvalidMedia(
                "source changed while it was being parsed".to_owned(),
            ))
        }
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
