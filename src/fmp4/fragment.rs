use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{Sample, Track, TrackKind};
use crate::segment::TrackSegment;
use crate::source::{ByteRange, MediaSourceKind};

const TRUN_FLAGS: u32 = 0x0000_0f01;
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;
const SYNC_SAMPLE_FLAGS: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE_FLAGS: u32 = 0x0101_0000;

#[derive(Debug, Clone)]
pub(crate) struct PreparedSegment {
    pub(crate) header: Bytes,
    pub(crate) ranges: Vec<ByteRange>,
    pub(crate) content_length: u64,
    /// For an encrypted track: what remains once the samples are read (see `cenc::segment`).
    /// `header`, `ranges`, and `content_length` then describe the clear fragment and are replaced.
    #[allow(dead_code, reason = "TEMPORARY: used by the DRM plan's later tasks")]
    pub(crate) encryption: Option<Box<crate::cenc::PendingEncryption>>,
}

pub(crate) async fn write_media_segment(
    source: &MediaSourceKind,
    track: &Track,
    segment: TrackSegment,
    sequence_number: u32,
    limits: &LimitsConfig,
) -> Result<Vec<u8>> {
    let prepared = prepare_media_segment(track, segment, sequence_number, limits)?;
    let capacity = usize::try_from(prepared.content_length)
        .map_err(|_| Error::InvalidMedia("fragment size does not fit in memory".to_owned()))?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&prepared.header);
    for range in prepared.ranges {
        let bytes = source.read_range(range).await?;
        output.extend_from_slice(&bytes);
    }
    Ok(output)
}

pub(crate) fn prepare_media_segment(
    track: &Track,
    segment: TrackSegment,
    sequence_number: u32,
    limits: &LimitsConfig,
) -> Result<PreparedSegment> {
    let samples = track
        .samples
        .get(segment.first_sample..segment.end_sample)
        .ok_or_else(|| Error::InvalidMedia("segment sample range is invalid".to_owned()))?;
    if samples.is_empty() {
        return Err(Error::InvalidMedia(
            "segment contains no samples".to_owned(),
        ));
    }

    let provisional_moof = build_moof(
        track.id,
        track.kind,
        samples,
        segment.decode_time,
        sequence_number,
        0,
        &[],
    )?;
    let data_offset = i32::try_from(provisional_moof.len() + 8)
        .map_err(|_| Error::InvalidMedia("fragment header is too large".to_owned()))?;
    let moof = build_moof(
        track.id,
        track.kind,
        samples,
        segment.decode_time,
        sequence_number,
        data_offset,
        &[],
    )?;
    let payload_len = samples.iter().try_fold(0u64, |total, sample| {
        total
            .checked_add(u64::from(sample.size))
            .ok_or_else(|| Error::InvalidMedia("fragment payload size overflow".to_owned()))
    })?;
    if payload_len > limits.max_segment_bytes {
        return Err(Error::InvalidMedia(
            "segment payload exceeds configured limit".to_owned(),
        ));
    }
    let header_capacity = moof
        .len()
        .checked_add(8)
        .ok_or_else(|| Error::InvalidMedia("fragment header size overflow".to_owned()))?;
    let mut header = Vec::with_capacity(header_capacity);
    header.extend_from_slice(&moof);
    write_box_header(&mut header, payload_len + 8, *b"mdat")?;
    let content_length = u64::try_from(header.len())
        .ok()
        .and_then(|length| length.checked_add(payload_len))
        .ok_or_else(|| Error::InvalidMedia("fragment size overflow".to_owned()))?;

    Ok(PreparedSegment {
        header: Bytes::from(header),
        ranges: coalesced_ranges(samples)?,
        content_length,
        encryption: None,
    })
}

fn coalesced_ranges(samples: &[Sample]) -> Result<Vec<ByteRange>> {
    let mut ranges: Vec<ByteRange> = Vec::new();
    for sample in samples {
        let sample_range = ByteRange::new(sample.offset, u64::from(sample.size));
        if let Some(previous) = ranges.last_mut()
            && previous.end() == Some(sample.offset)
        {
            previous.length = previous
                .length
                .checked_add(sample_range.length)
                .ok_or_else(|| Error::InvalidMedia("sample range size overflow".to_owned()))?;
        } else {
            ranges.push(sample_range);
        }
    }
    Ok(ranges)
}

