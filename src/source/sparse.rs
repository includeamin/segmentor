//! The parts of a file the MP4 metadata parser needs, held in memory.
//!
//! The `mp4` crate wants `Read + Seek` over the whole file, but it only reads box headers and
//! the `ftyp` and `moov` payloads; it seeks past `mdat`. `SparseFile` fetches exactly those
//! regions (a few kilobytes to a few megabytes) and presents them as a file whose other bytes
//! fail to read. That makes metadata parsing independent of where the file lives: a remote
//! origin costs a handful of small range requests instead of a download.

use std::io::{self, Read, Seek, SeekFrom};

use bytes::Bytes;

use super::{ByteRange, MediaSourceKind};
use crate::error::{Error, Result};

/// A well-formed file has a handful of top-level boxes. Refusing more keeps a hostile file from
/// turning metadata discovery into millions of round trips.
const MAX_TOP_LEVEL_BOXES: usize = 4096;

/// `ftyp` is a few dozen bytes in practice.
const MAX_FTYP_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub(crate) struct SparseFile {
    len: u64,
    /// Non-overlapping regions in ascending offset order.
    regions: Vec<(u64, Bytes)>,
    moov: ByteRange,
}

impl SparseFile {
    /// Walks the top-level boxes with small reads and fetches `ftyp` and `moov` whole.
    pub(crate) async fn fetch(source: &MediaSourceKind, max_metadata_bytes: u64) -> Result<Self> {
        let len = source.len();
        let mut regions: Vec<(u64, Bytes)> = Vec::new();
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
            let (size, header_size, mut header_bytes) = match size32 {
                1 => {
                    let extended = source.read_range(ByteRange::new(offset + 8, 8)).await?;
                    let size = u64::from_be_bytes(extended[..8].try_into().expect("eight bytes"));
                    let mut both = header.to_vec();
                    both.extend_from_slice(&extended);
                    (size, 16, both)
                }
                0 => (len - offset, 8, header.to_vec()),
                size => (u64::from(size), 8, header.to_vec()),
            };
            if size < header_size || offset.checked_add(size).is_none_or(|end| end > len) {
                return Err(invalid("invalid top-level MP4 box size"));
            }
            let fetch_whole = match &name {
                b"moov" => {
                    if size > max_metadata_bytes {
                        return Err(invalid("moov exceeds configured metadata limit"));
                    }
                    moov = Some(ByteRange::new(offset, size));
                    true
                }
                b"ftyp" => size <= MAX_FTYP_BYTES,
                _ => false,
            };
            if fetch_whole && size > header_size {
                let rest = source
                    .read_range(ByteRange::new(offset + header_size, size - header_size))
                    .await?;
                header_bytes.extend_from_slice(&rest);
            }
            regions.push((offset, Bytes::from(header_bytes)));
            offset += size;
        }
        let moov = moov.ok_or_else(|| invalid("missing moov box"))?;
        Ok(Self { len, regions, moov })
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
        let (_, bytes) = self
            .regions
            .iter()
            .find(|(offset, _)| *offset == self.moov.offset)
            .expect("fetch always records the moov region");
        bytes
    }

    pub(crate) fn reader(&self) -> SparseReader<'_> {
        SparseReader {
            file: self,
            position: 0,
        }
    }

    fn region_at(&self, position: u64) -> Option<(u64, &Bytes)> {
        self.regions
            .iter()
            .find(|(offset, bytes)| position >= *offset && position < offset + bytes.len() as u64)
            .map(|(offset, bytes)| (*offset, bytes))
    }
}

#[derive(Debug)]
pub(crate) struct SparseReader<'a> {
    file: &'a SparseFile,
    position: u64,
}

impl Read for SparseReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.file.len {
            return Ok(0);
        }
        let Some((offset, bytes)) = self.file.region_at(self.position) else {
            return Err(io::Error::other(
                "parser read outside the fetched metadata regions",
            ));
        };
        let start = usize::try_from(self.position - offset).expect("region offsets fit in memory");
        let count = buffer.len().min(bytes.len() - start);
        buffer[..count].copy_from_slice(&bytes[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for SparseReader<'_> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let next = match target {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::End(delta) => self.file.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        self.position = next;
        Ok(next)
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidMedia(message.to_owned())
}
