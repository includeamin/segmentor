//! Codec configuration read from sample entries.
//!
//! Only what packaging needs is read: the codec string for the manifest, dimensions or channel
//! layout, and (for AAC) the audio object type. The sample entry itself is copied verbatim into
//! the init segment, so nothing here has to reproduce it, and a new codec needs its
//! configuration box read and no writer.

use std::fmt::Write;

use super::boxes::{RawBox, Reader, invalid_media, optional_child};
use crate::error::{Error, Result};
use crate::media::CodecConfig;

/// The fixed-size head of a visual sample entry, before its child boxes.
const VISUAL_ENTRY_HEAD: usize = 78;

/// The width, height, and child boxes of a visual sample entry payload.
fn visual_entry(entry: &[u8]) -> Result<(u16, u16, &[u8])> {
    let mut reader = Reader::new(entry);
    // Reserved (6), data reference index (2), and 16 bytes of pre-defined fields.
    reader.skip(24)?;
    let width = reader.u16()?;
    let height = reader.u16()?;
    // Resolutions, frame count, compressor name, depth, pre-defined.
    reader.skip(VISUAL_ENTRY_HEAD - 28)?;
    Ok((width, height, reader.rest()))
}

/// Reads an `avc1` sample entry payload.
pub(crate) fn parse_avc(entry: &[u8]) -> Result<CodecConfig> {
    let (width, height, children) = visual_entry(entry)?;
    let config = optional_child(children, *b"avcC")?
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

/// Reads an `hvc1` or `hev1` sample entry payload. The codec string keeps the entry's own tag,
/// because it says where the parameter sets live.
pub(crate) fn parse_hevc(entry: &[u8], tag: [u8; 4]) -> Result<CodecConfig> {
    let (width, height, children) = visual_entry(entry)?;
    let config = optional_child(children, *b"hvcC")?
        .ok_or_else(|| Error::Unsupported("HEVC without hvcC".to_owned()))?;
    let mut config = Reader::new(config.payload);
    config.skip(1)?;
    let profile = config.u8()?;
    let compatibility = config.u32()?;
    let mut constraints = [0u8; 6];
    constraints.copy_from_slice(config.take(6)?);
    let level = config.u8()?;

    // ISO/IEC 14496-15 Annex E: profile space and idc, the compatibility flags with their bit
    // order reversed, tier and level, then the constraint bytes without trailing zeros.
    let space = match profile >> 6 {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let tier = if profile & 0x20 == 0 { 'L' } else { 'H' };
    let mut codecs = format!(
        "{}.{space}{}.{:X}.{tier}{level}",
        String::from_utf8_lossy(&tag),
        profile & 0x1f,
        compatibility.reverse_bits()
    );
    let used = constraints
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    for byte in &constraints[..used] {
        write!(codecs, ".{byte:X}").expect("writing to a String cannot fail");
    }
    Ok(CodecConfig::Hevc {
        width,
        height,
        codecs,
    })
}

/// Reads a `vp09` sample entry payload.
pub(crate) fn parse_vp9(entry: &[u8]) -> Result<CodecConfig> {
    let (width, height, children) = visual_entry(entry)?;
    let config = optional_child(children, *b"vpcC")?
        .ok_or_else(|| Error::Unsupported("VP9 without vpcC".to_owned()))?;
    let mut config = Reader::new(config.payload);
    config.full_box()?;
    let profile = config.u8()?;
    let level = config.u8()?;
    let bit_depth = config.u8()? >> 4;
    Ok(CodecConfig::Vp9 {
        width,
        height,
        codecs: format!("vp09.{profile:02}.{level:02}.{bit_depth:02}"),
    })
}

/// Reads an `av01` sample entry payload.
pub(crate) fn parse_av1(entry: &[u8]) -> Result<CodecConfig> {
    let (width, height, children) = visual_entry(entry)?;
    let config = optional_child(children, *b"av1C")?
        .ok_or_else(|| Error::Unsupported("AV1 without av1C".to_owned()))?;
    let mut config = Reader::new(config.payload);
    // The marker bit and version, which must be 1 and 0.
    if config.u8()? != 0x81 {
        return Err(Error::Unsupported(
            "AV1 configuration version is not 1".to_owned(),
        ));
    }
    let profile_and_level = config.u8()?;
    let flags = config.u8()?;
    let tier = if flags & 0x80 == 0 { 'M' } else { 'H' };
    let bit_depth = match (flags & 0x40 != 0, flags & 0x20 != 0) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    Ok(CodecConfig::Av1 {
        width,
        height,
        codecs: format!(
            "av01.{}.{:02}{tier}.{bit_depth:02}",
            profile_and_level >> 5,
            profile_and_level & 0x1f
        ),
    })
}

/// The fields of an audio sample entry payload that every audio codec shares.
struct AudioEntry<'a> {
    channels: u16,
    /// The integer part of the entry's 16.16 sample rate.
    sample_rate: u32,
    children: &'a [u8],
}

fn audio_entry(entry: &[u8], track_id: u32) -> Result<AudioEntry<'_>> {
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
    Ok(AudioEntry {
        channels,
        sample_rate,
        children: reader.rest(),
    })
}