fn build_moof(
    track_id: u32,
    kind: TrackKind,
    samples: &[Sample],
    decode_time: u64,
    sequence_number: u32,
    data_offset: i32,
    extra: &[u8],
) -> Result<Vec<u8>> {
    let mut mfhd = Vec::new();
    write_full_box_fields(&mut mfhd, 0, 0);
    mfhd.extend_from_slice(&sequence_number.to_be_bytes());

    let mut tfhd = Vec::new();
    write_full_box_fields(&mut tfhd, 0, TFHD_DEFAULT_BASE_IS_MOOF);
    tfhd.extend_from_slice(&track_id.to_be_bytes());

    let mut tfdt = Vec::new();
    write_full_box_fields(&mut tfdt, 1, 0);
    tfdt.extend_from_slice(&decode_time.to_be_bytes());

    let mut trun = Vec::new();
    write_full_box_fields(&mut trun, 1, TRUN_FLAGS);
    let sample_count = u32::try_from(samples.len())
        .map_err(|_| Error::InvalidMedia("fragment has too many samples".to_owned()))?;
    trun.extend_from_slice(&sample_count.to_be_bytes());
    trun.extend_from_slice(&data_offset.to_be_bytes());
    for sample in samples {
        trun.extend_from_slice(&sample.duration.to_be_bytes());
        trun.extend_from_slice(&sample.size.to_be_bytes());
        let flags = match kind {
            TrackKind::Audio => SYNC_SAMPLE_FLAGS,
            TrackKind::Video if sample.is_sync => SYNC_SAMPLE_FLAGS,
            TrackKind::Video => NON_SYNC_SAMPLE_FLAGS,
        };
        trun.extend_from_slice(&flags.to_be_bytes());
        trun.extend_from_slice(&sample.composition_offset.to_be_bytes());
    }

    let mut traf_payload = Vec::new();
    write_box(&mut traf_payload, *b"tfhd", &tfhd)?;
    write_box(&mut traf_payload, *b"tfdt", &tfdt)?;
    write_box(&mut traf_payload, *b"trun", &trun)?;
    traf_payload.extend_from_slice(extra);

    let mut moof_payload = Vec::new();
    write_box(&mut moof_payload, *b"mfhd", &mfhd)?;
    write_box(&mut moof_payload, *b"traf", &traf_payload)?;
    let mut moof = Vec::new();
    write_box(&mut moof, *b"moof", &moof_payload)?;
    Ok(moof)
}

fn write_full_box_fields(output: &mut Vec<u8>, version: u8, flags: u32) {
    output.push(version);
    let flag_bytes = flags.to_be_bytes();
    output.extend_from_slice(&flag_bytes[1..]);
}

fn write_box(output: &mut Vec<u8>, name: [u8; 4], payload: &[u8]) -> Result<()> {
    let size = u64::try_from(payload.len())
        .ok()
        .and_then(|length| length.checked_add(8))
        .ok_or_else(|| Error::InvalidMedia("box size overflow".to_owned()))?;
    write_box_header(output, size, name)?;
    output.extend_from_slice(payload);
    Ok(())
}

fn write_box_header(output: &mut Vec<u8>, size: u64, name: [u8; 4]) -> Result<()> {
    let size = u32::try_from(size)
        .map_err(|_| Error::InvalidMedia("large-size boxes are not supported".to_owned()))?;
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&name);
    Ok(())
}

/// Most subsamples one sample can have: `saiz` records each sample's `2 + 6n` bytes in a `u8`.
pub(crate) const MAX_SUBSAMPLES: usize = 42;

/// The `moof` and `mdat` header of an encrypted fragment: the usual boxes plus `senc`, `saiz`,
/// and `saio`, which describe each sample's clear and protected bytes (ISO/IEC 23001-7, 7.2).
/// An empty map means a whole-sample (audio) encryption with no subsamples.
pub(crate) fn encrypted_fragment_header(
    track_id: u32,
    kind: TrackKind,
    samples: &[Sample],
    decode_time: u64,
    sequence_number: u32,
    subsamples: &[Vec<(u16, u32)>],
    payload_len: usize,
) -> Result<Vec<u8>> {
    let overflow = || Error::InvalidMedia("encrypted fragment is too large".to_owned());
    let probe_extra = encryption_boxes(subsamples, 0)?;
    let probe = build_moof(
        track_id,
        kind,
        samples,
        decode_time,
        sequence_number,
        0,
        &probe_extra,
    )?;
    // `senc` is the first extra box, at the end of the `moof`; its entries start 16 bytes in
    // (box header 8, version and flags 4, sample count 4). Box sizes do not depend on the values
    // filled in below, so the probe's layout is the final one.
    let senc_entries = probe.len() - probe_extra.len() + 16;
    let extra = encryption_boxes(
        subsamples,
        u32::try_from(senc_entries).map_err(|_| overflow())?,
    )?;
    let data_offset = i32::try_from(probe.len() + 8).map_err(|_| overflow())?;
    let mut header = build_moof(
        track_id,
        kind,
        samples,
        decode_time,
        sequence_number,
        data_offset,
        &extra,
    )?;
    let payload = u64::try_from(payload_len).map_err(|_| overflow())?;
    write_box_header(
        &mut header,
        payload.checked_add(8).ok_or_else(overflow)?,
        *b"mdat",
    )?;
    Ok(header)
}

