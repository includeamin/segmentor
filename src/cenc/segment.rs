//! Encrypting one segment: which bytes of each sample are protected, the encryption itself, and
//! the fragment that describes it.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use super::aes128::Aes128;
use super::avc::AvcParameters;
use super::cipher::{Cipher, Pattern};
use super::hevc::HevcParameters;
use super::keys::{ContentKey, Encryption};
use crate::error::{Error, Result};
use crate::fmp4::{self, InitProtection, MAX_SUBSAMPLES};
use crate::media::{CodecConfig, Sample, Track, TrackKind};
use crate::source::Metadata;

/// What a track needs to encrypt its segments.
#[derive(Debug)]
pub(crate) struct TrackProtection {
    pub(crate) key: ContentKey,
    pub(crate) kind: TrackKind,
    /// Parameter sets, for finding slice headers; `None` for audio.
    pub(crate) video: Option<VideoParameters>,
}

/// The parameter sets of a video track, by codec.
#[derive(Debug)]
pub(crate) enum VideoParameters {
    Avc(AvcParameters),
    Hevc(HevcParameters),
}

impl TrackProtection {
    pub(crate) const fn pattern(&self) -> Pattern {
        match self.kind {
            TrackKind::Video => Pattern::VIDEO,
            TrackKind::Audio => Pattern::FULL,
        }
    }

    pub(crate) fn init<'a>(&self, pssh: &'a [Vec<u8>]) -> InitProtection<'a> {
        let pattern = self.pattern();
        InitProtection {
            key_id: self.key.key_id,
            constant_iv: self.key.iv,
            crypt_byte_block: pattern.crypt,
            skip_byte_block: pattern.skip,
            pssh,
        }
    }
}

/// An encrypted asset's per-track protection, built once when it loads.
#[derive(Debug)]
pub(crate) struct AssetProtection {
    pub(crate) encryption: Arc<Encryption>,
    tracks: HashMap<u32, Arc<TrackProtection>>,
    /// Every `pssh` the mapper supplied, for each init segment's `moov`.
    pub(crate) pssh: Vec<Vec<u8>>,
}

impl AssetProtection {
    /// Fails for a codec this stage cannot encrypt, naming it (TDD 0009, "Security and limits").
    pub(crate) fn new(
        encryption: Arc<Encryption>,
        tracks: &[Track],
        metadata: &Metadata,
    ) -> Result<Self> {
        let mut protected = HashMap::with_capacity(tracks.len());
        for track in tracks {
            let video = match &track.codec {
                CodecConfig::Avc { .. } => {
                    let (_, entry) = fmp4::sample_entry(metadata, track.id)?;
                    Some(VideoParameters::Avc(AvcParameters::from_sample_entry(
                        &entry,
                    )?))
                }
                CodecConfig::Hevc { .. } => {
                    let (_, entry) = fmp4::sample_entry(metadata, track.id)?;
                    Some(VideoParameters::Hevc(HevcParameters::from_sample_entry(
                        &entry,
                    )?))
                }
                CodecConfig::Aac { .. } | CodecConfig::Ac3 { .. } | CodecConfig::Eac3 { .. } => {
                    None
                }
                other => {
                    return Err(Error::Unsupported(format!(
                        "{} cannot be encrypted",
                        other.codecs()
                    )));
                }
            };
            protected.insert(
                track.id,
                Arc::new(TrackProtection {
                    key: encryption.key_for(track.kind).clone(),
                    kind: track.kind,
                    video,
                }),
            );
        }
        let pssh = encryption
            .systems
            .iter()
            .filter_map(|system| system.pssh.clone())
            .collect();
        Ok(Self {
            encryption,
            tracks: protected,
            pssh,
        })
    }

    pub(crate) fn track(&self, id: u32) -> Option<&Arc<TrackProtection>> {
        self.tracks.get(&id)
    }
}