/// Reads an `mp4a` sample entry payload for an AAC track: LC, HE-AAC, or HE-AACv2.
pub(crate) fn parse_aac(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let audio = audio_entry(entry, track_id)?;
    let esds = find_esds(audio.children, track_id)?;
    let object_type = audio_object_type(esds.payload, track_id)?;
    // 2 is LC; 5 and 29 signal SBR and SBR with parametric stereo explicitly, which players
    // need in the codec string to set up the decoder for the doubled output rate.
    let object_type = u8::try_from(object_type)
        .ok()
        .filter(|object_type| matches!(object_type, 2 | 5 | 29))
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "track {track_id}: AAC audio object type {object_type} is not supported, only \
                 AAC-LC (2), HE-AAC (5), and HE-AACv2 (29) are"
            ))
        })?;
    Ok(CodecConfig::Aac {
        sample_rate: audio.sample_rate,
        channels: audio.channels,
        object_type,
    })
}

/// Reads an `ac-3` sample entry payload.
pub(crate) fn parse_ac3(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let audio = audio_entry(entry, track_id)?;
    require_child(audio.children, *b"dac3", "AC-3", track_id)?;
    Ok(CodecConfig::Ac3 {
        sample_rate: audio.sample_rate,
        channels: audio.channels,
    })
}

/// Reads an `ec-3` sample entry payload.
pub(crate) fn parse_eac3(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let audio = audio_entry(entry, track_id)?;
    require_child(audio.children, *b"dec3", "E-AC-3", track_id)?;
    Ok(CodecConfig::Eac3 {
        sample_rate: audio.sample_rate,
        channels: audio.channels,
    })
}

/// Reads an `Opus` sample entry payload. The channel count is in `dOps`; the entry's own field
/// is not authoritative for Opus.
pub(crate) fn parse_opus(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let audio = audio_entry(entry, track_id)?;
    let config = require_child(audio.children, *b"dOps", "Opus", track_id)?;
    let mut config = Reader::new(config.payload);
    // Version, then the output channel count.
    config.skip(1)?;
    Ok(CodecConfig::Opus {
        channels: u16::from(config.u8()?),
    })
}

/// Reads a `fLaC` sample entry payload. Rates above 65535 Hz do not fit the entry's field, so
/// the rate and channel count come from the `STREAMINFO` block in `dfLa`.
pub(crate) fn parse_flac(entry: &[u8], track_id: u32) -> Result<CodecConfig> {
    let audio = audio_entry(entry, track_id)?;
    let config = require_child(audio.children, *b"dfLa", "FLAC", track_id)?;
    let mut config = Reader::new(config.payload);
    config.full_box()?;
    // The first metadata block must be STREAMINFO (type 0): a header of flags, type, and a
    // 24-bit length, then minimum and maximum block and frame sizes.
    let header = config.u8()?;
    if header & 0x7f != 0 {
        return Err(invalid_media("dfLa does not start with STREAMINFO"));
    }
    config.skip(3 + 10)?;
    let packed = config.take(3)?;
    let sample_rate =
        (u32::from(packed[0]) << 12) | (u32::from(packed[1]) << 4) | u32::from(packed[2] >> 4);
    let channels = u16::from((packed[2] >> 1) & 0x07) + 1;
    if sample_rate == 0 {
        return Err(invalid_media("FLAC sample rate is zero"));
    }
    Ok(CodecConfig::Flac {
        sample_rate,
        channels,
    })
}

