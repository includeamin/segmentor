//! Codec configuration read from sample entries: H.264's `avcC` and AAC's `esds`.
//!
//! Only what packaging needs is read: the parameter sets and profile bytes for the manifest's
//! codec string, and the audio object type to refuse anything but AAC-LC. The sample entry
//! itself is copied verbatim into the init segment, so nothing here has to reproduce it.

use super::boxes::{RawBox, Reader, invalid_media, optional_child};
use crate::error::{Error, Result};
use crate::media::CodecConfig;

/// The fixed-size head of a visual sample entry, before its child boxes.
const VISUAL_ENTRY_HEAD: usize = 78;

/// Reads an `avc1` sample entry payload.
pub(crate) fn parse_avc(entry: &[u8]) -> Result<CodecConfig> {
    let mut reader = Reader::new(entry);
    // Reserved (6), data reference index (2), and 16 bytes of pre-defined fields.
    reader.skip(24)?;
    let width = reader.u16()?;
    let height = reader.u16()?;
    // Resolutions, frame count, compressor name, depth, pre-defined.
    reader.skip(VISUAL_ENTRY_HEAD - 28)?;
    let config = optional_child(reader.rest(), *b"avcC")?
        .ok_or_else(|| Error::Unsupported("H.264 without avcC".to_owned()))?;

    let mut config = Reader::new(config.payload);
    config.skip(1)?;
    let profile = config.u8()?;
    let compatibility = config.u8()?;
    let level = config.u8()?;
    config.skip(1)?;
    let sequence_parameter_set = first_parameter_set(&mut config, 0x1f, "SPS")?;
    let picture_parameter_set = first_parameter_set(&mut config, 0xff, "PPS")?;
    Ok(CodecConfig::Avc {
        width,
        height,
        profile,
        compatibility,
        level,
        sequence_parameter_set,
        picture_parameter_set,
    })
}

/// Reads a count and that many length-prefixed parameter sets, keeping the first.
fn first_parameter_set(reader: &mut Reader<'_>, count_mask: u8, name: &str) -> Result<Vec<u8>> {
    let count = reader.u8()? & count_mask;
    let mut first = None;
    for _ in 0..count {
        let length = usize::from(reader.u16()?);
        let bytes = reader.take(length)?;
        first.get_or_insert_with(|| bytes.to_vec());
    }
    first.ok_or_else(|| Error::Unsupported(format!("H.264 without {name}")))
}

/// Reads an `mp4a` sample entry payload for an AAC-LC track.
pub(crate) fn parse_aac(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let mut reader = Reader::new(entry);
    // Reserved (6) and data reference index (2).
    reader.skip(8)?;
    let version = reader.u16()?;
    // Revision level and vendor.
    reader.skip(6)?;
    let channels = reader.u16()?;
    // Sample size, pre-defined, reserved.
    reader.skip(6)?;
    let sample_rate = reader.u32()? >> 16;
    match version {
        0 => {}
        // QuickTime's version 1 adds four 32-bit fields and moves `esds` inside a `wave` box.
        1 => reader.skip(16)?,
        other => {
            return Err(Error::Unsupported(format!(
                "track {track_id}: sound sample description version {other} is not supported"
            )));
        }
    }
    let esds = find_esds(reader.rest(), track_id)?;
    let object_type = audio_object_type(esds.payload, track_id)?;
    if object_type != AAC_LC {
        return Err(Error::Unsupported(format!(
            "track {track_id}: AAC audio object type {object_type} is not supported, only AAC-LC ({AAC_LC}) is"
        )));
    }
    Ok(CodecConfig::Aac {
        sample_rate,
        channels,
    })
}

/// The `esds` box, directly under the sample entry or, in `QuickTime` files, inside `wave`.
fn find_esds(children: &[u8], track_id: u32) -> Result<RawBox<'_>> {
    let missing = || Error::Unsupported(format!("track {track_id}: AAC without esds"));
    if let Some(esds) = optional_child(children, *b"esds")? {
        return Ok(esds);
    }
    let wave = optional_child(children, *b"wave")?.ok_or_else(missing)?;
    optional_child(wave.payload, *b"esds")?.ok_or_else(missing)
}

