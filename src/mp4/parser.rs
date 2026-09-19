use std::collections::HashSet;

use ::mp4::{MediaType, Mp4Reader, Mp4Track, TrackType};
use sha2::{Digest, Sha256};

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, MediaIndex, Sample, Track, TrackKind};
use crate::source::{ByteRange, LocalMediaSource, MediaSource};

pub(crate) fn parse(source: &LocalMediaSource, limits: &LimitsConfig) -> Result<MediaIndex> {
    if source.len() > limits.max_source_bytes {
        return Err(Error::Unsupported("source exceeds configured size limit"));
    }
    let moov_range = find_moov(source, limits.max_metadata_bytes)?;
    let moov_bytes = source.read_range(moov_range)?;
    validate_raw_moov(&moov_bytes)?;
    let moov_sha256 = Sha256::digest(&moov_bytes).into();
    let file = source.parser_file()?;
    let reader = Mp4Reader::read_header(file, source.len())?;

    if reader.is_fragmented() {
        return Err(Error::Unsupported("fragmented MP4 input"));
    }
    if reader.moov.traks.len() > limits.max_tracks {
        return Err(Error::Unsupported("track count exceeds configured limit"));
    }
    if reader.moov.traks.iter().any(|track| {
        track
            .edts
            .as_ref()
            .and_then(|edts| edts.elst.as_ref())
            .is_some()
    }) {
        return Err(Error::Unsupported("edit lists are not supported"));
    }

    let mut tracks = reader
        .tracks()
        .values()
        .map(|track| parse_track(track, source.len(), limits))
        .collect::<Result<Vec<_>>>()?;
    tracks.sort_unstable_by_key(|track| track.id);
    source.verify_unchanged()?;
    let current_moov_sha256: [u8; 32] = Sha256::digest(source.read_range(moov_range)?).into();
    if current_moov_sha256 != moov_sha256 {
        return Err(invalid_media("moov changed while it was being parsed"));
    }
    let mut identity = source.identity().clone();
    identity.moov_sha256 = Some(moov_sha256);

    Ok(MediaIndex {
        source: identity,
        movie_timescale: reader.moov.mvhd.timescale,
        duration: reader.moov.mvhd.duration,
        tracks,
    })
}

fn parse_track(track: &Mp4Track, source_len: u64, limits: &LimitsConfig) -> Result<Track> {
    let kind = match track.track_type()? {
        TrackType::Audio => TrackKind::Audio,
        TrackType::Video => TrackKind::Video,
        TrackType::Subtitle => return Err(Error::Unsupported("subtitle track")),
    };
    let codec = parse_codec(track)?;
    let samples = parse_samples(track, source_len, limits)?;

    Ok(Track {
        id: track.track_id(),
        kind,
        timescale: track.timescale(),
        duration: track.trak.mdia.mdhd.duration,
        codec,
        samples,
    })
}

fn parse_codec(track: &Mp4Track) -> Result<CodecConfig> {
    let sample_table = &track.trak.mdia.minf.stbl;

    match track.media_type()? {
        MediaType::H264 => {
            let avc = sample_table
                .stsd
                .avc1
                .as_ref()
                .ok_or(Error::Unsupported("H.264 without avc1 sample entry"))?;
            let sequence_parameter_set = avc
                .avcc
                .sequence_parameter_sets
                .first()
                .ok_or(Error::Unsupported("H.264 without SPS"))?
                .bytes
                .clone();
            let picture_parameter_set = avc
                .avcc
                .picture_parameter_sets
                .first()
                .ok_or(Error::Unsupported("H.264 without PPS"))?
                .bytes
                .clone();

            Ok(CodecConfig::Avc {
                width: avc.width,
                height: avc.height,
                profile: avc.avcc.avc_profile_indication,
                compatibility: avc.avcc.profile_compatibility,
                level: avc.avcc.avc_level_indication,
                sequence_parameter_set,
                picture_parameter_set,
            })
        }
        MediaType::AAC => {
            let aac = sample_table
                .stsd
                .mp4a
                .as_ref()
                .ok_or(Error::Unsupported("AAC without mp4a sample entry"))?;

            if track.audio_profile()? != ::mp4::AudioObjectType::AacLowComplexity {
                return Err(Error::Unsupported("AAC profile other than AAC-LC"));
            }

            Ok(CodecConfig::Aac {
                sample_rate: u32::from(aac.samplerate.value()),
                channels: aac.channelcount,
            })
        }
        _ => Err(Error::Unsupported("codec other than H.264 or AAC")),
    }
}

