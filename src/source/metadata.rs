//! The part of a file the MP4 parser needs, held in memory.
//!
//! A progressive file's index is entirely in `moov`, and the media payload is only ever read
//! later, one segment at a time. A fragmented file's index is spread over one `moof` per
//! fragment, so those are needed too. `Metadata` walks the top-level box headers, keeps `moov`
//! and every `moof` whole, and skips everything else. That makes parsing independent of where
//! the file lives: a remote origin costs a handful of small range requests instead of a
//! download.
//!
//! Finding each next box needs the size of the one before, so the walk is sequential and costs a
//! request per fragment. When a `sidx` lists where the fragments are, they are fetched in
//! parallel instead. The `sidx` is only a hint about where to look: every fragment is found by
//! walking its own box structure, and anything that disagrees sends the walk back to sequential.

use std::io;

use bytes::Bytes;
use tokio::task::JoinSet;

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

/// A `sidx` with fewer references than this is not worth fetching in parallel: a file with a
/// `sidx` per fragment has one reference each, and the sequential walk already reads them.
const MIN_PARALLEL_REFERENCES: usize = 4;

/// The largest `sidx` read, at 12 bytes per reference. Larger than any real one, since
/// `max_fragments` bounds the references before the box is trusted.
const MAX_SIDX_BYTES: u64 = 1024 * 1024;

/// How the fragments of a file were found, for the operator's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Discovery {
    /// One box after another.
    Sequential,
    /// In parallel, from the `sidx` at the start of the fragments.
    Sidx,
}

/// The `moov` box of a source, and the `moof` box of every fragment, with where they came from.
#[derive(Debug)]
pub(crate) struct Metadata {
    len: u64,
    moov: ByteRange,
    moov_bytes: Bytes,
    fragments: Vec<Fragment>,
    discovery: Discovery,
    dropped: Option<DroppedTail>,
}

/// What was left out because the file ends partway through a fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DroppedTail {
    /// Bytes at the end of the file that were not used.
    pub(crate) bytes: u64,
    /// Complete `moof` boxes dropped because their `mdat` was cut off.
    pub(crate) fragments: usize,
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

    /// The header of the box at `offset`, which must end at or before `end`.
    async fn header(&mut self, offset: u64, end: u64) -> Result<BoxHeader> {
        if end - offset < 8 {
            return Err(invalid("invalid top-level MP4 box size"));
        }
        let header = self.read(offset, 8).await?;
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let name: [u8; 4] = header[4..8].try_into().expect("four bytes");
        let (size, header_size) = match size32 {
            1 => {
                if end - offset < 16 {
                    return Err(invalid("invalid top-level MP4 box size"));
                }
                let extended = self.read(offset + 8, 8).await?;
                let size = u64::from_be_bytes(extended[..8].try_into().expect("eight bytes"));
                (size, 16)
            }
            0 => (end - offset, 8),
            size => (u64::from(size), 8),
        };
        if size < header_size || offset.checked_add(size).is_none_or(|box_end| box_end > end) {
            return Err(invalid("invalid top-level MP4 box size"));
        }
        Ok(BoxHeader { name, size })
    }
}

struct BoxHeader {
    name: [u8; 4],
    size: u64,
}

/// Where the fragments a `sidx` lists start, and how big each subsegment is.
struct Coverage {
    start: u64,
    sizes: Vec<u64>,
}

impl Coverage {
    /// The offset just past the last subsegment.
    fn end(&self) -> Option<u64> {
        self.sizes
            .iter()
            .try_fold(self.start, |end, size| end.checked_add(*size))
    }
}