/// A segment that must be encrypted once its bytes are read (TDD 0009, "The encrypted segment
/// path"): a `cbcs` track's samples, or a whole `AES-128` segment.
#[derive(Debug, Clone)]
pub(crate) enum PendingEncryption {
    Cbcs(PendingCbcs),
    /// Header and payload encrypted together as one message (TDD 0012).
    WholeSegment {
        /// The fragment's `moof` and `mdat` header, which the payload follows.
        header: Bytes,
        segment_index: u32,
        key: Arc<Aes128>,
    },
}

impl PendingEncryption {
    /// Room to reserve ahead of the payload, so [`Self::finish`] can build the fragment in the
    /// same buffer the payload was read into.
    pub(crate) fn header_room(&self) -> usize {
        match self {
            Self::Cbcs(pending) => pending.header_room(),
            // The header, and up to a block of padding.
            Self::WholeSegment { header, .. } => header.len() + 16,
        }
    }

    /// Encrypts `payload` (the samples' bytes, in order) and returns the finished fragment.
    pub(crate) fn finish(&self, payload: Vec<u8>) -> Result<Bytes> {
        match self {
            Self::Cbcs(pending) => pending.finish(payload),
            Self::WholeSegment {
                header,
                segment_index,
                key,
            } => {
                let mut message = Vec::with_capacity(header.len() + payload.len() + 16);
                message.extend_from_slice(header);
                message.extend_from_slice(&payload);
                key.encrypt_segment(*segment_index, &mut message);
                Ok(Bytes::from(message))
            }
        }
    }
}

/// A segment of a `cbcs` track, prepared up to the point where its bytes are needed.
#[derive(Debug, Clone)]
pub(crate) struct PendingCbcs {
    pub(crate) track_id: u32,
    pub(crate) kind: TrackKind,
    pub(crate) samples: Vec<Sample>,
    pub(crate) decode_time: u64,
    pub(crate) sequence_number: u32,
    pub(crate) protection: Arc<TrackProtection>,
}

impl PendingCbcs {
    /// Room to reserve ahead of the payload for the fragment header, so [`Self::finish`] can put
    /// the header in front without copying the payload into a second buffer. An estimate: a
    /// sample's `trun`, `senc` (a few subsamples), and `saiz` entries, plus the fixed boxes; a
    /// larger header only costs a reallocation.
    pub(crate) fn header_room(&self) -> usize {
        self.samples.len().saturating_mul(64).saturating_add(1024)
    }

    /// Encrypts `payload` (the samples' bytes, in order) and returns the finished fragment.
    pub(crate) fn finish(&self, mut payload: Vec<u8>) -> Result<Bytes> {
        let invalid = |message: &str| Error::InvalidMedia(message.to_owned());
        let expected = self.samples.iter().try_fold(0usize, |total, sample| {
            usize::try_from(sample.size)
                .ok()
                .and_then(|size| total.checked_add(size))
        });
        if expected != Some(payload.len()) {
            return Err(invalid("encrypted segment bytes do not match its samples"));
        }
        let cipher = Cipher::new(self.protection.key.key.bytes());
        let iv = self.protection.key.iv;
        let pattern = self.protection.pattern();
        let mut maps = Vec::with_capacity(self.samples.len());
        let mut offset = 0;
        for sample in &self.samples {
            let size = usize::try_from(sample.size).map_err(|_| invalid("sample too large"))?;
            let bytes = &mut payload[offset..offset + size];
            let map = match &self.protection.video {
                Some(VideoParameters::Avc(avc)) => avc_subsamples(avc, bytes)?,
                Some(VideoParameters::Hevc(hevc)) => hevc_subsamples(hevc, bytes)?,
                None => Vec::new(),
            };
            if map.is_empty() {
                cipher.encrypt_range(&iv, bytes, pattern);
            } else {
                let mut at = 0;
                for &(clear, protected) in &map {
                    at += usize::from(clear);
                    let end =
                        at + usize::try_from(protected).map_err(|_| invalid("range too large"))?;
                    cipher.encrypt_range(&iv, &mut bytes[at..end], pattern);
                    at = end;
                }
            }
            maps.push(map);
            offset += size;
        }
        let header = fmp4::encrypted_fragment_header(
            self.track_id,
            self.kind,
            &self.samples,
            self.decode_time,
            self.sequence_number,
            &maps,
            payload.len(),
        )?;
        // Put the header in front within the payload's own buffer (room is usually reserved by
        // the caller), rather than copying the whole payload after the header.
        payload.reserve_exact(header.len());
        payload.splice(0..0, header);
        Ok(Bytes::from(payload))
    }
}

