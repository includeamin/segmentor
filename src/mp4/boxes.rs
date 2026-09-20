//! A bounded walker over ISO BMFF boxes, and the checked reads every parser builds on.
//!
//! Every length is checked against the bytes actually present, so a hostile file can make a
//! parser fail but not read out of range, loop, or allocate more than its own size.

use crate::error::{Error, Result};

/// One box: its type and payload, with the header already removed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawBox<'a> {
    pub(crate) name: [u8; 4],
    pub(crate) payload: &'a [u8],
    size: usize,
}

pub(crate) fn invalid_media(message: &str) -> Error {
    Error::InvalidMedia(message.to_owned())
}

/// The child boxes of `data`, which must be exactly a run of whole boxes.
pub(crate) fn child_boxes(mut data: &[u8]) -> Result<Vec<RawBox<'_>>> {
    let mut children = Vec::new();
    while !data.is_empty() {
        let child = box_payload(data, 0)?;
        children.push(child);
        data = &data[child.size..];
    }
    Ok(children)
}

/// The first child called `name`, or an error if there is none.
pub(crate) fn required_child(data: &[u8], name: [u8; 4]) -> Result<RawBox<'_>> {
    optional_child(data, name)?.ok_or_else(|| {
        Error::InvalidMedia(format!("required MP4 box `{}` is missing", fourcc(name)))
    })
}

/// The first child called `name`, if any.
pub(crate) fn optional_child(data: &[u8], name: [u8; 4]) -> Result<Option<RawBox<'_>>> {
    Ok(child_boxes(data)?
        .into_iter()
        .find(|child| child.name == name))
}

pub(crate) fn box_payload(data: &[u8], offset: usize) -> Result<RawBox<'_>> {
    let header = data
        .get(offset..offset + 8)
        .ok_or_else(|| invalid_media("MP4 box header is truncated"))?;
    let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
    let name = header[4..8].try_into().expect("four bytes");
    let (size, header_size) = if size32 == 1 {
        let extended = data
            .get(offset + 8..offset + 16)
            .ok_or_else(|| invalid_media("extended MP4 box header is truncated"))?;
        (
            usize::try_from(u64::from_be_bytes(
                extended.try_into().expect("eight bytes"),
            ))
            .map_err(|_| invalid_media("MP4 box size does not fit in memory"))?,
            16,
        )
    } else if size32 == 0 {
        (data.len() - offset, 8)
    } else {
        (
            usize::try_from(size32).map_err(|_| invalid_media("MP4 box size is invalid"))?,
            8,
        )
    };
    if size < header_size || offset.checked_add(size).is_none_or(|end| end > data.len()) {
        return Err(invalid_media("MP4 child box size is invalid"));
    }
    Ok(RawBox {
        name,
        payload: &data[offset + header_size..offset + size],
        size,
    })
}

/// A printable spelling of a four-character code for error messages and logs.
pub(crate) fn fourcc(code: [u8; 4]) -> String {
    code.iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect()
}

/// A cursor over a payload whose every read is bounds-checked.
#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub(crate) const fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    /// Everything not yet read, without consuming it.
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.data[self.position..]
    }

    pub(crate) fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| invalid_media("MP4 box is truncated"))?;
        let bytes = &self.data[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    pub(crate) fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    pub(crate) fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    /// Reads the version and flags that open a "full box", returning the version.
    pub(crate) fn full_box(&mut self) -> Result<u8> {
        let version = self.u8()?;
        self.skip(3)?;
        Ok(version)
    }

    /// Reads the version and the 24-bit flags that open a "full box".
    pub(crate) fn full_box_flags(&mut self) -> Result<(u8, u32)> {
        let version = self.u8()?;
        let flags = self.take(3)?;
        Ok((
            version,
            u32::from_be_bytes([0, flags[0], flags[1], flags[2]]),
        ))
    }

    /// Reads a table's entry count and checks that many `entry_size`-byte entries can exist in
    /// what is left, so the count can be trusted for allocation.
    pub(crate) fn entry_count(&mut self, entry_size: usize) -> Result<usize> {
        let count = usize::try_from(self.u32()?)
            .map_err(|_| invalid_media("MP4 table entry count does not fit in memory"))?;
        if count
            .checked_mul(entry_size)
            .is_none_or(|bytes| bytes > self.remaining())
        {
            return Err(invalid_media("MP4 table entry count exceeds its box"));
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(name: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).unwrap();
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn walks_sibling_boxes() {
        let mut data = boxed(*b"aaaa", &[1, 2, 3]);
        data.extend(boxed(*b"bbbb", &[]));

        let children = child_boxes(&data).unwrap();

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, *b"aaaa");
        assert_eq!(children[0].payload, [1, 2, 3]);
        assert_eq!(children[1].payload, [] as [u8; 0]);
    }

    #[test]
    fn reads_extended_and_to_end_sizes() {
        let mut extended = 1u32.to_be_bytes().to_vec();
        extended.extend_from_slice(b"wide");
        extended.extend_from_slice(&19u64.to_be_bytes());
        extended.extend_from_slice(&[7, 8, 9]);
        assert_eq!(child_boxes(&extended).unwrap()[0].payload, [7, 8, 9]);

        let mut to_end = 0u32.to_be_bytes().to_vec();
        to_end.extend_from_slice(b"tail");
        to_end.extend_from_slice(&[4, 5]);
        assert_eq!(child_boxes(&to_end).unwrap()[0].payload, [4, 5]);
    }

    #[test]
    fn rejects_sizes_that_do_not_fit() {
        for size in [0u32.wrapping_add(4), 9999] {
            let mut data = size.to_be_bytes().to_vec();
            data.extend_from_slice(b"oops");
            data.extend_from_slice(&[0; 4]);
            assert!(child_boxes(&data).is_err(), "size {size}");
        }
        assert!(child_boxes(&[0, 0, 0]).is_err(), "truncated header");
    }

    #[test]
    fn a_missing_required_child_names_it() {
        let data = boxed(*b"aaaa", &[]);

        let error = required_child(&data, *b"zzzz").unwrap_err();

        assert!(error.to_string().contains("`zzzz`"), "{error}");
        assert!(optional_child(&data, *b"zzzz").unwrap().is_none());
    }

    #[test]
    fn reader_refuses_to_read_past_the_end() {
        let mut reader = Reader::new(&[0, 0, 0, 5, 9]);

        assert_eq!(reader.u32().unwrap(), 5);
        assert!(reader.u32().is_err());
        assert_eq!(reader.u8().unwrap(), 9);
        assert!(reader.u8().is_err());
    }

    #[test]
    fn entry_counts_cannot_exceed_the_bytes_that_follow() {
        // Three four-byte entries follow the count.
        let mut reader = Reader::new(&[0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(reader.entry_count(4).unwrap(), 3);

        let mut too_many = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
        assert!(too_many.entry_count(8).is_err());
        let mut overflow = Reader::new(&[0xff, 0xff, 0xff, 0xff]);
        assert!(overflow.entry_count(usize::MAX).is_err());
    }

    #[test]
    fn fourcc_hides_unprintable_bytes() {
        assert_eq!(fourcc(*b"avc1"), "avc1");
        assert_eq!(fourcc([0xa9, b'n', b'a', 0]), "?na?");
    }
}