/// Rewrites a `QuickTime`-style `mp4a` entry payload into the ISO layout, or returns `None` when
/// it already is one.
///
/// `QuickTime`'s sound description version 1 adds four fields and wraps `esds` in a `wave` box,
/// next to `QuickTime`-only boxes such as `chan`. Players that read MP4 through Media Source
/// Extensions reject that entry, so the ISO form is written instead: the same channel count and
/// sample rate, and the `esds` box alone.
pub(crate) fn iso_audio_entry(entry: &[u8], track_id: u32) -> Result<Option<Vec<u8>>> {
    let mut reader = Reader::new(entry);
    // Reserved (6) and data reference index (2), kept as they are.
    let head = reader.take(8)?;
    if reader.u16()? == 0 {
        return Ok(None);
    }
    // Revision level and vendor.
    reader.skip(6)?;
    let channels = reader.u16()?;
    // Sample size, compression ID, packet size.
    reader.skip(6)?;
    let sample_rate = reader.u32()?;
    // Version 1's four extra fields; `parse_aac` has already refused versions above 1.
    reader.skip(16)?;
    let esds = find_esds(reader.rest(), track_id)?;

    let mut iso = head.to_vec();
    iso.extend_from_slice(&[0; 2 + 2 + 4]);
    iso.extend_from_slice(&channels.to_be_bytes());
    // Sixteen bits per sample, no compression, no packet size.
    iso.extend_from_slice(&16u16.to_be_bytes());
    iso.extend_from_slice(&[0; 4]);
    iso.extend_from_slice(&sample_rate.to_be_bytes());
    let size = u32::try_from(esds.payload.len() + 8)
        .map_err(|_| invalid_media("esds box is too large"))?;
    iso.extend_from_slice(&size.to_be_bytes());
    iso.extend_from_slice(b"esds");
    iso.extend_from_slice(esds.payload);
    Ok(Some(iso))
}

const AAC_LC: u32 = 2;

const ES_DESCRIPTOR: u8 = 0x03;
const DECODER_CONFIG_DESCRIPTOR: u8 = 0x04;
const DECODER_SPECIFIC_INFO: u8 = 0x05;
/// `objectTypeIndication` for MPEG-4 audio.
const MPEG4_AUDIO: u8 = 0x40;

/// The audio object type from the `AudioSpecificConfig` inside an `esds` payload.
fn audio_object_type(esds: &[u8], track_id: u32) -> Result<u32> {
    let mut reader = Reader::new(esds);
    reader.full_box()?;
    let es = descriptor(&mut reader, ES_DESCRIPTOR)?
        .ok_or_else(|| invalid_media("esds has no ES descriptor"))?;

    let mut es = Reader::new(es);
    // ES_ID, then optional dependency, URL, and OCR fields named by the flags.
    es.skip(2)?;
    let flags = es.u8()?;
    if flags & 0x80 != 0 {
        es.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let url_length = usize::from(es.u8()?);
        es.skip(url_length)?;
    }
    if flags & 0x20 != 0 {
        es.skip(2)?;
    }
    let decoder = descriptor(&mut es, DECODER_CONFIG_DESCRIPTOR)?
        .ok_or_else(|| invalid_media("esds has no decoder configuration"))?;

    let mut decoder = Reader::new(decoder);
    let indication = decoder.u8()?;
    if indication != MPEG4_AUDIO {
        return Err(Error::Unsupported(format!(
            "track {track_id}: audio object type indication {indication:#04x} is not MPEG-4 AAC"
        )));
    }
    // Stream type, buffer size, maximum and average bitrate.
    decoder.skip(12)?;
    let config = descriptor(&mut decoder, DECODER_SPECIFIC_INFO)?
        .ok_or_else(|| invalid_media("esds has no AudioSpecificConfig"))?;
    object_type_of(config)
}