fn parse_samples(track: &Mp4Track, source_len: u64, limits: &LimitsConfig) -> Result<Vec<Sample>> {
    let sample_table = &track.trak.mdia.minf.stbl;
    let sample_count = usize::try_from(sample_table.stsz.sample_count)
        .map_err(|_| invalid_media("sample count does not fit in memory"))?;
    if sample_count > limits.max_samples_per_track {
        return Err(invalid_media("sample count exceeds configured limit"));
    }
    let sizes = sample_sizes(track, sample_count)?;
    let byte_offsets = sample_offsets(track, &sizes, sample_count)?;
    let times = sample_times(track, sample_count)?;
    let composition_offsets = composition_offsets(track, sample_count)?;
    let sync_samples = sample_table
        .stss
        .as_ref()
        .map(|stss| stss.entries.iter().copied().collect::<HashSet<_>>());

    let mut samples = Vec::with_capacity(sample_count);
    for index in 0..sample_count {
        let sample_number = u32::try_from(index)
            .ok()
            .and_then(|number| number.checked_add(1))
            .ok_or_else(|| invalid_media("sample number overflow"))?;
        let offset = byte_offsets[index];
        let size = sizes[index];
        let end = offset
            .checked_add(u64::from(size))
            .ok_or_else(|| invalid_media("sample byte range overflow"))?;
        if end > source_len {
            return Err(invalid_media("sample byte range exceeds source length"));
        }

        samples.push(Sample {
            offset,
            size,
            decode_time: times[index].0,
            duration: times[index].1,
            composition_offset: composition_offsets[index],
            is_sync: sync_samples
                .as_ref()
                .is_none_or(|samples| samples.contains(&sample_number)),
        });
    }

    Ok(samples)
}

fn find_moov(source: &LocalMediaSource, max_metadata_bytes: u64) -> Result<ByteRange> {
    let mut offset = 0u64;
    while offset < source.len() {
        let header = source.read_range(ByteRange::new(offset, 8))?;
        let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
        let name: [u8; 4] = header[4..8].try_into().unwrap();
        let (size, header_size) = if size32 == 1 {
            let extended = source.read_range(ByteRange::new(offset + 8, 8))?;
            (u64::from_be_bytes(extended[..8].try_into().unwrap()), 16)
        } else if size32 == 0 {
            (source.len() - offset, 8)
        } else {
            (u64::from(size32), 8)
        };
        if size < header_size
            || offset
                .checked_add(size)
                .is_none_or(|end| end > source.len())
        {
            return Err(invalid_media("invalid top-level MP4 box size"));
        }
        if &name == b"moov" {
            if size > max_metadata_bytes {
                return Err(invalid_media("moov exceeds configured metadata limit"));
            }
            return Ok(ByteRange::new(offset, size));
        }
        offset += size;
    }
    Err(invalid_media("missing moov box"))
}

fn validate_raw_moov(moov: &[u8]) -> Result<()> {
    let root = box_payload(moov, 0)?;
    if root.name != *b"moov" {
        return Err(invalid_media("metadata range is not a moov box"));
    }
    for track in child_boxes(root.payload)?
        .into_iter()
        .filter(|child| child.name == *b"trak")
    {
        let media = required_child(track.payload, *b"mdia")?;
        let media_info = required_child(media.payload, *b"minf")?;
        validate_data_reference(media_info.payload)?;
        let sample_table = required_child(media_info.payload, *b"stbl")?;
        validate_sample_description(sample_table.payload)?;
    }
    Ok(())
}

