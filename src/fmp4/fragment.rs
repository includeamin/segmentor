use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{Sample, Track, TrackKind};
use crate::segment::TrackSegment;
use crate::source::{ByteRange, LocalMediaSource, MediaSource};

const TRUN_FLAGS: u32 = 0x0000_0f01;
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;
const SYNC_SAMPLE_FLAGS: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE_FLAGS: u32 = 0x0101_0000;

#[derive(Debug, Clone)]
pub(crate) struct PreparedSegment {
    pub(crate) header: Bytes,
    pub(crate) ranges: Vec<ByteRange>,
    pub(crate) content_length: u64,
}

pub(crate) fn write_media_segment(
    source: &LocalMediaSource,
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
        let bytes = source.read_range(range)?;
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

    let provisional_moof = build_moof(track, samples, segment.decode_time, sequence_number, 0)?;
    let data_offset = i32::try_from(provisional_moof.len() + 8)
        .map_err(|_| Error::InvalidMedia("fragment header is too large".to_owned()))?;
    let moof = build_moof(
        track,
        samples,
        segment.decode_time,
        sequence_number,
        data_offset,
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
    track: &Track,
    samples: &[Sample],
    decode_time: u64,
    sequence_number: u32,
    data_offset: i32,
) -> Result<Vec<u8>> {
    let mut mfhd = Vec::new();
    write_full_box_fields(&mut mfhd, 0, 0);
    mfhd.extend_from_slice(&sequence_number.to_be_bytes());

    let mut tfhd = Vec::new();
    write_full_box_fields(&mut tfhd, 0, TFHD_DEFAULT_BASE_IS_MOOF);
    tfhd.extend_from_slice(&track.id.to_be_bytes());

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
        let flags = match track.kind {
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