fn require_child<'a>(
    children: &'a [u8],
    name: [u8; 4],
    codec: &str,
    track_id: u32,
) -> Result<RawBox<'a>> {
    optional_child(children, name)?.ok_or_else(|| {
        Error::Unsupported(format!(
            "track {track_id}: {codec} without its `{}` configuration box",
            String::from_utf8_lossy(&name)
        ))
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

    fn boxed(name: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(payload);
        bytes
    }

    /// A visual sample entry payload: the 78-byte head with the dimensions, then `children`.
    fn visual(width: u16, height: u16, children: &[u8]) -> Vec<u8> {
        let mut entry = vec![0; 24];
        entry.extend_from_slice(&width.to_be_bytes());
        entry.extend_from_slice(&height.to_be_bytes());
        entry.resize(VISUAL_ENTRY_HEAD, 0);
        entry.extend_from_slice(children);
        entry
    }

    /// A version 0 audio sample entry payload with `channels` and an integer sample rate.
    fn audio(channels: u16, sample_rate: u16, children: &[u8]) -> Vec<u8> {
        let mut entry = vec![0; 16];
        entry.extend_from_slice(&channels.to_be_bytes());
        entry.extend_from_slice(&[0; 6]);
        entry.extend_from_slice(&(u32::from(sample_rate) << 16).to_be_bytes());
        entry.extend_from_slice(children);
        entry
    }

    fn hvcc(profile: u8, compatibility: u32, constraints: [u8; 6], level: u8) -> Vec<u8> {
        let mut payload = vec![1, profile];
        payload.extend_from_slice(&compatibility.to_be_bytes());
        payload.extend_from_slice(&constraints);
        payload.push(level);
        payload.extend_from_slice(&[0; 10]);
        boxed(*b"hvcC", &payload)
    }

    fn codecs_of(config: Result<CodecConfig>) -> String {
        config.unwrap().codecs()
    }

    #[test]
    fn hevc_codec_strings_follow_annex_e() {
        let entry = |profile, compatibility, constraints, level| {
            visual(
                1920,
                1080,
                &hvcc(profile, compatibility, constraints, level),
            )
        };
        // Main, level 3.1: the string players expect for typical 1080p HEVC.
        assert_eq!(
            codecs_of(parse_hevc(
                &entry(0x01, 0x6000_0000, [0xB0, 0, 0, 0, 0, 0], 93),
                *b"hvc1"
            )),
            "hvc1.1.6.L93.B0"
        );
        // Main 10, level 4.0: profile 2, and the compatibility flags reverse to 4.
        assert_eq!(
            codecs_of(parse_hevc(
                &entry(0x02, 0x2000_0000, [0xB0, 0, 0, 0, 0, 0], 120),
                *b"hvc1"
            )),
            "hvc1.2.4.L120.B0"
        );
        // High tier, and the entry's own tag is kept for in-band parameter sets.
        assert_eq!(
            codecs_of(parse_hevc(
                &entry(0x21, 0x6000_0000, [0x90, 0, 0, 0, 0, 0], 153),
                *b"hev1"
            )),
            "hev1.1.6.H153.90"
        );
        // A profile space letter, and constraint bytes are kept up to the last non-zero one.
        assert_eq!(
            codecs_of(parse_hevc(
                &entry(0x41, 0x6000_0000, [0x90, 0, 0x01, 0, 0, 0], 93),
                *b"hvc1"
            )),
            "hvc1.A1.6.L93.90.0.1"
        );
        // No constraint flags at all leaves no trailing fields.
        assert_eq!(
            codecs_of(parse_hevc(&entry(0x01, 0x6000_0000, [0; 6], 93), *b"hvc1")),
            "hvc1.1.6.L93"
        );
    }

    #[test]
    fn hevc_without_its_configuration_is_unsupported() {
        let error = parse_hevc(&visual(64, 64, &[]), *b"hvc1").unwrap_err();

        assert!(error.to_string().contains("hvcC"), "{error}");
    }

    #[test]
    fn vp9_codec_strings_carry_profile_level_and_depth() {
        let config = |profile, level, depth_byte| {
            visual(
                64,
                64,
                &boxed(
                    *b"vpcC",
                    &[1, 0, 0, 0, profile, level, depth_byte, 2, 2, 2, 0, 0],
                ),
            )
        };

        assert_eq!(
            codecs_of(parse_vp9(&config(2, 31, 10 << 4))),
            "vp09.02.31.10"
        );
        assert_eq!(
            codecs_of(parse_vp9(&config(0, 11, 8 << 4 | 0x02))),
            "vp09.00.11.08"
        );
        assert!(parse_vp9(&visual(64, 64, &[])).is_err());
    }

    #[test]
    fn av1_codec_strings_carry_profile_level_tier_and_depth() {
        let config = |profile_and_level: u8, flags: u8| {
            visual(
                64,
                64,
                &boxed(*b"av1C", &[0x81, profile_and_level, flags, 0]),
            )
        };

        assert_eq!(codecs_of(parse_av1(&config(8, 0x00))), "av01.0.08M.08");
        assert_eq!(
            codecs_of(parse_av1(&config(8, 0x80 | 0x40))),
            "av01.0.08H.10"
        );
        assert_eq!(
            codecs_of(parse_av1(&config(0b0010_1101, 0x40 | 0x20))),
            "av01.1.13M.12"
        );
        let wrong_version = visual(64, 64, &boxed(*b"av1C", &[0x82, 8, 0, 0]));
        assert!(parse_av1(&wrong_version).is_err());
    }

    fn audio_with_esds(config: &[u8]) -> Vec<u8> {
        audio(
            2,
            48_000,
            &boxed(*b"esds", &esds(MPEG4_AUDIO, config, 0, &[])),
        )
    }

    #[test]
    fn aac_object_types_map_to_their_codec_strings() {
        // Object type in the top five bits of the first byte: 2 (LC), 5 (SBR), 29 (SBR + PS).
        for (first_byte, expected) in [
            (0x11, "mp4a.40.2"),
            (0x29, "mp4a.40.5"),
            (0xE9, "mp4a.40.29"),
        ] {
            let config = parse_aac(&audio_with_esds(&[first_byte, 0x90]), 1).unwrap();

            assert_eq!(config.codecs(), expected);
        }
    }

    #[test]
    fn other_aac_object_types_are_refused_by_number() {
        // 23 is AAC-LD, which no browser decodes through these protocols.
        let error = parse_aac(&audio_with_esds(&[23 << 3, 0x90]), 3).unwrap_err();

        assert!(error.to_string().contains("track 3"), "{error}");
        assert!(error.to_string().contains("object type 23"), "{error}");
    }

    #[test]
    fn dolby_entries_need_their_configuration_boxes() {
        let ac3 = audio(6, 48_000, &boxed(*b"dac3", &[0x50, 0x11, 0xC0]));
        let eac3 = audio(6, 48_000, &boxed(*b"dec3", &[0x06, 0x00, 0x60, 0x00]));

        assert_eq!(codecs_of(parse_ac3(&ac3, 1)), "ac-3");
        assert_eq!(codecs_of(parse_eac3(&eac3, 1)), "ec-3");
        assert_eq!(
            parse_ac3(&ac3, 1).unwrap().audio_format(),
            Some((48_000, 6))
        );
        let error = parse_ac3(&audio(2, 48_000, &[]), 5).unwrap_err();
        assert!(error.to_string().contains("dac3"), "{error}");
        assert!(error.to_string().contains("track 5"), "{error}");
        assert!(parse_eac3(&audio(2, 48_000, &[]), 5).is_err());
    }

    #[test]
    fn opus_takes_its_channels_from_dops_and_always_decodes_at_48_khz() {
        // Version 0, six channels, then pre-skip, input rate, gain, and channel mapping.
        let entry = audio(
            2,
            48_000,
            &boxed(*b"dOps", &[0, 6, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 1]),
        );

        let config = parse_opus(&entry, 1).unwrap();

        assert_eq!(config.codecs(), "opus");
        assert_eq!(config.audio_format(), Some((48_000, 6)));
        assert!(parse_opus(&audio(2, 48_000, &[]), 1).is_err());
    }

    #[test]
    fn flac_reads_a_rate_that_does_not_fit_the_entry_field() {
        // STREAMINFO for 96 kHz stereo: the rate is 20 bits (0x17700), channels-1 is 1.
        // Header and length, then block sizes (2 + 2 bytes) and frame sizes (3 + 3 bytes).
        let mut streaminfo = vec![0x80, 0, 0, 34, 0, 0x10, 0, 0x10, 0, 0, 0, 0, 0, 0];
        streaminfo.extend_from_slice(&[0x17, 0x70, 0x02]);
        streaminfo.extend_from_slice(&[0; 21]);
        let mut dfla = vec![0, 0, 0, 0];
        dfla.extend_from_slice(&streaminfo);
        // The entry's own 16-bit field cannot hold 96000, so it says something unrelated.
        let entry = audio(2, 0, &boxed(*b"dfLa", &dfla));

        let config = parse_flac(&entry, 1).unwrap();

        assert_eq!(config.codecs(), "fLaC");
        assert_eq!(config.audio_format(), Some((96_000, 2)));
    }

    #[test]
    fn flac_must_start_with_streaminfo() {
        let mut dfla = vec![0, 0, 0, 0, 0x81];
        dfla.extend_from_slice(&[0; 40]);

        assert!(parse_flac(&audio(2, 44_100, &boxed(*b"dfLa", &dfla)), 1).is_err());
    }
}
