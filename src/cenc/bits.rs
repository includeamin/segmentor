//! Reading the RBSP of a NAL unit: emulation-prevention bytes removed, each RBSP byte mapped
//! back to the NAL byte it came from, so a header's length in bits becomes a clear length in NAL
//! bytes.

use crate::error::{Error, Result};

pub(super) struct Rbsp {
    bytes: Vec<u8>,
    origin: Vec<usize>,
}

impl Rbsp {
    /// Unescapes at most `limit` NAL bytes: enough for any header read from them.
    pub(super) fn new(nal: &[u8], limit: usize) -> Self {
        let size = nal.len().min(limit);
        let mut bytes = Vec::with_capacity(size);
        let mut origin = Vec::with_capacity(size);
        let mut zeros = 0u8;
        for (index, &byte) in nal.iter().enumerate().take(limit) {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 {
                zeros.saturating_add(1)
            } else {
                0
            };
            bytes.push(byte);
            origin.push(index);
        }
        Self { bytes, origin }
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn reader(&self) -> BitReader<'_> {
        BitReader {
            data: &self.bytes,
            bit: 0,
        }
    }

    /// How many NAL bytes cover the first `bits` RBSP bits: through the byte holding the last.
    pub(super) fn nal_bytes_covering(&self, bits: usize) -> Option<usize> {
        if bits == 0 {
            return Some(0);
        }
        self.origin.get((bits - 1) / 8).map(|index| index + 1)
    }
}

pub(super) struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl BitReader<'_> {
    pub(super) const fn position(&self) -> usize {
        self.bit
    }

    pub(super) fn bit(&mut self) -> Result<bool> {
        let byte = *self.data.get(self.bit / 8).ok_or_else(truncated)?;
        let value = (byte >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        Ok(value == 1)
    }

    pub(super) fn bits(&mut self, count: usize) -> Result<u32> {
        if count > 32 {
            return Err(Error::InvalidMedia(
                "a field is wider than 32 bits".to_owned(),
            ));
        }
        let mut value = 0u64;
        for _ in 0..count {
            value = value << 1 | u64::from(self.bit()?);
        }
        u32::try_from(value).map_err(|_| truncated())
    }

    pub(super) fn skip(&mut self, count: usize) -> Result<()> {
        let end = self.bit.checked_add(count).ok_or_else(truncated)?;
        if end > self.data.len() * 8 {
            return Err(truncated());
        }
        self.bit = end;
        Ok(())
    }

    /// An unsigned exp-Golomb code (H.264 9.1).
    pub(super) fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::InvalidMedia(
                    "an exp-Golomb code is too long".to_owned(),
                ));
            }
        }
        let suffix = u64::from(self.bits(zeros)?);
        u32::try_from((1u64 << zeros) - 1 + suffix).map_err(|_| truncated())
    }

    /// A signed exp-Golomb code (H.264 9.1.1).
    pub(super) fn se(&mut self) -> Result<i64> {
        let code = i64::from(self.ue()?);
        let magnitude = (code + 1) / 2;
        Ok(if code % 2 == 1 { magnitude } else { -magnitude })
    }
}

fn truncated() -> Error {
    Error::InvalidMedia("a slice or parameter set is truncated".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_bytes_are_removed_and_mapped_back() {
        let nal = [0x65, 0x00, 0x00, 0x03, 0x01, 0xff];

        let rbsp = Rbsp::new(&nal, 64);

        assert_eq!(rbsp.bytes(), &[0x65, 0x00, 0x00, 0x01, 0xff]);
        // RBSP byte 3 (0x01) came from NAL byte 4: the 0x03 in between is skipped.
        assert_eq!(rbsp.nal_bytes_covering(32), Some(5));
        assert_eq!(rbsp.nal_bytes_covering(8), Some(1));
    }

    #[test]
    fn exp_golomb_codes_read_back() {
        // 1 | 010 | 011 | 00100 | 00101 => ue 0, 1, 2, 3; then se from 00101 (ue 4) = -2.
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let rbsp = Rbsp::new(&data, 64);
        let mut reader = rbsp.reader();

        assert_eq!(reader.ue().unwrap(), 0);
        assert_eq!(reader.ue().unwrap(), 1);
        assert_eq!(reader.ue().unwrap(), 2);
        assert_eq!(reader.ue().unwrap(), 3);
        assert_eq!(reader.se().unwrap(), -2);
        assert_eq!(reader.position(), 17);
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let rbsp = Rbsp::new(&[0x00], 64);
        let mut reader = rbsp.reader();

        assert!(reader.ue().is_err());
    }
}
