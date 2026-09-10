use std::collections::HashSet;

use ::mp4::{MediaType, Mp4Reader, Mp4Track, TrackType};

use crate::error::{Error, Result};
use crate::media::{CodecConfig, MediaIndex, Sample, Track, TrackKind};
use crate::source::{LocalMediaSource, MediaSource};

pub(crate) fn parse(source: &LocalMediaSource) -> Result<MediaIndex> {
    let file = source.open_file()?;
    let reader = Mp4Reader::read_header(file, source.len())?;

    if reader.is_fragmented() {
        return Err(Error::Unsupported("fragmented MP4 input"));
    }

    let mut tracks = reader
        .tracks()
        .values()
        .map(|track| parse_track(track, source.len()))
        .collect::<Result<Vec<_>>>()?;
    tracks.sort_unstable_by_key(|track| track.id);

    Ok(MediaIndex {
        source: source.identity().clone(),
        movie_timescale: reader.moov.mvhd.timescale,
        duration: reader.moov.mvhd.duration,
        tracks,
    })
}

fn parse_track(track: &Mp4Track, source_len: u64) -> Result<Track> {
    let kind = match track.track_type()? {
        TrackType::Audio => TrackKind::Audio,
        TrackType::Video => TrackKind::Video,
        TrackType::Subtitle => return Err(Error::Unsupported("subtitle track")),
    };
    let codec = parse_codec(track)?;
    let samples = parse_samples(track, source_len)?;

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

fn parse_samples(track: &Mp4Track, source_len: u64) -> Result<Vec<Sample>> {
    let sample_table = &track.trak.mdia.minf.stbl;
    let sample_count = usize::try_from(sample_table.stsz.sample_count)
        .map_err(|_| invalid_media("sample count does not fit in memory"))?;
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
        for _ in 0..entry.sample_count {
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
        offsets.extend(std::iter::repeat_n(
            entry.sample_offset,
            entry.sample_count as usize,
        ));
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
        let index = parse(&source).expect("fixture should parse");
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