fn validate_data_reference(media_info: &[u8]) -> Result<()> {
    let data_info = required_child(media_info, *b"dinf")?;
    let data_reference = required_child(data_info.payload, *b"dref")?;
    if data_reference.payload.len() < 8 {
        return Err(invalid_media("dref payload is truncated"));
    }
    let entry_count = read_u32(&data_reference.payload[4..8])?;
    if entry_count != 1 {
        return Err(Error::Unsupported(
            "exactly one self-contained data reference is required",
        ));
    }
    let entries = child_boxes(&data_reference.payload[8..])?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid_media("dref entry is missing"))?;
    if entry.name != *b"url " || entry.payload.len() < 4 {
        return Err(Error::Unsupported(
            "external data references are not supported",
        ));
    }
    let flags = read_u32(&[0, entry.payload[1], entry.payload[2], entry.payload[3]])?;
    if flags & 1 == 0 {
        return Err(Error::Unsupported(
            "external data references are not supported",
        ));
    }
    Ok(())
}

fn validate_sample_description(sample_table: &[u8]) -> Result<()> {
    let description = required_child(sample_table, *b"stsd")?;
    if description.payload.len() < 8 {
        return Err(invalid_media("stsd payload is truncated"));
    }
    let entry_count = read_u32(&description.payload[4..8])?;
    if entry_count != 1 {
        return Err(Error::Unsupported(
            "exactly one sample description per track is required",
        ));
    }
    let entries = child_boxes(&description.payload[8..])?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid_media("stsd entry is missing"))?;
    match &entry.name {
        b"avc1" | b"mp4a" => Ok(()),
        b"encv" | b"enca" => Err(Error::Unsupported("encrypted media is not supported")),
        _ => Err(Error::Unsupported("sample description is not supported")),
    }
}

#[derive(Clone, Copy)]
struct RawBox<'a> {
    name: [u8; 4],
    payload: &'a [u8],
    size: usize,
}

fn required_child(data: &[u8], name: [u8; 4]) -> Result<RawBox<'_>> {
    child_boxes(data)?
        .into_iter()
        .find(|child| child.name == name)
        .ok_or_else(|| invalid_media("required MP4 box is missing"))
}

fn child_boxes(mut data: &[u8]) -> Result<Vec<RawBox<'_>>> {
    let mut children = Vec::new();
    while !data.is_empty() {
        let child = box_payload(data, 0)?;
        children.push(child);
        data = &data[child.size..];
    }
    Ok(children)
}

fn box_payload(data: &[u8], offset: usize) -> Result<RawBox<'_>> {
    let header = data
        .get(offset..offset + 8)
        .ok_or_else(|| invalid_media("MP4 box header is truncated"))?;
    let size32 = read_u32(&header[..4])?;
    let name = header[4..8].try_into().unwrap();
    let (size, header_size) = if size32 == 1 {
        let extended = data
            .get(offset + 8..offset + 16)
            .ok_or_else(|| invalid_media("extended MP4 box header is truncated"))?;
        (
            usize::try_from(u64::from_be_bytes(extended.try_into().unwrap()))
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

fn read_u32(bytes: &[u8]) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
        invalid_media("expected four-byte integer")
    })?))
}

fn sample_sizes(track: &Mp4Track, sample_count: usize) -> Result<Vec<u32>> {
    let table = &track.trak.mdia.minf.stbl.stsz;
    if table.sample_size != 0 {
        return Ok(vec![table.sample_size; sample_count]);
    }
    if table.sample_sizes.len() != sample_count {
        return Err(invalid_media(
            "stsz entry count does not match sample count",
        ));
    }
    Ok(table.sample_sizes.clone())
}