/// Reads a `sidx` payload into the subsegments it lists, or `None` if it is not one that can be
/// used: hierarchical, empty, oversized, or zero-sized references.
fn parse_sidx(payload: &[u8], sidx_end: u64, max_references: usize) -> Option<Coverage> {
    let version = *payload.first()?;
    // Reference ID and timescale, then the presentation time and offset, 4 bytes each in
    // version 0 and 8 in version 1.
    let mut at = 4 + 4 + 4;
    let (first_offset, next) = if version == 0 {
        at += 4;
        (
            u64::from(u32::from_be_bytes(
                payload.get(at..at + 4)?.try_into().ok()?,
            )),
            at + 4,
        )
    } else {
        at += 8;
        (
            u64::from_be_bytes(payload.get(at..at + 8)?.try_into().ok()?),
            at + 8,
        )
    };
    // Reserved, then the reference count.
    let count_at = next + 2;
    let count = usize::from(u16::from_be_bytes(
        payload.get(count_at..count_at + 2)?.try_into().ok()?,
    ));
    if count < MIN_PARALLEL_REFERENCES || count > max_references {
        return None;
    }
    let mut sizes = Vec::with_capacity(count);
    for index in 0..count {
        let entry = payload.get(count_at + 2 + index * 12..count_at + 2 + index * 12 + 12)?;
        let word = u32::from_be_bytes(entry[..4].try_into().ok()?);
        // The top bit marks a reference to another `sidx`, which is not followed.
        let size = u64::from(word & 0x7fff_ffff);
        if word & 0x8000_0000 != 0 || size == 0 {
            return None;
        }
        sizes.push(size);
    }
    Some(Coverage {
        start: sidx_end.checked_add(first_offset)?,
        sizes,
    })
}

