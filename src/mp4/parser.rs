use std::collections::{HashMap, HashSet};

use ::mp4::{MediaType, Mp4Reader, Mp4Track, TrackType};
use sha2::{Digest, Sha256};

use super::edit::{self, ElstEntry, TrackEdit};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, MediaIndex, Sample, SkippedTrack, Track, TrackKey, TrackKind};
use crate::source::{MediaSourceKind, SourceIdentity, SparseFile};

/// A parsed file: the sample index plus the metadata regions it was built from, which the init
/// segment writer reuses so the file is not read again.
#[derive(Debug)]
pub(crate) struct ParsedMedia {
    pub(crate) index: MediaIndex,
    pub(crate) metadata: SparseFile,
}

/// Fetches a file's metadata, builds its sample index, and confirms the source did not change.
///
/// Only box headers, `ftyp`, and `moov` are read, whether the source is a local file or a remote
/// object. The CPU-bound table expansion runs on the blocking pool.
pub(crate) async fn parse(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<ParsedMedia> {
    if source.len() > limits.max_source_bytes {
        return Err(Error::Unsupported(
            "source exceeds configured size limit".to_owned(),
        ));
    }
    let metadata = SparseFile::fetch(source, limits.max_metadata_bytes).await?;
    let identity = source.identity().clone();
    let limits_for_parse = limits.clone();
    let (metadata, index) = tokio::task::spawn_blocking(move || {
        parse_metadata(&metadata, identity, &limits_for_parse).map(|index| (metadata, index))
    })
    .await
    .map_err(|error| Error::Io(std::io::Error::other(error)))??;

    // Mutation check: the object must be unchanged, and `moov` must hash the same when re-read.
    source.verify_unchanged().await?;
    let current = source.read_range(metadata.moov_range()).await?;
    let expected = index
        .source
        .moov_sha256
        .expect("parse_metadata always records the moov hash");
    if Sha256::digest(&current)[..] != expected[..] {
        return Err(invalid_media("moov changed while it was being parsed"));
    }
    Ok(ParsedMedia { index, metadata })
}

/// The synchronous, CPU-bound half of parsing: validation, table expansion, and limits.
fn parse_metadata(
    metadata: &SparseFile,
    mut identity: SourceIdentity,
    limits: &LimitsConfig,
) -> Result<MediaIndex> {
    let moov_bytes = metadata.moov_bytes();
    let raw = validate_raw_moov(moov_bytes)?;
    let moov_sha256: [u8; 32] = Sha256::digest(moov_bytes).into();
    let reader = Mp4Reader::read_header(metadata.reader(), metadata.len())?;

    if reader.is_fragmented() {
        return Err(fragmented());
    }
    if reader.moov.traks.len() > limits.max_tracks {
        return Err(Error::Unsupported(format!(
            "track count {} exceeds configured limit {}",
            reader.moov.traks.len(),
            limits.max_tracks
        )));
    }

    let movie_timescale = reader.moov.mvhd.timescale;
    let mut tracks = Vec::new();
    let mut edits = Vec::new();
    let mut skipped_tracks = Vec::new();
    let mut ids = reader.tracks().keys().copied().collect::<Vec<_>>();
    ids.sort_unstable();
    for id in ids {
        let track = &reader.tracks()[&id];
        if let Some(RawTrack {
            disposition: Disposition::Skip(reason),
            handler,
        }) = raw.get(&id)
        {
            skipped_tracks.push(SkippedTrack {
                id,
                handler: fourcc(*handler),
                reason,
            });
        } else {
            edits.push(track_edit(track, movie_timescale)?);
            tracks.push(parse_track(track, metadata.len(), limits)?);
        }
    }

    let timescales = edits
        .iter()
        .zip(&tracks)
        .map(|(edit, track)| (*edit, track.timescale))
        .collect::<Vec<_>>();
    let shifts = edit::timeline_shifts(&timescales, movie_timescale)?;
    for ((track, edit), shift) in tracks.iter_mut().zip(edits).zip(shifts) {
        edit::apply(track, edit, shift)?;
    }
    assign_keys(&mut tracks);
    identity.moov_sha256 = Some(moov_sha256);

    Ok(MediaIndex {
        source: identity,
        movie_timescale,
        duration: reader.moov.mvhd.duration,
        tracks,
        skipped_tracks,
    })
}

fn track_edit(track: &Mp4Track, movie_timescale: u32) -> Result<TrackEdit> {
    let Some(elst) = track.trak.edts.as_ref().and_then(|edts| edts.elst.as_ref()) else {
        return Ok(TrackEdit::NONE);
    };
    let entries = elst
        .entries
        .iter()
        .map(|entry| ElstEntry {
            segment_duration: entry.segment_duration,
            media_time: entry.media_time,
            media_rate: entry.media_rate,
            media_rate_fraction: entry.media_rate_fraction,
        })
        .collect::<Vec<_>>();
    edit::parse_edit_list(
        track.track_id(),
        elst.version,
        &entries,
        movie_timescale,
        track.timescale(),
    )
}

/// Names the tracks the way URLs will: one video, then audio numbered in file order.
fn assign_keys(tracks: &mut [Track]) {
    let mut audio = 0u16;
    for track in tracks {
        track.key = match track.kind {
            TrackKind::Video => TrackKey::VIDEO,
            TrackKind::Audio => {
                audio = audio.saturating_add(1);
                TrackKey::audio(audio)
            }
        };
    }
}

fn parse_track(track: &Mp4Track, source_len: u64, limits: &LimitsConfig) -> Result<Track> {
    let kind = match track.track_type()? {
        TrackType::Audio => TrackKind::Audio,
        TrackType::Video => TrackKind::Video,
        TrackType::Subtitle => {
            return Err(Error::Unsupported(format!(
                "track {}: subtitle tracks are not supported",
                track.track_id()
            )));
        }
    };
    let codec = parse_codec(track)?;
    let samples = parse_samples(track, source_len, limits)?;

    let mdia = &track.trak.mdia;
    Ok(Track {
        id: track.track_id(),
        // Replaced once every track is known; see `assign_keys`.
        key: TrackKey::VIDEO,
        kind,
        language: mdia.mdhd.language.clone(),
        timeline_shift: 0,
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
            let avc =
                sample_table.stsd.avc1.as_ref().ok_or_else(|| {
                    Error::Unsupported("H.264 without avc1 sample entry".to_owned())
                })?;
            let sequence_parameter_set = avc
                .avcc
                .sequence_parameter_sets
                .first()
                .ok_or_else(|| Error::Unsupported("H.264 without SPS".to_owned()))?
                .bytes
                .clone();
            let picture_parameter_set = avc
                .avcc
                .picture_parameter_sets
                .first()
                .ok_or_else(|| Error::Unsupported("H.264 without PPS".to_owned()))?
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
            let aac =
                sample_table.stsd.mp4a.as_ref().ok_or_else(|| {
                    Error::Unsupported("AAC without mp4a sample entry".to_owned())
                })?;

            if track.audio_profile()? != ::mp4::AudioObjectType::AacLowComplexity {
                return Err(Error::Unsupported(format!(
                    "track {}: AAC profile {:?} is not supported, only AAC-LC is",
                    track.track_id(),
                    track.audio_profile()?
                )));
            }

            Ok(CodecConfig::Aac {
                sample_rate: u32::from(aac.samplerate.value()),
                channels: aac.channelcount,
            })
        }
        other => Err(Error::Unsupported(format!(
            "track {}: codec {other} is not supported, only H.264 and AAC-LC are",
            track.track_id()
        ))),
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

/// What preflight learned about one `trak`, before the `mp4` crate sees it.
#[derive(Debug)]
struct RawTrack {
    handler: [u8; 4],
    disposition: Disposition,
}

#[derive(Debug)]
enum Disposition {
    Package,
    Skip(&'static str),
}

fn fragmented() -> Error {
    Error::Unsupported(
        "fragmented MP4 input is not supported; re-mux it to a regular MP4 with `ffmpeg -i in.mp4 -c copy out.mp4`"
            .to_owned(),
    )
}

/// Checks the raw `moov` structure and decides, per track, whether it is packaged.
///
/// Runs before the `mp4` crate so that unsupported constructs fail with a specific message and
/// tracks that are not audio or video never reach the crate at all.
fn validate_raw_moov(moov: &[u8]) -> Result<HashMap<u32, RawTrack>> {
    let root = box_payload(moov, 0)?;
    if root.name != *b"moov" {
        return Err(invalid_media("metadata range is not a moov box"));
    }
    let children = child_boxes(root.payload)?;
    if children.iter().any(|child| child.name == *b"mvex") {
        return Err(fragmented());
    }
    let mut tracks = HashMap::new();
    for track in children.into_iter().filter(|child| child.name == *b"trak") {
        let header = required_child(track.payload, *b"tkhd")?;
        let id = track_id(header.payload)?;
        let media = required_child(track.payload, *b"mdia")?;
        let handler = handler_type(required_child(media.payload, *b"hdlr")?.payload)?;
        let disposition = if handler == *b"vide" || handler == *b"soun" {
            let media_info = required_child(media.payload, *b"minf")?;
            validate_data_reference(media_info.payload)?;
            let sample_table = required_child(media_info.payload, *b"stbl")?;
            classify_sample_description(id, handler, sample_table.payload)?
        } else {
            // Timecode, timed metadata, chapters, and hint tracks: nothing to package.
            Disposition::Skip("not an audio or video track")
        };
        if tracks
            .insert(
                id,
                RawTrack {
                    handler,
                    disposition,
                },
            )
            .is_some()
        {
            return Err(invalid_media("two tracks share one track ID"));
        }
    }
    Ok(tracks)
}

fn track_id(tkhd: &[u8]) -> Result<u32> {
    let offset = match tkhd.first() {
        Some(1) => 20,
        Some(_) => 12,
        None => return Err(invalid_media("tkhd payload is truncated")),
    };
    tkhd.get(offset..offset + 4)
        .ok_or_else(|| invalid_media("tkhd payload is truncated"))
        .and_then(read_u32)
}

fn handler_type(hdlr: &[u8]) -> Result<[u8; 4]> {
    hdlr.get(8..12)
        .map(|bytes| bytes.try_into().expect("slice is four bytes"))
        .ok_or_else(|| invalid_media("hdlr payload is truncated"))
}

/// A printable spelling of a four-character code for error messages and logs.
fn fourcc(code: [u8; 4]) -> String {
    code.iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect()
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
            "exactly one self-contained data reference is required".to_owned(),
        ));
    }
    let entries = child_boxes(&data_reference.payload[8..])?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid_media("dref entry is missing"))?;
    if entry.name != *b"url " || entry.payload.len() < 4 {
        return Err(Error::Unsupported(
            "external data references are not supported".to_owned(),
        ));
    }
    let flags = read_u32(&[0, entry.payload[1], entry.payload[2], entry.payload[3]])?;
    if flags & 1 == 0 {
        return Err(Error::Unsupported(
            "external data references are not supported".to_owned(),
        ));
    }
    Ok(())
}