/// Reads descriptors until one with `wanted` is found, returning its body.
fn descriptor<'a>(reader: &mut Reader<'a>, wanted: u8) -> Result<Option<&'a [u8]>> {
    while reader.remaining() > 0 {
        let tag = reader.u8()?;
        // The length is 1 to 4 bytes of 7 bits each, high bit set on all but the last.
        let mut length = 0usize;
        for _ in 0..4 {
            let byte = reader.u8()?;
            length = (length << 7) | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        let body = reader.take(length)?;
        if tag == wanted {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

/// The leading five bits of an `AudioSpecificConfig`, with the escape to a six-bit extension.
fn object_type_of(config: &[u8]) -> Result<u32> {
    let first = *config
        .first()
        .ok_or_else(|| invalid_media("AudioSpecificConfig is empty"))?;
    let object_type = u32::from(first >> 3);
    if object_type != 31 {
        return Ok(object_type);
    }
    let second = *config
        .get(1)
        .ok_or_else(|| invalid_media("AudioSpecificConfig is truncated"))?;
    Ok(32 + ((u32::from(first & 0x07) << 3) | u32::from(second >> 5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `esds` payload for an AAC stream with the given `AudioSpecificConfig` bytes.
    fn esds(indication: u8, config: &[u8], es_flags: u8, extra: &[u8]) -> Vec<u8> {
        let length = |bytes: &[u8]| u8::try_from(bytes.len()).unwrap();
        let mut specific = vec![DECODER_SPECIFIC_INFO, length(config)];
        specific.extend_from_slice(config);
        let mut decoder = vec![indication, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        decoder.extend_from_slice(&specific);
        let mut es = vec![0, 1, es_flags];
        es.extend_from_slice(extra);
        es.extend_from_slice(&[DECODER_CONFIG_DESCRIPTOR, length(&decoder)]);
        es.extend_from_slice(&decoder);
        let mut payload = vec![0, 0, 0, 0, ES_DESCRIPTOR, length(&es)];
        payload.extend_from_slice(&es);
        payload
    }

    #[test]
    fn reads_aac_lc_from_an_audio_specific_config() {
        // Object type 2 (LC), 48 kHz index 3, two channels.
        let payload = esds(MPEG4_AUDIO, &[0x11, 0x90], 0, &[]);

        assert_eq!(audio_object_type(&payload, 1).unwrap(), 2);
    }

    #[test]
    fn reads_the_escaped_object_type() {
        // 31 escapes to 32 + the next six bits: here 32 + 0b000001 = 33.
        assert_eq!(object_type_of(&[0b1111_1000, 0b0010_0000]).unwrap(), 33);
        assert!(object_type_of(&[0b1111_1000]).is_err());
        assert!(object_type_of(&[]).is_err());
    }

    #[test]
    fn skips_the_optional_es_descriptor_fields() {
        // Dependency (2 bytes), a 3-byte URL, and OCR (2 bytes).
        let payload = esds(
            MPEG4_AUDIO,
            &[0x11, 0x90],
            0xE0,
            &[0, 0, 3, b'a', b'b', b'c', 0, 0],
        );

        assert_eq!(audio_object_type(&payload, 1).unwrap(), 2);
    }

    #[test]
    fn a_non_aac_stream_is_named_by_its_indication() {
        // 0x6B is MPEG-1 audio (MP3).
        let payload = esds(0x6B, &[0x11, 0x90], 0, &[]);

        let error = audio_object_type(&payload, 4).unwrap_err();

        assert!(error.to_string().contains("track 4"), "{error}");
        assert!(error.to_string().contains("0x6b"), "{error}");
    }

    #[test]
    fn reads_multi_byte_descriptor_lengths() {
        let mut data = vec![0x03, 0x80, 0x80, 0x80, 0x02, 0xAA, 0xBB];
        let mut reader = Reader::new(&data);

        assert_eq!(
            descriptor(&mut reader, 0x03).unwrap().unwrap(),
            [0xAA, 0xBB]
        );

        data.truncate(5);
        assert!(
            descriptor(&mut Reader::new(&data), 0x03).is_err(),
            "body is truncated"
        );
    }

    #[test]
    fn a_missing_descriptor_is_reported_not_guessed() {
        let payload = [0, 0, 0, 0];

        assert!(audio_object_type(&payload, 1).is_err());
    }
}