/// Every `moof` in `[start, end)`, found by walking the boxes there, or `None` if what is there
/// is not a whole number of boxes starting a fragment. A mismatch means the `sidx` that pointed
/// here was wrong; a failed read is a real error.
async fn walk_subsegment(
    source: &MediaSourceKind,
    start: u64,
    end: u64,
) -> Result<Option<Vec<Fragment>>> {
    let mut reader = WindowedReader::new(source);
    let mut fragments = Vec::new();
    let mut offset = start;
    while offset < end {
        let header = match reader.header(offset, end).await {
            Ok(header) => header,
            Err(Error::InvalidMedia(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if &header.name == b"moof" {
            let bytes = reader.read(offset, header.size).await?;
            fragments.push(Fragment { offset, bytes });
        }
        offset += header.size;
    }
    Ok((!fragments.is_empty()).then_some(fragments))
}

/// Fetches the fragments of every subsegment `coverage` lists, `concurrency` at a time, or `None`
/// if any of them is not what the `sidx` said.
async fn fetch_by_sidx(
    source: &MediaSourceKind,
    coverage: &Coverage,
    concurrency: usize,
) -> Result<Option<Vec<Fragment>>> {
    let mut ranges = Vec::with_capacity(coverage.sizes.len());
    let mut offset = coverage.start;
    for size in &coverage.sizes {
        ranges.push((offset, offset + size));
        offset += size;
    }
    let mut results: Vec<Option<Vec<Fragment>>> = vec![None; ranges.len()];
    let mut tasks = JoinSet::new();
    let mut next = 0;
    loop {
        while tasks.len() < concurrency && next < ranges.len() {
            let source = source.clone();
            let (start, end) = ranges[next];
            let index = next;
            tasks.spawn(async move { (index, walk_subsegment(&source, start, end).await) });
            next += 1;
        }
        let Some(finished) = tasks.join_next().await else {
            break;
        };
        let (index, result) = finished.map_err(|error| Error::Io(io::Error::other(error)))?;
        // Returning drops the set, which aborts whatever is still in flight.
        match result? {
            Some(fragments) => results[index] = Some(fragments),
            None => return Ok(None),
        }
    }
    Ok(Some(results.into_iter().flatten().flatten().collect()))
}

/// Which box a file was cut inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// Fewer than eight bytes are left, not even a box header.
    Header,
    Moof,
    Mdat,
}

/// If the box at `offset` is a fragment's `moof` or `mdat` that declares more bytes than the file
/// has left, or there is not even a header left, which one it is. A header that is merely
/// malformed, such as a size below eight, is corruption and not a cut, so it is `None`.
async fn cut_box(reader: &mut WindowedReader<'_>, offset: u64, len: u64) -> Result<Option<Cut>> {
    let remaining = len - offset;
    if remaining < 8 {
        return Ok(Some(Cut::Header));
    }
    let header = reader.read(offset, 8).await?;
    let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
    let declared = match size32 {
        // "To the end of the file" cannot run past it.
        0 => return Ok(None),
        1 if remaining < 16 => return Ok(Some(Cut::Header)),
        1 => u64::from_be_bytes(
            reader.read(offset + 8, 8).await?[..8]
                .try_into()
                .expect("eight bytes"),
        ),
        size if size < 8 => return Ok(None),
        size => u64::from(size),
    };
    if declared <= remaining {
        return Ok(None);
    }
    Ok(match &header[4..8] {
        b"moof" => Some(Cut::Moof),
        b"mdat" => Some(Cut::Mdat),
        _ => None,
    })
}

/// The state of one walk over a file's top-level boxes.
struct Walk<'a> {
    source: &'a MediaSourceKind,
    limits: &'a LimitsConfig,
    len: u64,
    reader: WindowedReader<'a>,
    moov: Option<(ByteRange, Bytes)>,
    fragments: Vec<Fragment>,
    metadata_bytes: u64,
    other_boxes: usize,
    media_boxes: usize,
    discovery: Discovery,
    dropped: Option<DroppedTail>,
    /// Where the last `moof` starts, so that a cut-off `mdat` can take its `moof` with it.
    last_moof: Option<u64>,
    /// A `sidx` seen before any fragment, and where the walk must arrive for it to be used.
    pending: Option<Coverage>,
}

impl<'a> Walk<'a> {
    fn new(source: &'a MediaSourceKind, limits: &'a LimitsConfig) -> Self {
        Self {
            source,
            limits,
            len: source.len(),
            reader: WindowedReader::new(source),
            moov: None,
            fragments: Vec::new(),
            metadata_bytes: 0,
            other_boxes: 0,
            media_boxes: 0,
            discovery: Discovery::Sequential,
            dropped: None,
            last_moof: None,
            pending: None,
        }
    }

    async fn run(&mut self, use_sidx: bool) -> Result<()> {
        let mut offset = 0u64;
        while offset < self.len {
            if let Some(end) = self.try_jump(offset).await? {
                offset = end;
                continue;
            }
            let header = match self.reader.header(offset, self.len).await {
                Ok(header) => header,
                Err(Error::InvalidMedia(message)) => {
                    // Either a real cut, which stops the walk, or plain corruption.
                    self.handle_cut(offset, message).await?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            self.record(offset, &header, use_sidx).await?;
            offset += header.size;
        }
        Ok(())
    }

    /// If a `sidx` said the fragments start exactly here, fetches them in parallel and returns
    /// where the walk resumes. `None` means carry on box by box: there is no such `sidx`, the walk
    /// went past its region without arriving at the start, or the region held something else.
    async fn try_jump(&mut self, offset: u64) -> Result<Option<u64>> {
        let Some(coverage) = self.pending.take_if(|coverage| offset >= coverage.start) else {
            return Ok(None);
        };
        if offset != coverage.start {
            return Ok(None);
        }
        let Some(end) = coverage.end().filter(|end| *end <= self.len) else {
            return Ok(None);
        };
        let Some(found) =
            fetch_by_sidx(self.source, &coverage, self.limits.metadata_concurrency).await?
        else {
            return Ok(None);
        };
        // Fragments the walk found before reaching the region stay; these follow them.
        let taken: u64 = found
            .iter()
            .map(|fragment| fragment.bytes.len() as u64)
            .sum();
        self.fragments.extend(found);
        if self.fragments.len() > self.limits.max_fragments {
            return Err(too_many_fragments(self.limits));
        }
        self.add_metadata_bytes(taken)?;
        self.discovery = Discovery::Sidx;
        Ok(Some(end))
    }

    /// The box at `offset` could not be read as a box. If the file was cut off inside a fragment,
    /// after at least one whole one, either refuses it with advice or, when tolerated, drops the
    /// cut fragment and returns so the walk can end. Anything else is corruption.
    async fn handle_cut(&mut self, offset: u64, message: String) -> Result<()> {
        let cut = if self.fragments.is_empty() {
            None
        } else {
            cut_box(&mut self.reader, offset, self.len).await?
        };
        let Some(cut) = cut else {
            return Err(Error::InvalidMedia(message));
        };
        if !self.limits.tolerate_truncated_tail {
            return Err(Error::InvalidMedia(format!(
                "the file ends partway through a {} box, {} bytes from the end of the last whole \
                 box; if it is still being written or was cut off, set \
                 limits.tolerate_truncated_tail to serve the complete fragments before it",
                match cut {
                    Cut::Header => "box header",
                    Cut::Moof => "moof",
                    Cut::Mdat => "mdat",
                },
                self.len - offset
            )));
        }
        // A cut `mdat` belongs to the `moof` just before it, which now describes samples that are
        // not all there.
        let (from, dropped_fragments) = if cut == Cut::Mdat
            && let Some(moof) = self.last_moof.take_if(|start| {
                self.fragments
                    .last()
                    .is_some_and(|fragment| fragment.offset == *start)
            }) {
            self.fragments.pop();
            (moof, 1)
        } else {
            (offset, 0)
        };
        if self.fragments.is_empty() {
            return Err(invalid(
                "the file ends before its first fragment is complete",
            ));
        }
        self.dropped = Some(DroppedTail {
            bytes: self.len - from,
            fragments: dropped_fragments,
        });
        Ok(())
    }

    /// Keeps or counts the box `header` describes.
    async fn record(&mut self, offset: u64, header: &BoxHeader, use_sidx: bool) -> Result<()> {
        let size = header.size;
        match &header.name {
            b"moov" => {
                if size > self.limits.max_metadata_bytes {
                    return Err(invalid("moov exceeds configured metadata limit"));
                }
                let bytes = self.reader.read(offset, size).await?;
                self.moov = Some((ByteRange::new(offset, size), bytes));
            }
            b"moof" => {
                if self.fragments.len() >= self.limits.max_fragments {
                    return Err(too_many_fragments(self.limits));
                }
                self.add_metadata_bytes(size)?;
                let bytes = self.reader.read(offset, size).await?;
                self.fragments.push(Fragment { offset, bytes });
                self.last_moof = Some(offset);
            }
            b"sidx" if use_sidx && self.fragments.is_empty() && self.pending.is_none() => {
                self.count_other()?;
                if size <= MAX_SIDX_BYTES {
                    let payload = self.reader.read(offset + 8, size - 8).await?;
                    self.pending = parse_sidx(&payload, offset + size, self.limits.max_fragments);
                }
            }
            // Every fragment has one, and a progressive file has few, so this bound is generous
            // for both and stops a file made of empty boxes.
            b"mdat" => {
                self.media_boxes += 1;
                if self.media_boxes > self.limits.max_fragments.saturating_add(16) {
                    return Err(invalid("too many top-level MP4 boxes"));
                }
            }
            _ => self.count_other()?,
        }
        Ok(())
    }

    fn count_other(&mut self) -> Result<()> {
        self.other_boxes += 1;
        if self.other_boxes > MAX_OTHER_BOXES {
            return Err(invalid("too many top-level MP4 boxes"));
        }
        Ok(())
    }

    /// Adds to the running total of metadata bytes and checks it against the limit.
    fn add_metadata_bytes(&mut self, bytes: u64) -> Result<()> {
        self.metadata_bytes = self.metadata_bytes.saturating_add(bytes);
        if self.metadata_bytes > self.limits.max_metadata_bytes {
            return Err(invalid(
                "moov and moof boxes exceed configured metadata limit",
            ));
        }
        Ok(())
    }
}

impl Metadata {
    /// Walks the top-level boxes and fetches `moov` and every `moof` whole.
    pub(crate) async fn fetch(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<Self> {
        Self::fetch_with(source, limits, true).await
    }

    /// `fetch`, with the `sidx` fast path switchable so a test can compare the two.
    async fn fetch_with(
        source: &MediaSourceKind,
        limits: &LimitsConfig,
        use_sidx: bool,
    ) -> Result<Self> {
        let mut walk = Walk::new(source, limits);
        walk.run(use_sidx).await?;
        let (moov, moov_bytes) = walk
            .moov
            .take()
            .ok_or_else(|| invalid("missing moov box"))?;
        walk.add_metadata_bytes(moov.length)?;
        Ok(Self {
            len: walk.len,
            moov,
            moov_bytes,
            fragments: walk.fragments,
            discovery: walk.discovery,
            dropped: walk.dropped,
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
            discovery: Discovery::Sequential,
            dropped: None,
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

    /// How the fragments were found.
    pub(crate) const fn discovery(&self) -> Discovery {
        self.discovery
    }

    /// What was left out because the file ends partway through a fragment, if anything.
    pub(crate) const fn dropped_tail(&self) -> Option<DroppedTail> {
        self.dropped
    }
}

fn too_many_fragments(limits: &LimitsConfig) -> Error {
    Error::Unsupported(format!(
        "the file has more than {} fragments, the limit set by limits.max_fragments",
        limits.max_fragments
    ))
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

    // ---- a file cut off inside a fragment ------------------------------------------------

    fn tolerant() -> LimitsConfig {
        LimitsConfig {
            tolerate_truncated_tail: true,
            ..LimitsConfig::default()
        }
    }

    /// Five whole fragments, then whatever `tail` is.
    fn five_then(tail: &[u8]) -> (Vec<u8>, usize) {
        let (head, parts) = subsegments(6, 1);
        let mut file = head;
        for part in &parts[..5] {
            file.extend(part);
        }
        let whole = file.len();
        file.extend(tail);
        (file, whole)
    }

    #[test]
    fn a_cut_mdat_takes_its_moof_with_it_when_tolerated() {
        let (_, parts) = subsegments(6, 1);
        // The sixth fragment's `moof` is whole, but its `mdat` stops halfway.
        let sixth = &parts[5];
        let (file, whole) = five_then(&sixth[..sixth.len() - 9000]);

        let metadata = fetch(&source("cut-mdat.mp4", &file), &tolerant()).unwrap();

        assert_eq!(
            metadata.fragments().len(),
            5,
            "only complete fragments are kept"
        );
        let dropped = metadata.dropped_tail().expect("something was dropped");
        assert_eq!(dropped.fragments, 1, "the moof whose mdat is incomplete");
        assert_eq!(dropped.bytes, (file.len() - whole) as u64);
    }

    #[test]
    fn a_cut_moof_is_dropped_when_tolerated() {
        let (_, parts) = subsegments(6, 1);
        let (file, whole) = five_then(&parts[5][..100]);

        let metadata = fetch(&source("cut-moof.mp4", &file), &tolerant()).unwrap();

        assert_eq!(metadata.fragments().len(), 5);
        let dropped = metadata.dropped_tail().unwrap();
        assert_eq!(
            (dropped.fragments, dropped.bytes),
            (0, (file.len() - whole) as u64)
        );
    }

    #[test]
    fn a_partial_box_header_at_the_end_is_dropped_when_tolerated() {
        let (file, _) = five_then(&[0, 0, 1]);

        let metadata = fetch(&source("cut-header.mp4", &file), &tolerant()).unwrap();

        assert_eq!(metadata.fragments().len(), 5);
        assert_eq!(metadata.dropped_tail().unwrap().bytes, 3);
    }

    #[test]
    fn a_cut_file_is_refused_by_default_with_a_message_that_says_what_to_do() {
        let (_, parts) = subsegments(6, 1);
        let (file, _) = five_then(&parts[5][..100]);

        let error = fetch(&source("cut-default.mp4", &file), &LimitsConfig::default()).unwrap_err();

        assert!(error.to_string().contains("moof"), "{error}");
        assert!(
            error.to_string().contains("limits.tolerate_truncated_tail"),
            "{error}"
        );
    }

    #[test]
    fn a_complete_file_has_nothing_dropped_whatever_the_setting() {
        let (head, parts) = subsegments(6, 1);
        let file = [head, parts.concat()].concat();

        let metadata = fetch(&source("whole.mp4", &file), &tolerant()).unwrap();

        assert_eq!(metadata.fragments().len(), 6);
        assert!(metadata.dropped_tail().is_none());
    }

    #[test]
    fn tolerance_does_not_rescue_a_file_with_no_complete_fragment() {
        // Nothing to serve: cut inside the very first fragment.
        let (head, parts) = subsegments(6, 1);
        let file = [head, parts[0][..50].to_vec()].concat();

        let error = fetch(&source("cut-first.mp4", &file), &tolerant()).unwrap_err();

        assert!(matches!(error, Error::InvalidMedia(_)), "{error}");
    }

    #[test]
    fn tolerance_does_not_rescue_a_cut_moov_or_a_progressive_file() {
        let mut cut_moov = boxed(*b"ftyp", 8, 1);
        cut_moov.extend(&boxed(*b"moov", 100, 2)[..40]);
        let mut progressive = boxed(*b"ftyp", 8, 1);
        progressive.extend(boxed(*b"moov", 100, 2));
        progressive.extend(&boxed(*b"mdat", 5000, 9)[..2000]);

        for (name, file) in [
            ("cut-moov.mp4", cut_moov),
            ("cut-progressive.mp4", progressive),
        ] {
            assert!(fetch(&source(name, &file), &tolerant()).is_err(), "{name}");
        }
    }

    #[test]
    fn a_malformed_header_is_corruption_and_not_a_cut() {
        // A size below eight is not a box that ran off the end of the file.
        let (mut file, _) = five_then(&[]);
        file.extend(&4u32.to_be_bytes());
        file.extend(b"moof");
        file.extend([0; 20]);

        assert!(fetch(&source("garbage.mp4", &file), &tolerant()).is_err());
    }

    #[test]
    fn a_cut_file_with_a_sidx_beyond_its_end_falls_back_to_the_walk() {
        let (head, parts) = subsegments(8, 1);
        let full = with_sidx(&head, &parts, &sizes_of(&parts), 0);
        // Cut inside the last subsegment: the sidx now describes bytes that are not there.
        let file = &full[..full.len() - 9000];

        let metadata = fetch(&source("cut-sidx.mp4", file), &tolerant()).unwrap();

        assert_eq!(metadata.discovery(), Discovery::Sequential);
        assert_eq!(metadata.fragments().len(), 7);
        assert_eq!(metadata.dropped_tail().unwrap().fragments, 1);
    }

    // ---- `sidx` discovery ---------------------------------------------------------------

    /// A box with exactly this payload.
    fn boxed_with(name: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(payload);
        bytes
    }

    /// A version 0 `sidx` listing subsegments of these sizes, `first_offset` bytes after it.
    /// A size with its top bit set is a reference to another `sidx`.
    fn sidx(first_offset: u32, sizes: &[u32]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0];
        payload.extend(1u32.to_be_bytes());
        payload.extend(1000u32.to_be_bytes());
        payload.extend(0u32.to_be_bytes());
        payload.extend(first_offset.to_be_bytes());
        payload.extend([0, 0]);
        payload.extend(u16::try_from(sizes.len()).unwrap().to_be_bytes());
        for size in sizes {
            payload.extend(size.to_be_bytes());
            payload.extend(1000u32.to_be_bytes());
            payload.extend(0x9000_0000u32.to_be_bytes());
        }
        boxed_with(*b"sidx", &payload)
    }

    /// The parts of a fragmented file: everything before its fragments, and each subsegment
    /// (`moofs_each` pairs of `moof` and `mdat`).
    fn subsegments(count: usize, moofs_each: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut head = boxed(*b"ftyp", 8, 1);
        head.extend(boxed(*b"moov", 100, 2));
        let subsegments = (0..count)
            .map(|index| {
                let mut bytes = Vec::new();
                for part in 0..moofs_each {
                    // Different sizes and contents, so a mix-up cannot go unnoticed.
                    let fill = u8::try_from((index * 3 + part) % 251).unwrap();
                    bytes.extend(boxed(*b"moof", 200 + index * 7 + part, fill));
                    bytes.extend(boxed(*b"mdat", 20_000 + index, 9));
                }
                bytes
            })
            .collect();
        (head, subsegments)
    }

    /// `head`, then a `sidx` listing `listed` sizes with `first_offset`, then every subsegment.
    fn with_sidx(
        head: &[u8],
        subsegments: &[Vec<u8>],
        sizes: &[u32],
        first_offset: u32,
    ) -> Vec<u8> {
        let mut file = head.to_vec();
        file.extend(sidx(first_offset, sizes));
        file.extend(subsegments.concat());
        file
    }

    fn sizes_of(subsegments: &[Vec<u8>]) -> Vec<u32> {
        subsegments
            .iter()
            .map(|s| u32::try_from(s.len()).unwrap())
            .collect()
    }

    fn both_ways(name: &str, file: &[u8]) -> (Metadata, Metadata) {
        let source = source(name, file);
        let limits = LimitsConfig::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fast = runtime
            .block_on(Metadata::fetch_with(&source, &limits, true))
            .unwrap();
        let slow = runtime
            .block_on(Metadata::fetch_with(&source, &limits, false))
            .unwrap();
        (fast, slow)
    }

    fn same_fragments(a: &Metadata, b: &Metadata) {
        assert_eq!(a.fragments().len(), b.fragments().len());
        for (x, y) in a.fragments().iter().zip(b.fragments()) {
            assert_eq!((x.offset, &x.bytes), (y.offset, &y.bytes));
        }
        assert_eq!(a.moov_bytes(), b.moov_bytes());
    }

    #[test]
    fn a_sidx_finds_the_same_fragments_as_walking_the_file() {
        let (head, parts) = subsegments(10, 1);
        let file = with_sidx(&head, &parts, &sizes_of(&parts), 0);

        let (fast, slow) = both_ways("sidx-ok.mp4", &file);

        assert_eq!(fast.discovery(), Discovery::Sidx);
        assert_eq!(slow.discovery(), Discovery::Sequential);
        assert_eq!(fast.fragments().len(), 10);
        same_fragments(&fast, &slow);
    }

    #[test]
    fn a_subsegment_holding_several_fragments_yields_all_of_them() {
        let (head, parts) = subsegments(6, 3);
        let file = with_sidx(&head, &parts, &sizes_of(&parts), 0);

        let (fast, slow) = both_ways("sidx-multi.mp4", &file);

        assert_eq!(fast.discovery(), Discovery::Sidx);
        assert_eq!(fast.fragments().len(), 18);
        same_fragments(&fast, &slow);
    }

    #[test]
    fn a_sidx_that_covers_only_the_start_leaves_the_rest_to_the_walk() {
        let (head, parts) = subsegments(10, 1);
        let listed = sizes_of(&parts[..6]);
        let file = with_sidx(&head, &parts, &listed, 0);

        let (fast, slow) = both_ways("sidx-partial.mp4", &file);

        assert_eq!(fast.discovery(), Discovery::Sidx);
        assert_eq!(
            fast.fragments().len(),
            10,
            "the four it did not list are still found"
        );
        same_fragments(&fast, &slow);
    }

    #[test]
    fn a_first_offset_that_skips_boxes_is_walked_not_jumped() {
        // Fragments sit between the `sidx` and the region it says the media starts at. Jumping
        // over them would lose them, so the walk must reach that region on its own or not use it.
        let (head, parts) = subsegments(8, 1);
        let gap = u32::try_from(parts[0].len() + parts[1].len()).unwrap();
        let file = with_sidx(&head, &parts, &sizes_of(&parts[2..]), gap);

        let (fast, slow) = both_ways("sidx-gap.mp4", &file);

        assert_eq!(
            fast.fragments().len(),
            8,
            "the two skipped subsegments are found"
        );
        same_fragments(&fast, &slow);
    }

    #[test]
    fn every_kind_of_wrong_sidx_falls_back_to_the_sequential_walk() {
        let (head, parts) = subsegments(8, 1);
        let honest = sizes_of(&parts);
        let mut too_big = honest.clone();
        too_big[3] += 1;
        let mut too_small = honest.clone();
        too_small[3] -= 1;
        let mut hierarchical = honest.clone();
        hierarchical[2] |= 0x8000_0000;
        let mut zero = honest.clone();
        zero[4] = 0;
        let mut past_the_end = honest.clone();
        past_the_end[7] = 1_000_000;

        for (name, sizes, first_offset) in [
            ("bigger.mp4", too_big, 0),
            ("smaller.mp4", too_small, 0),
            ("hier.mp4", hierarchical, 0),
            ("zero.mp4", zero, 0),
            ("past.mp4", past_the_end, 0),
            // Media said to start in the middle of a box.
            ("misaligned.mp4", honest, 5),
        ] {
            let file = with_sidx(&head, &parts, &sizes, first_offset);

            let (fast, slow) = both_ways(name, &file);

            assert_eq!(fast.discovery(), Discovery::Sequential, "{name}");
            same_fragments(&fast, &slow);
            assert_eq!(fast.fragments().len(), 8, "{name}");
        }
    }

    #[test]
    fn a_sidx_that_points_at_something_else_is_not_believed() {
        // Sizes that tile the file exactly, but where the boundaries are not fragment starts.
        let (head, parts) = subsegments(8, 1);
        let total: usize = parts.iter().map(Vec::len).sum();
        let shifted = vec![u32::try_from(total / 8).unwrap(); 8];
        let file = with_sidx(&head, &parts, &shifted, 0);

        let (fast, slow) = both_ways("shifted.mp4", &file);

        assert_eq!(fast.discovery(), Discovery::Sequential);
        same_fragments(&fast, &slow);
    }

    /// The `sidx` is a hint from an untrusted file. Whatever bytes it holds, discovery through it
    /// must find exactly the fragments a plain walk finds, or fail: never a different set.
    #[test]
    fn no_corruption_of_the_sidx_can_change_which_fragments_are_found() {
        let (head, parts) = subsegments(8, 1);
        let honest = with_sidx(&head, &parts, &sizes_of(&parts), 0);
        let sidx_start = head.len();
        let sidx_len = sidx(0, &sizes_of(&parts)).len();
        let limits = LimitsConfig::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut jumped, mut refused) = (0, 0);
        for round in 0..400 {
            let mut file = honest.clone();
            for _ in 0..=(next() % 3) {
                // Skip the box header: a corrupt size there is a different kind of bad file.
                let at = sidx_start + 8 + usize::try_from(next()).unwrap() % (sidx_len - 8);
                file[at] = match next() % 3 {
                    0 => 0xff,
                    1 => 0x00,
                    _ => u8::try_from(next() & 0xff).unwrap(),
                };
            }
            let source = source("sidx-corrupt.mp4", &file);
            let slow = runtime
                .block_on(Metadata::fetch_with(&source, &limits, false))
                .expect("the sequential walk never reads the sidx");
            match runtime.block_on(Metadata::fetch_with(&source, &limits, true)) {
                Ok(fast) => {
                    same_fragments(&fast, &slow);
                    jumped += usize::from(fast.discovery() == Discovery::Sidx);
                }
                Err(_) => refused += 1,
            }
            let _ = round;
        }
        // The test must exercise both the accepted and the fallback paths to mean anything.
        assert!(jumped > 0, "no corrupted sidx was still accepted");
        assert!(
            refused == 0,
            "a bad sidx must fall back, not fail the load ({refused} failed)"
        );
    }

    #[test]
    fn a_sidx_with_few_references_is_not_worth_a_parallel_fetch() {
        let (head, parts) = subsegments(3, 1);
        let file = with_sidx(&head, &parts, &sizes_of(&parts), 0);

        let (fast, _) = both_ways("sidx-few.mp4", &file);

        assert_eq!(fast.discovery(), Discovery::Sequential);
    }

    #[test]
    fn a_sidx_listing_more_fragments_than_the_limit_is_not_used() {
        let (head, parts) = subsegments(10, 1);
        let file = with_sidx(&head, &parts, &sizes_of(&parts), 0);
        let limits = LimitsConfig {
            max_fragments: 6,
            ..LimitsConfig::default()
        };

        let error = fetch(&source("sidx-limit.mp4", &file), &limits).unwrap_err();

        assert!(
            error.to_string().contains("limits.max_fragments"),
            "{error}"
        );
    }

    #[test]
    fn a_real_file_with_a_sidx_finds_the_same_fragments_either_way() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for name in ["h264-aac-fragmented-sidx.mp4", "h264-aac-fragmented.mp4"] {
            let bytes = std::fs::read(path.join(name)).unwrap();

            let (fast, slow) = both_ways(&format!("real-{name}"), &bytes);

            same_fragments(&fast, &slow);
        }
    }

    #[tokio::test]
    async fn a_remote_sidx_is_fetched_in_parallel_and_within_the_concurrency_limit() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        use crate::source::{RemoteReader, RemoteSettings};
        use crate::testutil::{MockOrigin, OriginValidator};

        let (head, parts) = subsegments(160, 1);
        let file = with_sidx(&head, &parts, &sizes_of(&parts), 0);
        let origin = MockOrigin::start().await;
        origin
            .state
            .add("m.mp4", file, OriginValidator::ETag("\"v\"".to_owned()));
        let reader = RemoteReader::new(RemoteSettings {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            max_retries: 0,
            max_inflight_reads: 64,
            allow_private_addresses: true,
        })
        .unwrap();
        let remote = MediaSourceKind::Http(Arc::new(
            reader
                .open(origin.url("m.mp4").parse().unwrap())
                .await
                .unwrap(),
        ));
        origin.state.delay_ms.store(20, Ordering::SeqCst);
        origin.state.requests.store(0, Ordering::SeqCst);
        let limits = LimitsConfig {
            metadata_concurrency: 8,
            ..LimitsConfig::default()
        };

        let started = Instant::now();
        let fast = Metadata::fetch_with(&remote, &limits, true).await.unwrap();
        let parallel = started.elapsed();
        let requests = origin.state.requests.load(Ordering::SeqCst);
        let peak = origin.state.max_in_flight.load(Ordering::SeqCst);
        let started = Instant::now();
        let slow = Metadata::fetch_with(&remote, &limits, false).await.unwrap();
        let sequential = started.elapsed();

        eprintln!(
            "DISCOVERY: 160 fragments at 20 ms: sequential {sequential:?}, sidx {parallel:?} ({requests} requests, {peak} at once)"
        );
        assert_eq!(fast.discovery(), Discovery::Sidx);
        same_fragments(&fast, &slow);
        assert!(peak > 1, "fragments were fetched one at a time");
        assert!(peak <= 8, "{peak} requests at once against a limit of 8");
        assert!(requests <= 160 + 4, "{requests} requests for 160 fragments");
        assert!(
            parallel * 3 < sequential,
            "parallel {parallel:?} against sequential {sequential:?}"
        );
    }
}