fn classify_sample_description(
    track_id: u32,
    handler: [u8; 4],
    sample_table: &[u8],
) -> Result<Disposition> {
    let description = required_child(sample_table, *b"stsd")?;
    if description.payload.len() < 8 {
        return Err(invalid_media("stsd payload is truncated"));
    }
    let entry_count = read_u32(&description.payload[4..8])?;
    if entry_count != 1 {
        return Err(Error::Unsupported(format!(
            "track {track_id}: {entry_count} sample descriptions; exactly one is required"
        )));
    }
    let entries = child_boxes(&description.payload[8..])?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid_media("stsd entry is missing"))?;
    match &entry.name {
        b"avc1" | b"mp4a" => Ok(Disposition::Package),
        b"encv" | b"enca" => Err(Error::Unsupported(format!(
            "track {track_id}: encrypted media is not supported"
        ))),
        _ if handler == *b"vide" && sample_count(sample_table)? <= 1 => {
            // A single still picture, such as cover art stored as a track.
            Ok(Disposition::Skip("single still image"))
        }
        name => Err(Error::Unsupported(format!(
            "track {track_id}: sample description `{}` is not supported",
            fourcc(*name)
        ))),
    }
}

fn sample_count(sample_table: &[u8]) -> Result<u32> {
    let sizes = required_child(sample_table, *b"stsz")?;
    sizes
        .payload
        .get(8..12)
        .ok_or_else(|| invalid_media("stsz payload is truncated"))
        .and_then(read_u32)
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
    use crate::source::LocalMediaSource;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(future)
    }

    fn open_kind(path: impl AsRef<std::path::Path>) -> Result<MediaSourceKind> {
        Ok(MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path)?,
        )))
    }

    fn parse_index(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<MediaIndex> {
        block_on(parse(source, limits)).map(|parsed| parsed.index)
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn sample_index_matches_ffprobe_packets() {
        let source = open_kind(fixture("h264-aac.mp4")).expect("fixture should open");
        let index = parse_index(&source, &LimitsConfig::default()).expect("fixture should parse");
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
        let source = open_kind(fixture("h264-aac-moov-last.mp4")).expect("fixture should open");

        let index =
            parse_index(&source, &LimitsConfig::default()).expect("moov-last MP4 should parse");

        assert_eq!(index.tracks.len(), 2);
    }

    /// Parses a fixture that must be rejected, without printing its whole index on failure.
    fn parse_error(name: &str) -> Error {
        let source = open_kind(fixture(name)).expect("fixture should open");
        match parse_index(&source, &LimitsConfig::default()) {
            Err(error) => error,
            Ok(_) => panic!("{name} should have been rejected"),
        }
    }

    fn parse_fixture(name: &str) -> MediaIndex {
        let source = open_kind(fixture(name)).expect("fixture should open");
        parse_index(&source, &LimitsConfig::default()).expect("fixture should parse")
    }

    fn track_of(index: &MediaIndex, key: TrackKey) -> &Track {
        index
            .tracks
            .iter()
            .find(|track| track.key == key)
            .expect("track should exist")
    }

    #[test]
    fn applies_the_edit_lists_ffmpeg_writes_by_default() {
        // Video: 1024 ticks of 15360 (66.7 ms) of B-frame delay. Audio: 1024 ticks of priming.
        let index = parse_fixture("h264-aac-default-edits.mp4");
        let video = track_of(&index, TrackKey::VIDEO);
        let audio = track_of(&index, TrackKey::audio(1));

        assert_eq!(video.timeline_shift, 0);
        assert_eq!(audio.timeline_shift, 2176);
        assert_eq!(audio.samples.len(), 141, "the priming frame is dropped");
        // The first frame and the first kept audio sample present at the same instant:
        // 1024 / 15360 s == 3200 / 48000 s.
        let video_start = video.samples[0].decode_time
            + u64::try_from(video.samples[0].composition_offset).unwrap();
        assert_eq!(video_start * 48_000, audio.samples[0].decode_time * 15_360);
    }

    #[test]
    fn a_leading_empty_edit_delays_audio_by_exactly_that_much() {
        let index = parse_fixture("h264-aac-audio-delay.mp4");
        let video = track_of(&index, TrackKey::VIDEO);
        let audio = track_of(&index, TrackKey::audio(1));

        assert_eq!(video.timeline_shift, 0);
        assert_eq!(audio.timeline_shift, 26_176);
        // The gap between the tracks is the empty edit: 22976 ticks of 48000.
        let video_start = video.samples[0].decode_time
            + u64::try_from(video.samples[0].composition_offset).unwrap();
        let gap = audio.samples[0].decode_time * 15_360 - video_start * 48_000;
        assert_eq!(gap, 22_976 * 15_360);
    }

    #[test]
    fn a_file_without_edit_lists_keeps_its_timestamps() {
        let index = parse_fixture("h264-aac.mp4");

        assert!(index.tracks.iter().all(|track| track.timeline_shift == 0));
        assert_eq!(index.tracks[1].samples[0].decode_time, 0);
        assert_eq!(index.tracks[1].samples.len(), 141 + 1);
    }

    #[test]
    fn fragmented_input_is_rejected_with_a_specific_message() {
        let error = parse_error("h264-aac-fragmented.mp4");

        assert!(matches!(error, Error::Unsupported(_)), "{error}");
        assert!(
            error.to_string().contains("fragmented MP4 input"),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_codec_is_named_in_the_error() {
        let error = parse_error("hevc-aac.mp4");

        assert!(matches!(error, Error::Unsupported(_)), "{error}");
        assert!(error.to_string().contains("track 1"), "{error}");
        assert!(error.to_string().contains("`hvc1`"), "{error}");
    }

    #[test]
    fn skips_tracks_that_are_not_audio_or_video() {
        let index = parse_fixture("h264-aac-timecode.mp4");

        assert_eq!(index.tracks.len(), 2);
        assert_eq!(index.skipped_tracks.len(), 1);
        assert_eq!(index.skipped_tracks[0].handler, "tmcd");
        assert_eq!(index.skipped_tracks[0].id, 3);
    }

    #[test]
    fn numbers_audio_tracks_in_file_order_and_reads_their_languages() {
        let index = parse_fixture("h264-aac-two-audio.mp4");

        let keys = index
            .tracks
            .iter()
            .map(|track| track.key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [TrackKey::VIDEO, TrackKey::audio(1), TrackKey::audio(2)]
        );
        assert_eq!(index.tracks[1].language, "eng");
        assert_eq!(index.tracks[2].language, "spa");
    }

    #[test]
    fn a_single_audio_track_is_still_numbered() {
        let index = parse_fixture("h264-aac.mp4");

        assert_eq!(index.tracks[1].key, TrackKey::audio(1));
        assert_eq!(index.tracks[1].language, "und");
    }

    #[test]
    fn parses_video_without_audio() {
        let source = open_kind(fixture("h264-video-only.mp4")).expect("fixture should open");

        let index =
            parse_index(&source, &LimitsConfig::default()).expect("video-only MP4 should parse");

        assert_eq!(index.tracks.len(), 1);
        assert_eq!(index.tracks[0].kind, TrackKind::Video);
    }

    #[test]
    fn parses_44100_hz_stereo_aac() {
        let source = open_kind(fixture("h264-aac-44100-stereo.mp4")).expect("fixture should open");

        let index =
            parse_index(&source, &LimitsConfig::default()).expect("AAC variant should parse");
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
        let source = open_kind(fixture("h264-variable-timing.mp4")).expect("fixture should open");

        let index =
            parse_index(&source, &LimitsConfig::default()).expect("VFR fixture should parse");
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
        let original_source = open_kind(&original).expect("fixture should open");
        let index =
            parse_index(&original_source, &LimitsConfig::default()).expect("fixture should parse");
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
        let source = open_kind(&truncated).expect("truncated fixture should open");

        let error = parse_index(&source, &LimitsConfig::default())
            .expect_err("truncated sample payload should fail");

        // Discovery walks every top-level box, so the truncated `mdat` is caught before any
        // sample table is expanded.
        assert!(
            error.to_string().contains("invalid top-level MP4 box size"),
            "{error}"
        );
        std::fs::remove_file(truncated).expect("temporary fixture should be removable");
    }

    #[test]
    fn rejects_multiple_sample_descriptions() {
        let mut moov = fixture_moov();
        let stsd = find_type(&moov, *b"stsd");
        moov[stsd + 8..stsd + 12].copy_from_slice(&2u32.to_be_bytes());

        let error = validate_raw_moov(&moov).expect_err("multiple descriptions should fail");

        assert!(
            error.to_string().contains("2 sample descriptions"),
            "{error}"
        );
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
        let source = open_kind(&path).expect("mutated fixture should open");

        let error = parse_index(&source, &LimitsConfig::default())
            .expect_err("oversized run-length entry should fail before expansion");

        assert!(error.to_string().contains("stts entry count"), "{error}");
        std::fs::remove_file(path).expect("temporary fixture should be removable");
    }

    fn fixture_moov() -> Vec<u8> {
        let source = open_kind(fixture("h264-aac.mp4")).expect("fixture should open");
        block_on(SparseFile::fetch(&source, u64::MAX))
            .expect("fixture should contain moov")
            .moov_bytes()
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