fn encryption_boxes(subsamples: &[Vec<(u16, u32)>], aux_offset: u32) -> Result<Vec<u8>> {
    let invalid = |message: &str| Error::InvalidMedia(message.to_owned());
    let use_subsamples = subsamples.iter().any(|map| !map.is_empty());
    let count = u32::try_from(subsamples.len()).map_err(|_| invalid("too many samples"))?;
    let mut senc = Vec::new();
    write_full_box_fields(&mut senc, 0, if use_subsamples { 2 } else { 0 });
    senc.extend_from_slice(&count.to_be_bytes());
    let mut sizes = Vec::with_capacity(subsamples.len());
    for map in subsamples {
        if !use_subsamples {
            sizes.push(0u8);
            continue;
        }
        if map.len() > MAX_SUBSAMPLES {
            return Err(invalid(
                "a sample has more than 42 subsamples, more than saiz can describe",
            ));
        }
        let entries = u16::try_from(map.len()).map_err(|_| invalid("too many subsamples"))?;
        senc.extend_from_slice(&entries.to_be_bytes());
        for (clear, protected) in map {
            senc.extend_from_slice(&clear.to_be_bytes());
            senc.extend_from_slice(&protected.to_be_bytes());
        }
        sizes.push(u8::try_from(2 + 6 * map.len()).map_err(|_| invalid("too many subsamples"))?);
    }
    let mut aux_sizes = Vec::new();
    write_full_box_fields(&mut aux_sizes, 0, 0);
    // A non-zero default means "every sample has this size"; zero means the sizes are listed.
    let uniform = sizes
        .first()
        .copied()
        .filter(|first| *first != 0 && sizes.iter().all(|size| size == first));
    aux_sizes.push(uniform.unwrap_or(0));
    aux_sizes.extend_from_slice(&count.to_be_bytes());
    if uniform.is_none() {
        aux_sizes.extend_from_slice(&sizes);
    }
    let mut aux_offsets = Vec::new();
    write_full_box_fields(&mut aux_offsets, 0, 0);
    aux_offsets.extend_from_slice(&1u32.to_be_bytes());
    aux_offsets.extend_from_slice(&aux_offset.to_be_bytes());
    let mut boxes = Vec::new();
    write_box(&mut boxes, *b"senc", &senc)?;
    write_box(&mut boxes, *b"saiz", &aux_sizes)?;
    write_box(&mut boxes, *b"saio", &aux_offsets)?;
    Ok(boxes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Sample;

    fn sample(size: u32) -> Sample {
        Sample {
            offset: 0,
            size,
            decode_time: 0,
            duration: 512,
            composition_offset: 0,
            is_sync: true,
        }
    }

    fn position(data: &[u8], name: [u8; 4]) -> usize {
        data.windows(4).position(|window| window == name).unwrap() - 4
    }

    #[test]
    fn senc_saiz_and_saio_describe_the_subsamples() {
        let samples = [sample(100), sample(60)];
        let subsamples = vec![vec![(10, 90)], vec![(5, 20), (7, 28)]];

        let header =
            encrypted_fragment_header(1, TrackKind::Video, &samples, 0, 3, &subsamples, 160)
                .unwrap();

        let senc = position(&header, *b"senc");
        assert_eq!(
            &header[senc + 8..senc + 12],
            &[0, 0, 0, 2],
            "subsample flag"
        );
        assert_eq!(&header[senc + 12..senc + 16], &2u32.to_be_bytes());
        assert_eq!(&header[senc + 16..senc + 24], &[0, 1, 0, 10, 0, 0, 0, 90]);
        let saiz = position(&header, *b"saiz");
        assert_eq!(header[saiz + 12], 0, "sizes differ, so they are listed");
        assert_eq!(&header[saiz + 13..saiz + 19], &[0, 0, 0, 2, 8, 14]);
        let offsets = position(&header, *b"saio");
        let offset = u32::from_be_bytes(header[offsets + 16..offsets + 20].try_into().unwrap());
        assert_eq!(
            offset as usize,
            senc + 16,
            "saio points at the first senc entry"
        );
        let mdat = position(&header, *b"mdat");
        assert_eq!(mdat + 8, header.len());
        assert_eq!(
            u32::from_be_bytes(header[mdat..mdat + 4].try_into().unwrap()),
            168
        );
    }

    #[test]
    fn whole_sample_audio_lists_zero_sized_entries() {
        let samples = [sample(50), sample(50)];

        let header =
            encrypted_fragment_header(2, TrackKind::Audio, &samples, 0, 1, &[vec![], vec![]], 100)
                .unwrap();

        let senc = position(&header, *b"senc");
        assert_eq!(&header[senc + 8..senc + 16], &[0, 0, 0, 0, 0, 0, 0, 2]);
        let saiz = position(&header, *b"saiz");
        assert_eq!(&header[saiz + 12..saiz + 19], &[0, 0, 0, 0, 2, 0, 0]);
    }
}