fn sample_offsets(track: &Mp4Track, sizes: &[u32], sample_count: usize) -> Result<Vec<u64>> {
    let table = &track.trak.mdia.minf.stbl;
    let chunks = if let Some(offsets) = &table.stco {
        offsets
            .entries
            .iter()
            .map(|offset| u64::from(*offset))
            .collect()
    } else if let Some(offsets) = &table.co64 {
        offsets.entries.clone()
    } else {
        return Err(invalid_media("missing stco/co64 chunk offsets"));
    };
    if table.stsc.entries.is_empty() {
        return Err(invalid_media("missing stsc entries"));
    }

    let mut offsets = Vec::with_capacity(sample_count);
    let mut sample_index = 0usize;
    for (entry_index, entry) in table.stsc.entries.iter().enumerate() {
        if entry.first_chunk == 0 || entry.samples_per_chunk == 0 {
            return Err(invalid_media("invalid stsc entry"));
        }
        let next_first_chunk = table
            .stsc
            .entries
            .get(entry_index + 1)
            .map_or(chunks.len() as u64 + 1, |next| u64::from(next.first_chunk));

        for chunk_number in u64::from(entry.first_chunk)..next_first_chunk {
            let chunk_index = usize::try_from(chunk_number - 1)
                .map_err(|_| invalid_media("chunk index does not fit in memory"))?;
            let mut offset = *chunks
                .get(chunk_index)
                .ok_or_else(|| invalid_media("stsc references a missing chunk"))?;

            for _ in 0..entry.samples_per_chunk {
                if sample_index == sample_count {
                    return Err(invalid_media("stsc maps more samples than stsz"));
                }
                offsets.push(offset);
                offset = offset
                    .checked_add(u64::from(sizes[sample_index]))
                    .ok_or_else(|| invalid_media("sample offset overflow"))?;
                sample_index += 1;
            }
        }
    }

    if sample_index != sample_count {
        return Err(invalid_media("stsc maps fewer samples than stsz"));
    }
    Ok(offsets)
}

fn sample_times(track: &Mp4Track, sample_count: usize) -> Result<Vec<(u64, u32)>> {
    let mut times = Vec::with_capacity(sample_count);
    let mut decode_time = 0u64;
    for entry in &track.trak.mdia.minf.stbl.stts.entries {
        // Bound the running total before expanding: a single run-length entry can claim
        // billions of samples, and expansion must never outgrow the already-limited count.
        let run = usize::try_from(entry.sample_count)
            .ok()
            .filter(|run| times.len().saturating_add(*run) <= sample_count)
            .ok_or_else(|| invalid_media("stts entry count does not match sample count"))?;
        for _ in 0..run {
            times.push((decode_time, entry.sample_delta));
            decode_time = decode_time
                .checked_add(u64::from(entry.sample_delta))
                .ok_or_else(|| invalid_media("decode timestamp overflow"))?;
        }
    }
    if times.len() != sample_count {
        return Err(invalid_media(
            "stts entry count does not match sample count",
        ));
    }
    Ok(times)
}

fn composition_offsets(track: &Mp4Track, sample_count: usize) -> Result<Vec<i32>> {
    let Some(table) = &track.trak.mdia.minf.stbl.ctts else {
        return Ok(vec![0; sample_count]);
    };
    let mut offsets = Vec::with_capacity(sample_count);
    for entry in &table.entries {
        let run = usize::try_from(entry.sample_count)
            .ok()
            .filter(|run| offsets.len().saturating_add(*run) <= sample_count)
            .ok_or_else(|| invalid_media("ctts entry count does not match sample count"))?;
        offsets.extend(std::iter::repeat_n(entry.sample_offset, run));
    }
    if offsets.len() != sample_count {
        return Err(invalid_media(
            "ctts entry count does not match sample count",
        ));
    }
    Ok(offsets)
}