/// One H.264 sample's `(clear, protected)` subsamples (TDD 0009, "What is encrypted"): every NAL
/// unit that is not a slice joins the next clear run, and each slice's header stays clear.
pub(super) fn avc_subsamples(parameters: &AvcParameters, sample: &[u8]) -> Result<Vec<(u16, u32)>> {
    let invalid = |message: &str| Error::InvalidMedia(message.to_owned());
    let length_size = parameters.nal_length_size();
    let mut in_band: Option<AvcParameters> = None;
    let mut map = Vec::new();
    let mut clear = 0usize;
    let mut at = 0usize;
    while at < sample.len() {
        let prefix = sample
            .get(at..at + length_size)
            .ok_or_else(|| invalid("a NAL length is truncated"))?;
        let length = prefix
            .iter()
            .fold(0usize, |value, byte| value << 8 | usize::from(*byte));
        let start = at + length_size;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= sample.len())
            .ok_or_else(|| invalid("a NAL unit runs past its sample"))?;
        let nal = &sample[start..end];
        let nal_type = nal.first().ok_or_else(|| invalid("an empty NAL unit"))? & 0x1f;
        match nal_type {
            1 | 5 => {
                let header = in_band.as_ref().unwrap_or(parameters).clear_bytes(nal)?;
                push_subsample(&mut map, clear + length_size + header, length - header)?;
                clear = 0;
            }
            2..=4 => {
                return Err(Error::Unsupported(
                    "H.264 data partitioning cannot be encrypted".to_owned(),
                ));
            }
            7 | 8 => {
                in_band
                    .get_or_insert_with(|| parameters.clone())
                    .update(nal)?;
                clear += length_size + length;
            }
            _ => clear += length_size + length,
        }
        at = end;
    }
    if clear > 0 {
        push_subsample(&mut map, clear, 0)?;
    }
    if map.len() > MAX_SUBSAMPLES {
        return Err(invalid(
            "a sample has more than 42 subsamples, more than saiz can describe",
        ));
    }
    Ok(map)
}

/// One H.265 sample's `(clear, protected)` subsamples (TDD 0009, "What is encrypted"): every NAL
/// unit that is not a slice segment joins the next clear run, and each slice segment's header stays
/// clear, as for H.264.
pub(super) fn hevc_subsamples(
    parameters: &HevcParameters,
    sample: &[u8],
) -> Result<Vec<(u16, u32)>> {
    let invalid = |message: &str| Error::InvalidMedia(message.to_owned());
    let length_size = parameters.nal_length_size();
    let mut in_band: Option<HevcParameters> = None;
    let mut map = Vec::new();
    let mut clear = 0usize;
    let mut at = 0usize;
    while at < sample.len() {
        let prefix = sample
            .get(at..at + length_size)
            .ok_or_else(|| invalid("a NAL length is truncated"))?;
        let length = prefix
            .iter()
            .fold(0usize, |value, byte| value << 8 | usize::from(*byte));
        let start = at + length_size;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= sample.len())
            .ok_or_else(|| invalid("a NAL unit runs past its sample"))?;
        let nal = &sample[start..end];
        let nal_type = (nal.first().ok_or_else(|| invalid("an empty NAL unit"))? >> 1) & 0x3f;
        match nal_type {
            0..=9 | 16..=21 => {
                let header = in_band.as_ref().unwrap_or(parameters).clear_bytes(nal)?;
                push_subsample(&mut map, clear + length_size + header, length - header)?;
                clear = 0;
            }
            10..=15 | 22..=31 => {
                return Err(Error::Unsupported(format!(
                    "H.265 NAL unit type {nal_type} cannot be encrypted"
                )));
            }
            32..=34 => {
                in_band
                    .get_or_insert_with(|| parameters.clone())
                    .update(nal)?;
                clear += length_size + length;
            }
            _ => clear += length_size + length,
        }
        at = end;
    }
    if clear > 0 {
        push_subsample(&mut map, clear, 0)?;
    }
    if map.len() > MAX_SUBSAMPLES {
        return Err(invalid(
            "a sample has more than 42 subsamples, more than saiz can describe",
        ));
    }
    Ok(map)
}

