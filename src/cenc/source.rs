//! Undoing the protection of a source file (TDD 0014): the four schemes of ISO/IEC 23001-7,
//! applied to one sample at a time.
//!
//! Block ciphers are used raw, as in `cipher`, because a library mode would chain or count
//! through the bytes a pattern skips.

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use crate::error::{Error, Result};
use crate::media::{SourceScheme, Subsample};

/// Decrypts samples of one scheme under one key.
pub(crate) struct SampleDecryptor {
    cipher: Aes128,
    scheme: SourceScheme,
    crypt: u8,
    skip: u8,
}

impl SampleDecryptor {
    /// `crypt` and `skip` are the `tenc` pattern, used by `cens` and `cbcs` only. A pattern of
    /// 0:0 selects every block, which is how `cbcs` audio is protected.
    pub(crate) fn new(key: &[u8; 16], scheme: SourceScheme, crypt: u8, skip: u8) -> Self {
        Self {
            cipher: Aes128::new(&(*key).into()),
            scheme,
            crypt,
            skip,
        }
    }

    /// Decrypts `data`, one whole sample, in place. `iv` is 16 bytes (an 8-byte IV padded with
    /// zeros); `subsamples` empty means the whole sample is protected.
    pub(crate) fn decrypt(
        &self,
        iv: &[u8; 16],
        data: &mut [u8],
        subsamples: &[Subsample],
    ) -> Result<()> {
        let ranges = protected_ranges(data.len(), subsamples)?;
        match self.scheme {
            SourceScheme::Cenc => {
                let mut stream = CtrStream::new(&self.cipher, iv);
                for range in ranges {
                    stream.apply(&mut data[range]);
                }
            }
            SourceScheme::Cens => {
                let mut stream = CtrStream::new(&self.cipher, iv);
                for range in ranges {
                    for (index, block) in data[range].as_chunks_mut::<16>().0.iter_mut().enumerate()
                    {
                        if self.selects(index) {
                            stream.apply(block);
                        }
                    }
                }
            }
            SourceScheme::Cbc1 => {
                let mut chain = *iv;
                for range in ranges {
                    for block in data[range].as_chunks_mut::<16>().0 {
                        self.cbc_block(block, &mut chain);
                    }
                }
            }
            SourceScheme::Cbcs => {
                for range in ranges {
                    let mut chain = *iv;
                    for (index, block) in data[range].as_chunks_mut::<16>().0.iter_mut().enumerate()
                    {
                        if self.selects(index) {
                            self.cbc_block(block, &mut chain);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the pattern protects block `index` of a range.
    fn selects(&self, index: usize) -> bool {
        let period = usize::from(self.crypt) + usize::from(self.skip);
        period == 0 || index % period < usize::from(self.crypt)
    }

    /// One CBC block: the plaintext is the decrypted block XOR the previous ciphertext block.
    fn cbc_block(&self, block: &mut [u8; 16], chain: &mut [u8; 16]) {
        let ciphertext = *block;
        self.cipher.decrypt_block(block.into());
        for (byte, previous) in block.iter_mut().zip(*chain) {
            *byte ^= previous;
        }
        *chain = ciphertext;
    }
}

/// The protected byte ranges of a sample of `length` bytes, which the subsamples must cover
/// exactly.
fn protected_ranges(
    length: usize,
    subsamples: &[Subsample],
) -> Result<Vec<std::ops::Range<usize>>> {
    if subsamples.is_empty() {
        let whole = 0..length;
        return Ok(vec![whole]);
    }
    let mismatch = || {
        Error::InvalidMedia("subsample sizes do not add up to the size of the sample".to_owned())
    };
    let mut ranges = Vec::with_capacity(subsamples.len());
    let mut at = 0usize;
    for subsample in subsamples {
        let start = at + usize::from(subsample.clear);
        let end = start
            .checked_add(subsample.protected as usize)
            .filter(|end| *end <= length)
            .ok_or_else(mismatch)?;
        ranges.push(start..end);
        at = end;
    }
    if at != length {
        return Err(mismatch());
    }
    Ok(ranges)
}

/// AES-CTR keystream that carries on from one call to the next, partial blocks included.
struct CtrStream<'a> {
    cipher: &'a Aes128,
    counter: u128,
    keystream: [u8; 16],
    used: usize,
}

impl<'a> CtrStream<'a> {
    fn new(cipher: &'a Aes128, iv: &[u8; 16]) -> Self {
        Self {
            cipher,
            counter: u128::from_be_bytes(*iv),
            keystream: [0; 16],
            used: 16,
        }
    }

    fn apply(&mut self, mut data: &mut [u8]) {
        while !data.is_empty() {
            if self.used == 16 {
                self.keystream = self.counter.to_be_bytes();
                self.cipher.encrypt_block((&mut self.keystream).into());
                self.counter = self.counter.wrapping_add(1);
                self.used = 0;
            }
            let take = (16 - self.used).min(data.len());
            let (now, later) = data.split_at_mut(take);
            for (byte, key) in now.iter_mut().zip(&self.keystream[self.used..]) {
                *byte ^= key;
            }
            self.used += take;
            data = later;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::iv16;

    const KEY: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const VECTORS: &str = include_str!("../../tests/fixtures/source-cipher-vectors.json");

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    fn scheme(name: &str) -> SourceScheme {
        SourceScheme::from_fourcc(name.as_bytes().try_into().unwrap()).unwrap()
    }

    fn subsamples(vector: &serde_json::Value) -> Vec<Subsample> {
        vector["subsamples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| Subsample {
                clear: u16::try_from(pair[0].as_u64().unwrap()).unwrap(),
                protected: u32::try_from(pair[1].as_u64().unwrap()).unwrap(),
            })
            .collect()
    }

    fn decryptor(vector: &serde_json::Value, key: &[u8; 16]) -> SampleDecryptor {
        SampleDecryptor::new(
            key,
            scheme(vector["scheme"].as_str().unwrap()),
            u8::try_from(vector["crypt"].as_u64().unwrap()).unwrap(),
            u8::try_from(vector["skip"].as_u64().unwrap()).unwrap(),
        )
    }

    fn plain(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect()
    }

    /// Samples protected by the openssl-based reference encryptor
    /// (`tests/fixtures/generate-source-vectors.py`), which shares no code with this module.
    #[test]
    fn every_scheme_recovers_what_the_reference_encryptor_protected() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let vectors = vectors.as_object().unwrap();
        assert_eq!(vectors.len(), 6);
        for (name, vector) in vectors {
            let length = usize::try_from(vector["length"].as_u64().unwrap()).unwrap();
            let mut data = hex(vector["cipher"].as_str().unwrap());
            assert_eq!(data.len(), length, "{name}");
            assert_ne!(data, plain(length), "{name}: the vector is protected");

            let iv = iv16(&hex(vector["iv"].as_str().unwrap())).unwrap();
            decryptor(vector, &KEY)
                .decrypt(&iv, &mut data, &subsamples(vector))
                .unwrap();

            assert_eq!(data, plain(length), "{name}");
        }
    }

    #[test]
    fn another_key_does_not_recover_the_sample() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        for (name, vector) in vectors.as_object().unwrap() {
            let mut data = hex(vector["cipher"].as_str().unwrap());
            let iv = iv16(&hex(vector["iv"].as_str().unwrap())).unwrap();
            let mut other = KEY;
            other[0] ^= 1;
            decryptor(vector, &other)
                .decrypt(&iv, &mut data, &subsamples(vector))
                .unwrap();
            assert_ne!(data, plain(data.len()), "{name}");
        }
    }

    #[test]
    fn an_iv_off_by_one_bit_does_not_recover_the_sample() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        for (name, vector) in vectors.as_object().unwrap() {
            let mut data = hex(vector["cipher"].as_str().unwrap());
            let mut iv = iv16(&hex(vector["iv"].as_str().unwrap())).unwrap();
            iv[0] ^= 1;
            decryptor(vector, &KEY)
                .decrypt(&iv, &mut data, &subsamples(vector))
                .unwrap();
            assert_ne!(data, plain(data.len()), "{name}");
        }
    }

    #[test]
    fn subsamples_that_do_not_cover_the_sample_are_refused() {
        let decryptor = SampleDecryptor::new(&KEY, SourceScheme::Cenc, 0, 0);
        let mut data = vec![0; 100];
        let short = [Subsample {
            clear: 10,
            protected: 50,
        }];
        let long = [Subsample {
            clear: 10,
            protected: 91,
        }];
        for subsamples in [&short[..], &long[..]] {
            let error = decryptor
                .decrypt(&[0; 16], &mut data, subsamples)
                .unwrap_err();
            assert!(error.to_string().contains("subsample"), "{error}");
        }
    }

    #[test]
    fn an_iv_is_8_or_16_bytes() {
        assert_eq!(iv16(&[1; 8]).unwrap()[8..], [0; 8]);
        assert_eq!(iv16(&[2; 16]).unwrap(), [2; 16]);
        assert!(iv16(&[3; 12]).is_none());
        assert!(iv16(&[]).is_none());
    }
}