fn invalid_media(message: &str) -> Error {
    Error::InvalidMedia(message.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn sample_index_matches_ffprobe_packets() {
        let source = LocalMediaSource::open(fixture("h264-aac.mp4")).expect("fixture should open");
        let index = parse(&source, &LimitsConfig::default()).expect("fixture should parse");
        let expected: Value = serde_json::from_slice(
            &std::fs::read(fixture("h264-aac.ffprobe.json")).expect("probe should be readable"),
        )
        .expect("probe should be JSON");
        let packets = expected["packets"]
            .as_array()
            .expect("probe should contain packets");

        assert_eq!(index.tracks.len(), 2);
        for (stream_index, track) in index.tracks.iter().enumerate() {
            let expected_packets = packets
                .iter()
                .filter(|packet| packet["stream_index"].as_u64() == Some(stream_index as u64))
                .collect::<Vec<_>>();
            assert_eq!(track.samples.len(), expected_packets.len());

            let first_dts = json_i64(expected_packets[0], "dts");
            for (sample, packet) in track.samples.iter().zip(expected_packets) {
                let dts = json_i64(packet, "dts");
                let pts = json_i64(packet, "pts");
                assert_eq!(sample.offset, json_u64(packet, "pos"));
                assert_eq!(u64::from(sample.size), json_u64(packet, "size"));
                assert_eq!(sample.decode_time, u64::try_from(dts - first_dts).unwrap());
                assert_eq!(u64::from(sample.duration), json_u64(packet, "duration"));
                assert_eq!(i64::from(sample.composition_offset), pts - dts);
                assert_eq!(
                    sample.is_sync,
                    packet["flags"].as_str().unwrap().contains('K')
                );
            }
        }
    }

    #[test]
    fn parses_moov_after_media_data() {
        let source =
            LocalMediaSource::open(fixture("h264-aac-moov-last.mp4")).expect("fixture should open");

        let index = parse(&source, &LimitsConfig::default()).expect("moov-last MP4 should parse");

        assert_eq!(index.tracks.len(), 2);
    }

    #[test]
    fn rejects_edit_lists() {
        let source =
            LocalMediaSource::open(fixture("h264-aac-edit-list.mp4")).expect("fixture should open");

        let error =
            parse(&source, &LimitsConfig::default()).expect_err("edit-list MP4 should be rejected");

        assert!(error.to_string().contains("edit lists"));
    }

    #[test]
    fn parses_video_without_audio() {
        let source =
            LocalMediaSource::open(fixture("h264-video-only.mp4")).expect("fixture should open");

        let index = parse(&source, &LimitsConfig::default()).expect("video-only MP4 should parse");

        assert_eq!(index.tracks.len(), 1);
        assert_eq!(index.tracks[0].kind, TrackKind::Video);
    }

    #[test]
    fn parses_44100_hz_stereo_aac() {
        let source = LocalMediaSource::open(fixture("h264-aac-44100-stereo.mp4"))
            .expect("fixture should open");

        let index = parse(&source, &LimitsConfig::default()).expect("AAC variant should parse");
        let audio = index
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .expect("fixture should contain audio");

        assert_eq!(
            audio.codec,
            CodecConfig::Aac {
                sample_rate: 44_100,
                channels: 2,
            }
        );
    }

    #[test]
    fn preserves_variable_sample_durations() {
        let source = LocalMediaSource::open(fixture("h264-variable-timing.mp4"))
            .expect("fixture should open");

        let index = parse(&source, &LimitsConfig::default()).expect("VFR fixture should parse");
        let durations = index.tracks[0]
            .samples
            .iter()
            .map(|sample| sample.duration)
            .collect::<HashSet<_>>();

        assert!(durations.len() > 1);
    }

    #[test]
    fn rejects_malformed_child_box_size() {
        let mut moov = fixture_moov();
        moov[8..12].copy_from_slice(&4u32.to_be_bytes());

        let error = validate_raw_moov(&moov).expect_err("undersized child box should fail");

        assert!(error.to_string().contains("box size"));
    }

    #[test]
    fn rejects_truncated_metadata() {
        let moov = fixture_moov();

        let error =
            validate_raw_moov(&moov[..moov.len() / 2]).expect_err("truncated moov should fail");

        assert!(error.to_string().contains("box size"));
    }

    #[test]
    fn rejects_truncated_sample_payload() {
        let original = fixture("h264-aac.mp4");
        let original_source = LocalMediaSource::open(&original).expect("fixture should open");
        let index =
            parse(&original_source, &LimitsConfig::default()).expect("fixture should parse");
        let final_sample_end = index
            .tracks
            .iter()
            .flat_map(|track| &track.samples)
            .map(|sample| sample.offset + u64::from(sample.size))
            .max()
            .expect("fixture should contain samples");
        let truncated =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/h264-aac-truncated-payload.mp4");
        std::fs::copy(original, &truncated).expect("fixture should copy");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&truncated)
            .expect("copy should open")
            .set_len(final_sample_end - 1)
            .expect("copy should truncate");
        let source = LocalMediaSource::open(&truncated).expect("truncated fixture should open");

        let error = parse(&source, &LimitsConfig::default())
            .expect_err("truncated sample payload should fail");

        assert!(error.to_string().contains("sample byte range exceeds"));
        std::fs::remove_file(truncated).expect("temporary fixture should be removable");
    }

    #[test]
    fn rejects_multiple_sample_descriptions() {
        let mut moov = fixture_moov();
        let stsd = find_type(&moov, *b"stsd");
        moov[stsd + 8..stsd + 12].copy_from_slice(&2u32.to_be_bytes());

        let error = validate_raw_moov(&moov).expect_err("multiple descriptions should fail");

        assert!(error.to_string().contains("exactly one sample description"));
    }

    #[test]
    fn rejects_encrypted_sample_entries() {
        let mut moov = fixture_moov();
        let avc1 = find_type(&moov, *b"avc1");
        moov[avc1..avc1 + 4].copy_from_slice(b"encv");

        let error = validate_raw_moov(&moov).expect_err("encrypted media should fail");

        assert!(error.to_string().contains("encrypted media"));
    }

    #[test]
    fn rejects_external_data_references() {
        let mut moov = fixture_moov();
        let url = find_type(&moov, *b"url ");
        moov[url + 7] = 0;

        let error = validate_raw_moov(&moov).expect_err("external reference should fail");

        assert!(error.to_string().contains("external data reference"));
    }

    #[test]
    fn rejects_run_length_entries_that_claim_more_samples_than_stsz() {
        let original = std::fs::read(fixture("h264-aac.mp4")).expect("fixture should read");
        let mut mutated = original.clone();
        // stts payload: version/flags (4), entry count (4), then (sample_count, delta) pairs.
        let stts = find_type(&mutated, *b"stts");
        mutated[stts + 12..stts + 16].copy_from_slice(&i32::MAX.to_be_bytes());
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/h264-aac-huge-stts.mp4");
        std::fs::write(&path, mutated).expect("mutated fixture should write");
        let source = LocalMediaSource::open(&path).expect("mutated fixture should open");

        let error = parse(&source, &LimitsConfig::default())
            .expect_err("oversized run-length entry should fail before expansion");

        assert!(error.to_string().contains("stts entry count"), "{error}");
        std::fs::remove_file(path).expect("temporary fixture should be removable");
    }

    fn fixture_moov() -> Vec<u8> {
        let source = LocalMediaSource::open(fixture("h264-aac.mp4")).expect("fixture should open");
        let range = find_moov(&source, u64::MAX).expect("fixture should contain moov");
        source
            .read_range(range)
            .expect("moov should be readable")
            .to_vec()
    }

    fn find_type(bytes: &[u8], name: [u8; 4]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == name)
            .unwrap_or_else(|| panic!("fixture should contain {}", String::from_utf8_lossy(&name)))
    }

    fn json_i64(value: &Value, field: &str) -> i64 {
        value[field].as_i64().unwrap_or_else(|| {
            value[field]
                .as_str()
                .expect("field should be an integer string")
                .parse()
                .expect("field should parse as an integer")
        })
    }

    fn json_u64(value: &Value, field: &str) -> u64 {
        u64::try_from(json_i64(value, field)).expect("field should be non-negative")
    }
}
