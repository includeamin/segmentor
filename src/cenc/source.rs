//! Undoing the protection of a source file (TDD 0014): the four schemes of ISO/IEC 23001-7,
//! applied to one sample at a time.
//!
//! Block ciphers are used raw, as in `cipher`, because a library mode would chain or count
//! through the bytes a pattern skips.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use super::keys::uuid_string;
use super::source_keys::SourceKeys;
use crate::error::{Error, Result};
use crate::media::{
    CodecConfig, SourceProtection, SourceScheme, Subsample, Track, TrackKind, iv16,
};
use crate::source::{ByteRange, MediaSourceKind, Metadata};

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

/// Undoes the protection of one track: its per-sample data, and the cipher for its key.
pub(crate) struct TrackDecryption {
    decryptor: SampleDecryptor,
    protection: Arc<SourceProtection>,
}

impl fmt::Debug for TrackDecryption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrackDecryption")
            .field("scheme", &self.protection.scheme)
            .field("key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl TrackDecryption {
    /// Decrypts `data`, the bytes of the track's sample `index` counted from the first sample of
    /// the file's tables (see `SampleIndex::window_start`), in place.
    pub(crate) fn decrypt_sample(&self, index: usize, data: &mut [u8]) -> Result<()> {
        let aux = &self.protection.aux;
        let iv = match aux.iv(index) {
            Some(iv) => iv16(iv),
            None => self.protection.constant_iv,
        }
        .ok_or_else(|| Error::InvalidMedia(format!("sample {index} has no IV")))?;
        self.decryptor.decrypt(&iv, data, aux.subsamples(index))
    }
}

impl TrackDecryption {
    /// Whether the NAL unit length prefix at the start of sample `index` is left clear, so that
    /// decrypting the sample cannot show whether the key was right.
    fn leaves_prefix_clear(&self, index: usize, length_size: usize) -> bool {
        self.protection
            .aux
            .subsamples(index)
            .first()
            .is_some_and(|first| usize::from(first.clear) >= length_size)
    }
}

/// The tracks of an asset whose source protection is undone while serving (TDD 0014).
#[derive(Debug)]
pub(crate) struct AssetDecryption {
    tracks: HashMap<u32, Arc<TrackDecryption>>,
    /// Key IDs of the protected tracks, in UUID form, for status reporting.
    key_ids: Vec<String>,
}

impl AssetDecryption {
    /// `None` when no track is protected. A protected track the mapper gave no key for fails the
    /// asset with the message an encrypted source has always had when nothing can read it.
    pub(crate) fn new(tracks: &[Track], keys: Option<&SourceKeys>) -> Result<Option<Self>> {
        let mut decryptions = HashMap::new();
        let mut key_ids: Vec<String> = Vec::new();
        for track in tracks {
            let Some(protection) = &track.source_protection else {
                continue;
            };
            let Some(keys) = keys else {
                return Err(Error::Unsupported(format!(
                    "track {}: encrypted media is not supported",
                    track.id
                )));
            };
            let key_id = uuid_string(&protection.key_id);
            let key = keys
                .key_for(&protection.key_id, track.kind)
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "track {}: source_encryption has no key for key ID {key_id}",
                        track.id
                    ))
                })?;
            decryptions.insert(
                track.id,
                Arc::new(TrackDecryption {
                    decryptor: SampleDecryptor::new(
                        key.bytes(),
                        protection.scheme,
                        protection.crypt_blocks,
                        protection.skip_blocks,
                    ),
                    protection: Arc::clone(protection),
                }),
            );
            if !key_ids.contains(&key_id) {
                key_ids.push(key_id);
            }
        }
        Ok((!decryptions.is_empty()).then_some(Self {
            tracks: decryptions,
            key_ids,
        }))
    }

    pub(crate) fn track(&self, id: u32) -> Option<&Arc<TrackDecryption>> {
        self.tracks.get(&id)
    }

    pub(crate) fn key_ids(&self) -> &[String] {
        &self.key_ids
    }

    /// Memory held by the per-sample tables, for the index budget.
    pub(crate) fn bytes(&self) -> usize {
        self.tracks
            .values()
            .map(|track| track.protection.aux.bytes())
            .sum()
    }

    /// A key decrypts to garbage without complaint, and a CDN would keep the garbage. Where it
    /// can, this notices: the first sample of a video track is decrypted now, and its NAL unit
    /// lengths must tile it exactly, which random bytes will not do.
    ///
    /// That only works when the length prefixes are themselves encrypted. Real encoders leave
    /// each NAL unit's length and header clear and protect the rest, and decrypted slice data
    /// looks like random bytes whether or not the key is right, so for those files (and for
    /// codecs without NAL units) a wrong key cannot be told from a right one here.
    pub(crate) async fn check_keys(
        &self,
        source: &MediaSourceKind,
        metadata: &Metadata,
        tracks: &[Track],
    ) -> Result<()> {
        for track in tracks.iter().filter(|track| track.kind == TrackKind::Video) {
            let Some(decryption) = self.tracks.get(&track.id) else {
                continue;
            };
            let (_, entry) = crate::fmp4::sample_entry(metadata, track.id)?;
            let Some(length_size) = nal_length_size(&entry, &track.codec) else {
                continue;
            };
            let first = track.samples.get(0);
            if decryption.leaves_prefix_clear(track.samples.window_start(), length_size) {
                continue;
            }
            let mut sample = source
                .read_range(ByteRange::new(first.offset, u64::from(first.size)))
                .await?
                .to_vec();
            decryption.decrypt_sample(track.samples.window_start(), &mut sample)?;
            if !nal_units_tile(&sample, length_size) {
                return Err(Error::InvalidMedia(format!(
                    "track {}: the source_encryption key does not decrypt it",
                    track.id
                )));
            }
        }
        Ok(())
    }
}