/// Adds one subsample; a clear run longer than `u16::MAX` becomes several clear-only ones.
fn push_subsample(map: &mut Vec<(u16, u32)>, mut clear: usize, protected: usize) -> Result<()> {
    while clear > usize::from(u16::MAX) {
        map.push((u16::MAX, 0));
        clear -= usize::from(u16::MAX);
    }
    let clear = u16::try_from(clear).expect("reduced below u16::MAX above");
    let protected = u32::try_from(protected)
        .map_err(|_| Error::InvalidMedia("a slice is larger than 4 GiB".to_owned()))?;
    map.push((clear, protected));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avc_fixture() -> (AvcParameters, Vec<u8>) {
        // One IDR access unit from the fixture, with its parameters: see avc.rs tests.
        crate::cenc::avc::tests_support::first_idr("h264-aac.mp4")
    }

    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                let mut unit = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
                unit.extend_from_slice(nal);
                unit
            })
            .collect()
    }

    #[test]
    fn non_slice_nal_units_join_the_next_clear_run() {
        let (parameters, idr) = avc_fixture();
        let sei = vec![0x06; 20];
        let sample = length_prefixed(&[&sei, &idr]);

        let map = avc_subsamples(&parameters, &sample).unwrap();

        let header = parameters.clear_bytes(&idr).unwrap();
        assert_eq!(
            map,
            vec![(
                u16::try_from(4 + 20 + 4 + header).unwrap(),
                u32::try_from(idr.len() - header).unwrap()
            )]
        );
    }

    #[test]
    fn a_long_clear_run_is_split_into_u16_subsamples() {
        let (parameters, idr) = avc_fixture();
        let sei = vec![0x06; 70_000];
        let sample = length_prefixed(&[&sei, &idr]);

        let map = avc_subsamples(&parameters, &sample).unwrap();

        assert_eq!(map[0], (65_535, 0));
        let clear: usize = map.iter().map(|(clear, _)| usize::from(*clear)).sum();
        let protected: usize = map.iter().map(|(_, protected)| *protected as usize).sum();
        assert_eq!(clear + protected, sample.len());
    }

    #[test]
    fn too_many_slices_fail_instead_of_truncating_saiz() {
        let (parameters, idr) = avc_fixture();
        let slices = vec![idr.as_slice(); 43];
        let sample = length_prefixed(&slices);

        let error = avc_subsamples(&parameters, &sample).unwrap_err();

        assert!(error.to_string().contains("42"), "{error}");
    }

    #[test]
    fn an_in_band_parameter_set_is_honoured_within_its_sample() {
        let (mut parameters, idr) = avc_fixture();
        let (sps, pps) = parameters.take_all_for_test();
        let sample = length_prefixed(&[&sps, &pps, &idr]);

        let map = avc_subsamples(&parameters, &sample).unwrap();

        assert_eq!(
            map.len(),
            1,
            "the in-band SPS and PPS are clear bytes of the one slice's subsample"
        );
    }

    #[test]
    fn a_truncated_nal_length_fails() {
        let (parameters, _) = avc_fixture();

        assert!(avc_subsamples(&parameters, &[0, 0, 0, 9, 0x65]).is_err());
    }
}
