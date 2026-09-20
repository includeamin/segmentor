//! The part of a file the MP4 parser needs, held in memory.
//!
//! A progressive file's index is entirely in `moov`, and the media payload is only ever read
//! later, one segment at a time. A fragmented file's index is spread over one `moof` per
//! fragment, so those are needed too. `Metadata` walks the top-level box headers, keeps `moov`
//! and every `moof` whole, and skips everything else. That makes parsing independent of where
//! the file lives: a remote origin costs a handful of small range requests instead of a
//! download.

use bytes::Bytes;

use super::{ByteRange, MediaSourceKind};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};

/// A file has a handful of top-level boxes besides `moov`, `moof`, and `mdat`. Refusing more
/// keeps a hostile file from turning metadata discovery into millions of round trips.
const MAX_OTHER_BOXES: usize = 4096;

/// How many bytes are read at once while walking. A read this size usually holds a whole `moof`
/// (a two-second HD fragment has a few kilobytes of `trun` entries) and the header of the `mdat`
/// after it, so a fragment costs about one read instead of three.
const WINDOW: u64 = 8 * 1024;

/// The `moov` box of a source, and the `moof` box of every fragment, with where they came from.
#[derive(Debug)]
pub(crate) struct Metadata {
    len: u64,
    moov: ByteRange,
    moov_bytes: Bytes,
    fragments: Vec<Fragment>,
}

/// One `moof` box, header included.
#[derive(Debug, Clone)]
pub(crate) struct Fragment {
    /// Where the box starts in the source.
    pub(crate) offset: u64,
    pub(crate) bytes: Bytes,
}

/// Reads from a source through a window, so that consecutive small reads near each other cost
/// one request.
struct WindowedReader<'a> {
    source: &'a MediaSourceKind,
    window_start: u64,
    window: Bytes,
}

impl<'a> WindowedReader<'a> {
    const fn new(source: &'a MediaSourceKind) -> Self {
        Self {
            source,
            window_start: 0,
            window: Bytes::new(),
        }
    }

    /// Exactly `length` bytes at `offset`, which must be inside the source.
    async fn read(&mut self, offset: u64, length: u64) -> Result<Bytes> {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("box range overflows"))?;
        let window_end = self.window_start + self.window.len() as u64;
        if offset < self.window_start || end > window_end {
            let fetch = length.max(WINDOW).min(self.source.len() - offset);
            self.window = self
                .source
                .read_range(ByteRange::new(offset, fetch))
                .await?;
            self.window_start = offset;
        }
        let start = usize::try_from(offset - self.window_start)
            .map_err(|_| invalid("window offset does not fit in memory"))?;
        let length = usize::try_from(length).map_err(|_| invalid("box does not fit in memory"))?;
        Ok(self.window.slice(start..start + length))
    }
}