/// The size of the length prefix of a NAL unit, from the codec's configuration box.
fn nal_length_size(entry: &[u8], codec: &CodecConfig) -> Option<usize> {
    let (name, offset) = match codec {
        CodecConfig::Avc { .. } => (*b"avcC", 4),
        CodecConfig::Hevc { .. } => (*b"hvcC", 21),
        _ => return None,
    };
    let (_, _, children) = crate::mp4::codec::visual_entry(entry).ok()?;
    let config = crate::mp4::boxes::optional_child(children, name).ok()??;
    Some(usize::from(config.payload.get(offset)? & 3) + 1)
}

/// Whether length-prefixed NAL units cover `sample` exactly, with none empty.
fn nal_units_tile(sample: &[u8], length_size: usize) -> bool {
    let mut at = 0;
    while at < sample.len() {
        let Some(prefix) = sample.get(at..at + length_size) else {
            return false;
        };
        let size = prefix
            .iter()
            .fold(0usize, |size, byte| (size << 8) | usize::from(*byte));
        if size == 0 {
            return false;
        }
        at += length_size + size;
    }
    at == sample.len()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn nal_units_must_tile_the_sample_exactly() {
        // Two NAL units of 3 and 2 bytes after 4-byte lengths.
        let good = [0, 0, 0, 3, 1, 2, 3, 0, 0, 0, 2, 9, 9];
        assert!(nal_units_tile(&good, 4));
        assert!(
            !nal_units_tile(&good[..12], 4),
            "the last NAL unit is cut short"
        );
        assert!(
            !nal_units_tile(&[0, 0, 0, 3, 1, 2, 3, 0, 0], 4),
            "a prefix cut short"
        );
        assert!(
            !nal_units_tile(&[0, 0, 0, 0, 0, 0, 0, 1, 1], 4),
            "an empty NAL unit"
        );
        assert!(
            !nal_units_tile(&[0xff; 12], 4),
            "random bytes claim sizes far past the end"
        );
        assert!(nal_units_tile(&[0, 2, 1, 1], 2));
    }

    #[test]
    fn an_iv_is_8_or_16_bytes() {
        assert_eq!(iv16(&[1; 8]).unwrap()[8..], [0; 8]);
        assert_eq!(iv16(&[2; 16]).unwrap(), [2; 16]);
        assert!(iv16(&[3; 12]).is_none());
        assert!(iv16(&[]).is_none());
    }
}
