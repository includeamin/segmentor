use sha2::{Digest, Sha256};

use super::boxes::{
    RawBox, Reader, child_boxes, fourcc, invalid_media, optional_child, required_child,
};
use super::codec;
use super::edit::{self, ElstEntry, TrackEdit};
use super::tables::{expand_samples, parse_sample_tables};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, SkippedTrack, Track, TrackKey, TrackKind};
use crate::source::{MediaSourceKind, Metadata, SourceIdentity};

/// A parsed file: the sample index plus the `moov` box it was built from, which the init
/// segment writer reuses so the file is not read again.
#[derive(Debug)]
pub(crate) struct ParsedMedia {
    pub(crate) index: MediaIndex,
    pub(crate) metadata: Metadata,
}

/// Fetches a file's metadata, builds its sample index, and confirms the source did not change.
///
/// Only box headers and `moov` are read, whether the source is a local file or a remote
/// object. The CPU-bound table expansion runs on the blocking pool.
pub(crate) async fn parse(source: &MediaSourceKind, limits: &LimitsConfig) -> Result<ParsedMedia> {
    if source.len() > limits.max_source_bytes {
        return Err(Error::Unsupported(
            "source exceeds configured size limit".to_owned(),
        ));
    }
    let metadata = Metadata::fetch(source, limits.max_metadata_bytes).await?;
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
    metadata: &Metadata,
    mut identity: SourceIdentity,
    limits: &LimitsConfig,
) -> Result<MediaIndex> {
    let moov_bytes = metadata.moov_bytes();
    let moov = validate_raw_moov(moov_bytes)?;
    let moov_sha256: [u8; 32] = Sha256::digest(moov_bytes).into();
    if moov.tracks.len() > limits.max_tracks {
        return Err(Error::Unsupported(format!(
            "track count {} exceeds configured limit {}",
            moov.tracks.len(),
            limits.max_tracks
        )));
    }

    let mut tracks = Vec::new();
    let mut edits = Vec::new();
    let mut skipped_tracks = Vec::new();
    for raw in &moov.tracks {
        if let Disposition::Skip(reason) = raw.disposition {
            skipped_tracks.push(SkippedTrack {
                id: raw.id,
                handler: fourcc(raw.handler),
                reason,
            });
        } else {
            let (track, edit) = parse_track(raw, moov.movie_timescale, metadata.len(), limits)?;
            tracks.push(track);
            edits.push(edit);
        }
    }

    let timescales = edits
        .iter()
        .zip(&tracks)
        .map(|(edit, track)| (*edit, track.timescale))
        .collect::<Vec<_>>();
    let shifts = edit::timeline_shifts(&timescales, moov.movie_timescale)?;
    for ((track, edit), shift) in tracks.iter_mut().zip(edits).zip(shifts) {
        edit::apply(track, edit, shift)?;
    }
    assign_keys(&mut tracks);
    identity.moov_sha256 = Some(moov_sha256);

    Ok(MediaIndex {
        source: identity,
        movie_timescale: moov.movie_timescale,
        duration: moov.movie_duration,
        tracks,
        skipped_tracks,
    })
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

fn parse_track(
    raw: &RawTrack<'_>,
    movie_timescale: u32,
    source_len: u64,
    limits: &LimitsConfig,
) -> Result<(Track, TrackEdit)> {
    let kind = if raw.handler == *b"vide" {
        TrackKind::Video
    } else {
        TrackKind::Audio
    };
    let media = required_child(raw.trak, *b"mdia")?;
    let header = parse_media_header(required_child(media.payload, *b"mdhd")?.payload, raw.id)?;
    let stbl = required_child(required_child(media.payload, *b"minf")?.payload, *b"stbl")?;
    let entry = sample_entry(stbl.payload)?;
    let codec = match (&entry.name, kind) {
        (b"avc1", TrackKind::Video) => codec::parse_avc(entry.payload)?,
        (b"hvc1" | b"hev1", TrackKind::Video) => codec::parse_hevc(entry.payload, entry.name)?,
        (b"vp09", TrackKind::Video) => codec::parse_vp9(entry.payload)?,
        (b"av01", TrackKind::Video) => codec::parse_av1(entry.payload)?,
        (b"mp4a", TrackKind::Audio) => codec::parse_aac(entry.payload, raw.id)?,
        (b"ac-3", TrackKind::Audio) => codec::parse_ac3(entry.payload, raw.id)?,
        (b"ec-3", TrackKind::Audio) => codec::parse_eac3(entry.payload, raw.id)?,
        (b"Opus", TrackKind::Audio) => codec::parse_opus(entry.payload, raw.id)?,
        (b"fLaC", TrackKind::Audio) => codec::parse_flac(entry.payload, raw.id)?,
        (name, _) => {
            return Err(Error::Unsupported(format!(
                "track {}: sample description `{}` does not belong in a {} track",
                raw.id,
                fourcc(*name),
                fourcc(raw.handler)
            )));
        }
    };
    let samples = expand_samples(&parse_sample_tables(stbl.payload)?, source_len, limits)?;
    let edit = track_edit(raw, movie_timescale, header.timescale)?;

    Ok((
        Track {
            id: raw.id,
            // Replaced once every track is known; see `assign_keys`.
            key: TrackKey::VIDEO,
            kind,
            language: header.language,
            timeline_shift: 0,
            timescale: header.timescale,
            duration: header.duration,
            codec,
            samples,
        },
        edit,
    ))
}

/// What `mdhd` says about a track's media.
struct MediaHeader {
    timescale: u32,
    duration: u64,
    language: String,
}

fn parse_media_header(payload: &[u8], track_id: u32) -> Result<MediaHeader> {
    let mut reader = Reader::new(payload);
    let version = reader.full_box()?;
    let (timescale, duration) = read_timescale_and_duration(&mut reader, version)?;
    if timescale == 0 {
        return Err(invalid_media(&format!(
            "track {track_id}: timescale is zero"
        )));
    }
    // Three five-bit letters, each stored as an offset from 0x60, after a padding bit.
    let packed = reader.u16()?;
    let language = [10, 5, 0]
        .into_iter()
        .map(|shift| char::from(u8::try_from((packed >> shift) & 0x1f).unwrap_or(0) + 0x60))
        .collect();
    Ok(MediaHeader {
        timescale,
        duration,
        language,
    })
}

/// The creation and modification times are skipped; version 1 widens every time field.
fn read_timescale_and_duration(reader: &mut Reader<'_>, version: u8) -> Result<(u32, u64)> {
    if version == 1 {
        reader.skip(16)?;
        Ok((reader.u32()?, reader.u64()?))
    } else {
        reader.skip(8)?;
        Ok((reader.u32()?, u64::from(reader.u32()?)))
    }
}

/// The track's edit list, or the identity edit when it has none.
fn track_edit(raw: &RawTrack<'_>, movie_timescale: u32, track_timescale: u32) -> Result<TrackEdit> {
    let Some(edts) = optional_child(raw.trak, *b"edts")? else {
        return Ok(TrackEdit::NONE);
    };
    let Some(elst) = optional_child(edts.payload, *b"elst")? else {
        return Ok(TrackEdit::NONE);
    };
    let mut reader = Reader::new(elst.payload);
    let version = reader.full_box()?;
    let count = reader.entry_count(if version == 1 { 20 } else { 12 })?;
    let entries = (0..count)
        .map(|_| {
            let (segment_duration, media_time) = if version == 1 {
                (reader.u64()?, reader.u64()?)
            } else {
                (u64::from(reader.u32()?), u64::from(reader.u32()?))
            };
            Ok(ElstEntry {
                segment_duration,
                media_time,
                media_rate: reader.u16()?,
                media_rate_fraction: reader.u16()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    edit::parse_edit_list(raw.id, version, &entries, movie_timescale, track_timescale)
}

/// What preflight learned about one `trak`.
#[derive(Debug)]
struct RawMoov<'a> {
    movie_timescale: u32,
    movie_duration: u64,
    tracks: Vec<RawTrack<'a>>,
}

#[derive(Debug)]
struct RawTrack<'a> {
    id: u32,
    handler: [u8; 4],
    disposition: Disposition,
    /// The `trak` payload, for the detailed parse of tracks that are packaged.
    trak: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
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
/// Unsupported constructs fail here with a specific message, and tracks that are not audio or
/// video are set aside without their tables ever being read.
fn validate_raw_moov(moov: &[u8]) -> Result<RawMoov<'_>> {
    let root = super::boxes::box_payload(moov, 0)?;
    if root.name != *b"moov" {
        return Err(invalid_media("metadata range is not a moov box"));
    }
    let children = child_boxes(root.payload)?;
    if children.iter().any(|child| child.name == *b"mvex") {
        return Err(fragmented());
    }
    let header = children
        .iter()
        .find(|child| child.name == *b"mvhd")
        .ok_or_else(|| invalid_media("required MP4 box `mvhd` is missing"))?;
    let mut header = Reader::new(header.payload);
    let version = header.full_box()?;
    let (movie_timescale, movie_duration) = read_timescale_and_duration(&mut header, version)?;
    if movie_timescale == 0 {
        return Err(invalid_media("movie timescale is zero"));
    }

    let mut tracks: Vec<RawTrack<'_>> = Vec::new();
    for track in children.into_iter().filter(|child| child.name == *b"trak") {
        let id = track_id(required_child(track.payload, *b"tkhd")?.payload)?;
        let media = required_child(track.payload, *b"mdia")?;
        let handler = handler_type(required_child(media.payload, *b"hdlr")?.payload)?;
        let disposition = if handler == *b"vide" || handler == *b"soun" {
            let media_info = required_child(media.payload, *b"minf")?;
            validate_data_reference(media_info.payload)?;
            let sample_table = required_child(media_info.payload, *b"stbl")?;
            classify_sample_description(id, handler, sample_table.payload)?
        } else {
            // Timecode, timed metadata, chapters, subtitles, and hint tracks: nothing to package.
            Disposition::Skip("not an audio or video track")
        };
        if tracks.iter().any(|existing| existing.id == id) {
            return Err(invalid_media("two tracks share one track ID"));
        }
        tracks.push(RawTrack {
            id,
            handler,
            disposition,
            trak: track.payload,
        });
    }
    Ok(RawMoov {
        movie_timescale,
        movie_duration,
        tracks,
    })
}

fn track_id(tkhd: &[u8]) -> Result<u32> {
    let mut reader = Reader::new(tkhd);
    let version = reader.full_box()?;
    // Creation and modification times come first; version 1 widens both.
    reader.skip(if version == 1 { 16 } else { 8 })?;
    reader.u32()
}

fn handler_type(hdlr: &[u8]) -> Result<[u8; 4]> {
    hdlr.get(8..12)
        .map(|bytes| bytes.try_into().expect("slice is four bytes"))
        .ok_or_else(|| invalid_media("hdlr payload is truncated"))
}

fn validate_data_reference(media_info: &[u8]) -> Result<()> {
    let data_info = required_child(media_info, *b"dinf")?;
    let data_reference = required_child(data_info.payload, *b"dref")?;
    let mut reader = Reader::new(data_reference.payload);
    reader.full_box()?;
    let entry_count = reader.u32()?;
    if entry_count != 1 {
        return Err(Error::Unsupported(
            "exactly one self-contained data reference is required".to_owned(),
        ));
    }
    let entries = child_boxes(reader.rest())?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid_media("dref entry is missing"))?;
    // The low flag bit of a `url ` entry means "the media is in this file".
    let self_contained =
        entry.name == *b"url " && entry.payload.get(3).is_some_and(|flags| flags & 1 != 0);
    if !self_contained {
        return Err(Error::Unsupported(
            "external data references are not supported".to_owned(),
        ));
    }
    Ok(())
}

/// The track's one sample entry.
fn sample_entry(sample_table: &[u8]) -> Result<RawBox<'_>> {
    let description = required_child(sample_table, *b"stsd")?;
    let mut reader = Reader::new(description.payload);
    reader.full_box()?;
    let entry_count = reader.u32()?;
    if entry_count != 1 {
        return Err(Error::Unsupported(format!(
            "{entry_count} sample descriptions; exactly one is required"
        )));
    }
    child_boxes(reader.rest())?
        .into_iter()
        .next()
        .ok_or_else(|| invalid_media("stsd entry is missing"))
}

fn classify_sample_description(
    track_id: u32,
    handler: [u8; 4],
    sample_table: &[u8],
) -> Result<Disposition> {
    let description = required_child(sample_table, *b"stsd")?;
    let mut count = Reader::new(description.payload);
    count.full_box()?;
    let entry_count = count.u32()?;
    if entry_count != 1 {
        return Err(Error::Unsupported(format!(
            "track {track_id}: {entry_count} sample descriptions; exactly one is required"
        )));
    }
    let entry = sample_entry(sample_table)?;
    match &entry.name {
        b"avc1" | b"hvc1" | b"hev1" | b"vp09" | b"av01" | b"mp4a" | b"ac-3" | b"ec-3" | b"Opus"
        | b"fLaC" => Ok(Disposition::Package),
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

/// The number of samples the track's size table declares, in either `stsz` or `stz2`.
fn sample_count(sample_table: &[u8]) -> Result<u32> {
    if let Some(sizes) = optional_child(sample_table, *b"stsz")? {
        let mut reader = Reader::new(sizes.payload);
        reader.full_box()?;
        reader.skip(4)?;
        return reader.u32();
    }
    let sizes = required_child(sample_table, *b"stz2")?;
    let mut reader = Reader::new(sizes.payload);
    reader.full_box()?;
    reader.skip(4)?;
    reader.u32()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;
    use crate::media::CodecConfig;
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
        // MP3 in MP4 hides in an `mp4a` entry, so it is the audio object type that gives it away.
        let error = parse_error("h264-mp3.mp4");

        assert!(matches!(error, Error::Unsupported(_)), "{error}");
        assert!(error.to_string().contains("track 2"), "{error}");
        assert!(error.to_string().contains("0x6b"), "{error}");
    }

    #[test]
    fn reads_the_codec_of_every_supported_format_from_real_files() {
        // File, video codec string, audio codec string, and the audio format.
        for (name, video, audio, format) in [
            ("hevc-aac.mp4", "hvc1.1.6.L60.90", "mp4a.40.2", (48_000, 1)),
            ("vp9-opus.mp4", "vp09.00.11.08", "opus", (48_000, 1)),
            ("av1-aac.mp4", "av01.0.00M.08", "mp4a.40.2", (48_000, 1)),
            ("h264-ac3.mp4", "avc1.64000d", "ac-3", (48_000, 1)),
            ("h264-eac3.mp4", "avc1.64000d", "ec-3", (48_000, 1)),
            ("h264-flac.mp4", "avc1.64000d", "fLaC", (48_000, 1)),
        ] {
            let index = parse_fixture(name);

            assert_eq!(index.tracks[0].codec.codecs(), video, "{name} video");
            assert_eq!(
                index.tracks[0].codec.dimensions(),
                Some((320, 180)),
                "{name}"
            );
            assert_eq!(index.tracks[1].codec.codecs(), audio, "{name} audio");
            assert_eq!(index.tracks[1].codec.audio_format(), Some(format), "{name}");
        }
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

    /// Checks every sample of every track against an independent MP4 implementation: the same
    /// timing and sync flags, and the same payload bytes, which proves each offset and size.
    #[test]
    fn agrees_with_an_independent_implementation_on_every_sample() {
        for name in [
            "h264-aac.mp4",
            "h264-aac-moov-last.mp4",
            "h264-video-only.mp4",
            "h264-aac-44100-stereo.mp4",
            "h264-variable-timing.mp4",
            "h264-aac-anamorphic.mp4",
            "h264-aac-two-audio.mp4",
        ] {
            let index = parse_fixture(name);
            let raw = std::fs::read(fixture(name)).expect("fixture should be readable");
            let mut reference = ::mp4::Mp4Reader::read_header(
                std::io::BufReader::new(std::io::Cursor::new(raw.clone())),
                raw.len() as u64,
            )
            .expect("the reference should parse the fixture");

            for track in &index.tracks {
                assert_eq!(
                    reference.sample_count(track.id).unwrap() as usize,
                    track.samples.len(),
                    "{name} track {}: sample count",
                    track.id
                );
                for (position, sample) in track.samples.iter().enumerate() {
                    let expected = reference
                        .read_sample(track.id, u32::try_from(position + 1).unwrap())
                        .unwrap()
                        .expect("the reference should have the sample");
                    let at = format!("{name} track {} sample {position}", track.id);
                    assert_eq!(expected.start_time, sample.decode_time, "{at}: decode time");
                    assert_eq!(expected.duration, sample.duration, "{at}: duration");
                    assert_eq!(
                        expected.rendering_offset, sample.composition_offset,
                        "{at}: composition offset"
                    );
                    assert_eq!(expected.is_sync, sample.is_sync, "{at}: sync flag");
                    let start = usize::try_from(sample.offset).unwrap();
                    let end = start + usize::try_from(sample.size).unwrap();
                    assert_eq!(&expected.bytes[..], &raw[start..end], "{at}: payload bytes");
                }
            }
        }
    }

    /// A deterministic stand-in for a fuzzer: corrupts a few random bytes of each fixture's
    /// `moov` many times and runs the whole pipeline over the result. Any outcome is fine except
    /// a panic, a hang, or a runaway allocation, which would abort the test process.
    #[test]
    fn corrupted_metadata_never_panics_the_pipeline() {
        let limits = LimitsConfig::default();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            // xorshift64: small, deterministic, and good enough to pick byte positions.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for name in [
            "h264-aac.mp4",
            "h264-aac-default-edits.mp4",
            "h264-aac-audio-delay.mp4",
            "h264-aac-two-audio.mp4",
            "h264-aac-timecode.mp4",
            "h264-aac-quicktime.mov",
            "h264-aac-anamorphic.mp4",
            "hevc-aac.mp4",
            "vp9-opus.mp4",
            "av1-aac.mp4",
            "h264-ac3.mp4",
            "h264-eac3.mp4",
            "h264-flac.mp4",
            "aac-only.m4a",
            "aac-two-tracks-only.m4a",
        ] {
            let source = open_kind(fixture(name)).expect("fixture should open");
            let original = block_on(Metadata::fetch(&source, u64::MAX)).expect("fixture has moov");
            let identity = source.identity().clone();
            let len = original.len();
            for _ in 0..400 {
                let mut moov = original.moov_bytes().to_vec();
                for _ in 0..=(next() % 4) {
                    let position = usize::try_from(next()).unwrap() % moov.len();
                    // Half the time set a byte to an extreme, which stresses counts and sizes.
                    moov[position] = match next() % 4 {
                        0 => 0xff,
                        1 => 0x00,
                        _ => u8::try_from(next() & 0xff).unwrap(),
                    };
                }
                let metadata = Metadata::from_moov(len, moov);
                match parse_metadata(&metadata, identity.clone(), &limits) {
                    Ok(index) => {
                        accepted += 1;
                        if let Ok(plan) = crate::segment::plan(&index, 1000, &limits) {
                            for track in &index.tracks {
                                let _ = crate::fmp4::write_init_segment(&metadata, track.id);
                                for segment in plan.segments.iter().take(2) {
                                    if let Some(part) = segment
                                        .tracks
                                        .iter()
                                        .find(|candidate| candidate.track_id == track.id)
                                    {
                                        let _ = crate::fmp4::prepare_media_segment(
                                            track, *part, 1, &limits,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => rejected += 1,
                }
            }
        }
        // Both outcomes must occur, or the test is not exercising what it claims to.
        assert!(accepted > 0, "no mutation survived parsing");
        assert!(rejected > 0, "no mutation was rejected");
    }

    #[test]
    fn parses_a_quicktime_file_with_versioned_sound_entries() {
        let index = parse_fixture("h264-aac-quicktime.mov");

        assert_eq!(index.tracks.len(), 2);
        assert!(matches!(
            index.tracks[1].codec,
            CodecConfig::Aac {
                sample_rate: 48_000,
                channels: 1,
                object_type: 2
            }
        ));
        assert_eq!(index.tracks[1].samples.len(), 142);
    }

    #[test]
    fn reads_the_avc_configuration_and_dimensions() {
        let index = parse_fixture("h264-aac.mp4");

        let CodecConfig::Avc {
            width,
            height,
            profile,
            level,
            ref sequence_parameter_set,
            ref picture_parameter_set,
            ..
        } = index.tracks[0].codec
        else {
            panic!("the first track is H.264");
        };
        assert_eq!((width, height), (320, 180));
        assert_eq!((profile, level), (100, 13));
        assert_eq!(sequence_parameter_set[0] & 0x1f, 7, "a real SPS NAL unit");
        assert_eq!(picture_parameter_set[0] & 0x1f, 8, "a real PPS NAL unit");
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
                object_type: 2,
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
        block_on(Metadata::fetch(&source, u64::MAX))
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