impl Metadata {
    /// Walks the top-level boxes and fetches `moov` and every `moof` whole.
    pub(crate) async fn fetch(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<Self> {
        let len = source.len();
        let mut reader = WindowedReader::new(source);
        let mut moov = None;
        let mut fragments: Vec<Fragment> = Vec::new();
        let mut metadata_bytes = 0u64;
        let mut other_boxes = 0usize;
        let mut media_boxes = 0usize;
        let mut offset = 0u64;
        while offset < len {
            if len - offset < 8 {
                return Err(invalid("invalid top-level MP4 box size"));
            }
            let header = reader.read(offset, 8).await?;
            let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
            let name: [u8; 4] = header[4..8].try_into().expect("four bytes");
            let (size, header_size) = match size32 {
                1 => {
                    if len - offset < 16 {
                        return Err(invalid("invalid top-level MP4 box size"));
                    }
                    let extended = reader.read(offset + 8, 8).await?;
                    let size = u64::from_be_bytes(extended[..8].try_into().expect("eight bytes"));
                    (size, 16)
                }
                0 => (len - offset, 8),
                size => (u64::from(size), 8),
            };
            if size < header_size || offset.checked_add(size).is_none_or(|end| end > len) {
                return Err(invalid("invalid top-level MP4 box size"));
            }
            match &name {
                b"moov" => {
                    if size > limits.max_metadata_bytes {
                        return Err(invalid("moov exceeds configured metadata limit"));
                    }
                    let bytes = reader.read(offset, size).await?;
                    moov = Some((ByteRange::new(offset, size), bytes));
                }
                b"moof" => {
                    if fragments.len() >= limits.max_fragments {
                        return Err(Error::Unsupported(format!(
                            "the file has more than {} fragments, the limit set by limits.max_fragments",
                            limits.max_fragments
                        )));
                    }
                    metadata_bytes = metadata_bytes.saturating_add(size);
                    if metadata_bytes > limits.max_metadata_bytes {
                        return Err(invalid(
                            "moov and moof boxes exceed configured metadata limit",
                        ));
                    }
                    let bytes = reader.read(offset, size).await?;
                    fragments.push(Fragment { offset, bytes });
                }
                // Every fragment has one, and a progressive file has few, so this bound is
                // generous for both and stops a file made of empty boxes.
                b"mdat" => {
                    media_boxes += 1;
                    if media_boxes > limits.max_fragments.saturating_add(16) {
                        return Err(invalid("too many top-level MP4 boxes"));
                    }
                }
                _ => {
                    other_boxes += 1;
                    if other_boxes > MAX_OTHER_BOXES {
                        return Err(invalid("too many top-level MP4 boxes"));
                    }
                }
            }
            offset += size;
        }
        let (moov, moov_bytes) = moov.ok_or_else(|| invalid("missing moov box"))?;
        metadata_bytes = metadata_bytes.saturating_add(moov.length);
        if metadata_bytes > limits.max_metadata_bytes {
            return Err(invalid(
                "moov and moof boxes exceed configured metadata limit",
            ));
        }
        Ok(Self {
            len,
            moov,
            moov_bytes,
            fragments,
        })
    }

    /// Wraps metadata built or altered in memory, for tests that corrupt real files.
    #[cfg(test)]
    pub(crate) fn from_parts(len: u64, moov_bytes: Vec<u8>, fragments: Vec<Fragment>) -> Self {
        Self {
            len,
            moov: ByteRange::new(0, moov_bytes.len() as u64),
            moov_bytes: Bytes::from(moov_bytes),
            fragments,
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

    /// Every `moof` box in file order; empty for a progressive file.
    pub(crate) fn fragments(&self) -> &[Fragment] {
        &self.fragments
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidMedia(message.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::source::LocalMediaSource;

    fn boxed(name: [u8; 4], payload_len: usize, fill: u8) -> Vec<u8> {
        let mut bytes = u32::try_from(payload_len + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(&name);
        bytes.resize(payload_len + 8, fill);
        bytes
    }

    /// Writes `bytes` under `target/` and opens it as a source.
    fn source(name: &str, bytes: &[u8]) -> MediaSourceKind {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/metadata-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        MediaSourceKind::Local(Arc::new(LocalMediaSource::open(path).unwrap()))
    }

    fn fetch(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<Metadata> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Metadata::fetch(source, limits))
    }

    /// `ftyp`, `moov`, then three fragments, the middle `moof` larger than the read window,
    /// with other boxes in between that must be skipped.
    fn fragmented_file() -> (Vec<u8>, Vec<(u64, usize)>) {
        let mut file = boxed(*b"ftyp", 8, 1);
        file.extend(boxed(*b"moov", 100, 2));
        file.extend(boxed(*b"sidx", 24, 3));
        let mut moofs = Vec::new();
        for (size, mdat) in [(60, 500), (20_000, 100), (44, 0)] {
            moofs.push((file.len() as u64, size + 8));
            file.extend(boxed(*b"moof", size, 7));
            file.extend(boxed(*b"mdat", mdat, 9));
        }
        file.extend(boxed(*b"mfra", 12, 4));
        (file, moofs)
    }

    #[test]
    fn keeps_moov_and_every_moof_whole_and_skips_everything_else() {
        let (file, moofs) = fragmented_file();

        let metadata = fetch(&source("walk.mp4", &file), &LimitsConfig::default()).unwrap();

        assert_eq!(metadata.moov_bytes(), &file[16..16 + 108]);
        assert_eq!(metadata.fragments().len(), 3);
        for (fragment, (offset, size)) in metadata.fragments().iter().zip(&moofs) {
            let start = usize::try_from(*offset).unwrap();
            assert_eq!(fragment.offset, *offset);
            assert_eq!(&fragment.bytes[..], &file[start..start + size]);
        }
        assert_eq!(
            metadata.fragments()[1].bytes.len(),
            20_008,
            "larger than the window"
        );
    }

    #[test]
    fn a_progressive_file_has_no_fragments() {
        let mut file = boxed(*b"ftyp", 8, 1);
        file.extend(boxed(*b"mdat", 5000, 9));
        file.extend(boxed(*b"moov", 100, 2));

        let metadata = fetch(&source("progressive.mp4", &file), &LimitsConfig::default()).unwrap();

        assert!(metadata.fragments().is_empty());
    }

    #[test]
    fn more_fragments_than_the_limit_is_refused_naming_the_limit() {
        let (file, _) = fragmented_file();
        let limits = LimitsConfig {
            max_fragments: 2,
            ..LimitsConfig::default()
        };

        let error = fetch(&source("limit.mp4", &file), &limits).unwrap_err();

        assert!(matches!(error, Error::Unsupported(_)), "{error}");
        assert!(
            error.to_string().contains("limits.max_fragments"),
            "{error}"
        );
    }

    #[test]
    fn moov_and_moof_boxes_together_are_held_to_the_metadata_limit() {
        let (file, _) = fragmented_file();
        // The moov alone (108 bytes) fits; with 20 KB of moof it does not.
        let limits = LimitsConfig {
            max_metadata_bytes: 1000,
            ..LimitsConfig::default()
        };

        let error = fetch(&source("bytes.mp4", &file), &limits).unwrap_err();

        assert!(error.to_string().contains("metadata limit"), "{error}");
    }

    #[test]
    fn a_flood_of_empty_mdat_boxes_is_refused_before_it_costs_a_read_each() {
        let mut file = boxed(*b"moov", 40, 2);
        for _ in 0..200 {
            file.extend(boxed(*b"mdat", 0, 0));
        }
        let limits = LimitsConfig {
            max_fragments: 10,
            ..LimitsConfig::default()
        };

        let error = fetch(&source("flood.mp4", &file), &limits).unwrap_err();

        assert!(error.to_string().contains("too many top-level"), "{error}");
    }

    #[test]
    fn a_flood_of_other_boxes_is_refused() {
        let mut file = boxed(*b"moov", 40, 2);
        for _ in 0..=MAX_OTHER_BOXES {
            file.extend(boxed(*b"free", 0, 0));
        }

        let error = fetch(&source("free.mp4", &file), &LimitsConfig::default()).unwrap_err();

        assert!(error.to_string().contains("too many top-level"), "{error}");
    }

    #[test]
    fn malformed_top_level_boxes_are_refused() {
        let mut past_the_end = boxed(*b"moov", 40, 2);
        past_the_end.extend(&9999u32.to_be_bytes());
        past_the_end.extend(b"mdat");
        let mut truncated_header = boxed(*b"moov", 40, 2);
        truncated_header.extend([0, 0, 0]);
        let mut too_small = boxed(*b"moov", 40, 2);
        too_small.extend(&4u32.to_be_bytes());
        too_small.extend(b"free");
        let no_moov = boxed(*b"ftyp", 8, 1);

        for (name, file) in [
            ("past.mp4", past_the_end),
            ("truncated.mp4", truncated_header),
            ("small.mp4", too_small),
            ("nomoov.mp4", no_moov),
        ] {
            assert!(
                fetch(&source(name, &file), &LimitsConfig::default()).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_moof_that_ends_past_the_file_is_refused() {
        let mut file = boxed(*b"moov", 40, 2);
        file.extend(&5000u32.to_be_bytes());
        file.extend(b"moof");
        file.extend([0; 100]);

        assert!(fetch(&source("cut.mp4", &file), &LimitsConfig::default()).is_err());
    }
}
